//! Shared Win32 helpers: process identity, user-only security descriptors, random
//! bytes, DPAPI, client elevation. All unsafe is confined here and in host.rs (plus one `unsafe impl Send` in broker.rs).
#![cfg(windows)]
// Handles here are opaque kernel identifiers passed to Win32, never dereferenced as Rust pointers, so taking them in safe functions is sound.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
use std::{ffi::c_void, mem::zeroed, path::Path, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Cryptography::*, *},
    Storage::FileSystem::*,
    System::{Pipes::GetNamedPipeClientProcessId, Threading::*},
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn valid(h: HANDLE) -> bool {
    !h.is_null() && h != INVALID_HANDLE_VALUE
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
/// `size_of::<T>()` as the `u32` Win32 structures want (these are small, fixed sizes).
pub fn size32<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).unwrap_or(u32::MAX)
}
/// A buffer length as the `u32` Win32 calls take: lengths above `u32::MAX` are cut to it (callers loop).
pub fn len32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Fill `buf` from the system's cryptographic generator. A failure is not survivable for the callers (ids, nonces,
/// salts must never fall back to something guessable), so it panics rather than returning weak bytes.
pub fn random_bytes(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(1 << 20) {
        let status = unsafe { BCryptGenRandom(null_mut(), chunk.as_mut_ptr(), len32(chunk.len()), BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        assert!(status >= 0, "BCryptGenRandom failed");
    }
}

pub fn creation_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !valid(h) {
            return None;
        }
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = zeroed();
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        CloseHandle(h);
        (ok != 0).then(|| filetime_u64(c))
    }
}

/// True only if `pid` is running AND started at `ctime` (PID-reuse defence).
pub fn alive(pid: u32, ctime: u64) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if !valid(h) {
            return false;
        }
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = zeroed();
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        CloseHandle(h);
        ok != 0 && filetime_u64(c) == ctime && running
    }
}

pub fn pid_running(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if !valid(h) {
            return false;
        }
        let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        CloseHandle(h);
        running
    }
}

/// A named, auto-reset event that a host sets whenever its terminal produced output. It carries no data (only
/// "something is new"), so even a process that guessed the name could do nothing but cause a spurious wake-up; it is
/// still created with the user-only descriptor. `None` when it cannot be created: the host then runs without it and
/// the broker falls back to asking the host.
pub struct Signal(HANDLE);
// SAFETY: an event HANDLE is an opaque kernel identifier; Win32 allows signalling and waiting from any thread.
unsafe impl Send for Signal {}
unsafe impl Sync for Signal {}
impl Signal {
    pub fn create(name: &str) -> Option<Signal> {
        let sa = user_only_sa(false)?;
        let w = wide(name);
        // SAFETY: `sa` and `w` outlive the call; the returned handle is owned by the Signal.
        let h = unsafe { CreateEventW(sa.attrs(), 0, 0, w.as_ptr()) };
        valid(h).then_some(Signal(h))
    }
    pub fn open(name: &str) -> Option<Signal> {
        let w = wide(name);
        // SAFETY: `w` outlives the call; the returned handle is owned by the Signal.
        let h = unsafe { OpenEventW(SYNCHRONIZE, 0, w.as_ptr()) };
        valid(h).then_some(Signal(h))
    }
    pub fn set(&self) {
        // SAFETY: the handle is valid until this Signal is dropped.
        unsafe { SetEvent(self.0) };
    }
    /// Wait up to `ms` for the event; true when it was set (and is now cleared again).
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: the handle is valid until this Signal is dropped.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}
impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this Signal and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The name of the output event for one terminal (the nonce makes it unguessable and unique to this host).
pub fn output_signal_name(id: &str, nonce: &str) -> String {
    format!(r"Local\wb-out-{id}-{nonce}")
}

pub fn kill_pid(pid: u32) {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
        if valid(h) {
            TerminateProcess(h, 9);
            WaitForSingleObject(h, 5000);
            CloseHandle(h);
        }
    }
}

/// The current user's SID as text, or `None` when the process token cannot be read.
pub fn current_sid_string() -> Option<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let mut buf = vec![0u8; usize::try_from(len).ok()?];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast::<c_void>(), len, &mut len);
        let sid = if ok == 0 {
            None
        } else {
            // The buffer holds a TOKEN_USER that points into itself; read it unaligned, then use the SID it names.
            let user = std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_USER>());
            let mut sid_str: *mut u16 = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                None
            } else {
                let mut n = 0;
                while *sid_str.add(n) != 0 {
                    n += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
                LocalFree(sid_str.cast::<c_void>());
                Some(s)
            }
        };
        CloseHandle(token);
        sid
    }
}

/// SECURITY_ATTRIBUTES granting only the current user full access (protected DACL).
/// `None` when the user's identity or the descriptor cannot be built: callers refuse to create the object rather than
/// create it without the protection.
pub fn user_only_sa(inheritable_dir: bool) -> Option<OwnedSa> {
    let flags = if inheritable_dir { "OICI" } else { "" };
    let sddl = wide(&format!("D:P(A;{flags};GA;;;{})", current_sid_string()?));
    let mut psd: *mut c_void = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut psd, null_mut()) } == 0 {
        return None;
    }
    Some(OwnedSa(SECURITY_ATTRIBUTES { nLength: size32::<SECURITY_ATTRIBUTES>(), lpSecurityDescriptor: psd, bInheritHandle: 0 }))
}

/// A user-only security descriptor that is freed when dropped. (It used to be leaked on purpose, which was harmless
/// for the objects created once at startup but cost the broker about 60 bytes for every connection it accepted: the
/// overnight soak showed it as a steady growth of the broker's memory.) Create the object, then drop this.
pub struct OwnedSa(SECURITY_ATTRIBUTES);
impl OwnedSa {
    pub fn attrs(&self) -> &SECURITY_ATTRIBUTES {
        &self.0
    }
}
impl Drop for OwnedSa {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW (LocalAlloc) and
        // is freed exactly once; the objects created with it keep their own copy.
        unsafe { LocalFree(self.0.lpSecurityDescriptor) };
    }
}

/// Create `dir` (and parents) with a user-only protected DACL when it does not exist.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        ensure_private_dir(parent).ok();
    }
    let Some(sa) = user_only_sa(true) else { return Err(std::io::Error::other("cannot build a user-only security descriptor")) };
    let w = wide(&dir.to_string_lossy());
    let ok = unsafe { CreateDirectoryW(w.as_ptr(), sa.attrs()) };
    if ok == 0 && !dir.exists() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Create a new file that only the current user can read or write, with the same protection the data directory
/// gets. `CREATE_NEW`: an existing file is never reused or truncated (a log always starts a fresh file).
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    let Some(sa) = user_only_sa(false) else { return Err(std::io::Error::other("cannot build a user-only security descriptor")) };
    let w = wide(&path.to_string_lossy());
    // SAFETY: `w` and `sa` outlive the call (the file copies the descriptor); the returned handle is owned by the File.
    let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ, sa.attrs(), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, null_mut()) };
    if !valid(h) {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `h` is a fresh, owned file handle and `from_raw_handle` takes ownership of it exactly once.
    Ok(unsafe { std::fs::File::from_raw_handle(h as std::os::windows::io::RawHandle) })
}

pub fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr().cast_mut() };
        let mut out: CRYPT_INTEGER_BLOB = zeroed();
        if CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, usize::try_from(out.cbData).ok()?).to_vec();
        LocalFree(out.pbData.cast::<c_void>());
        Some(v)
    }
}

pub fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr().cast_mut() };
        let mut out: CRYPT_INTEGER_BLOB = zeroed();
        if CryptUnprotectData(&input, null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, usize::try_from(out.cbData).ok()?).to_vec();
        LocalFree(out.pbData.cast::<c_void>());
        Some(v)
    }
}

/// Is the process on the other end of a connected pipe running elevated?
pub fn pipe_client_elevated(pipe: &PipeIo) -> Option<bool> {
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeClientProcessId(pipe.raw(), &mut pid) == 0 {
            return None;
        }
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !valid(p) {
            return None;
        }
        let mut token: HANDLE = null_mut();
        let ok = OpenProcessToken(p, TOKEN_QUERY, &mut token);
        CloseHandle(p);
        if ok == 0 {
            return None;
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, std::ptr::from_mut(&mut elevation).cast::<c_void>(), size32::<TOKEN_ELEVATION>(), &mut len);
        CloseHandle(token);
        (ok != 0).then_some(elevation.TokenIsElevated != 0)
    }
}

/// Load the per-install secret from `dir/secret.bin` (DPAPI-protected). With
/// `create`, generate it atomically if missing.
pub fn load_secret(dir: &Path, create: bool) -> std::io::Result<Vec<u8>> {
    let path = dir.join("secret.bin");
    if !path.exists() {
        if !create {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no secret: the daemon has not been set up"));
        }
        ensure_private_dir(dir)?;
        let mut secret = [0u8; crate::auth::SECRET_BYTES];
        random_bytes(&mut secret);
        let blob = dpapi_protect(&secret).ok_or_else(|| std::io::Error::other("DPAPI protect failed"))?;
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return load_secret(dir, false),
            Err(e) => return Err(e),
        };
        std::io::Write::write_all(&mut f, &blob)?;
        return Ok(secret.to_vec());
    }
    let blob = std::fs::read(&path)?;
    dpapi_unprotect(&blob).filter(|s| s.len() == crate::auth::SECRET_BYTES).ok_or_else(|| std::io::Error::other("secret unreadable"))
}

/// Blocking `Read`/`Write` over a pipe handle. It does not own the handle (it is not closed on drop): whoever
/// created the handle closes it, and must not do so while a call through this wrapper is in progress.
/// This is the safe face of a raw pipe handle: the one `unsafe` step is `from_raw`, where the caller vouches
/// for the handle; reading and writing through the wrapper cannot violate memory safety (an invalid or closed
/// handle just makes the Win32 call fail, which reads as end of stream or an error).
pub struct PipeIo(HANDLE);
// SAFETY: a pipe HANDLE is an opaque kernel identifier; Win32 allows using it from any thread.
unsafe impl Send for PipeIo {}
impl PipeIo {
    /// # Safety
    /// `h` must be a pipe handle that stays open for as long as this value is used.
    pub unsafe fn from_raw(h: HANDLE) -> PipeIo {
        PipeIo(h)
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}
impl std::io::Read for PipeIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0u32;
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(self.0, buf.as_mut_ptr(), len32(buf.len()), &mut n, null_mut()) };
        if ok == 0 {
            // A broken or closed pipe reads as end of stream.
            return Ok(0);
        }
        Ok(usize::try_from(n).unwrap_or(buf.len()).min(buf.len()))
    }
}
impl std::io::Write for PipeIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut n = 0u32;
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::WriteFile(self.0, buf.as_ptr(), len32(buf.len()), &mut n, null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(usize::try_from(n).unwrap_or(buf.len()).min(buf.len()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Short per-user suffix so the default pipe name is stable per user.
pub fn user_tag() -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(current_sid_string().unwrap_or_default().as_bytes());
    crate::auth::hex(d.get(..4).unwrap_or(&[]))
}

/// Whether this process belongs to the development channel (`AW_CHANNEL=dev`, set by a debug build of the app before it
/// starts anything, so the service it starts, the terminals' shells and `wb` run inside them all inherit it). The
/// development channel has its own terminal service: its own data folder and pipe, so a debug build never shares
/// terminals with the normal build (two apps showing and resizing the same terminals was the worry).
pub fn dev_channel() -> bool {
    std::env::var_os("AW_CHANNEL").is_some_and(|v| v == "dev")
}
fn data_dir_for(dev: bool) -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("AgentWorkbench").join(if dev { "wb-dev" } else { "wb" })
}
fn pipe_name_for(dev: bool, tag: &str) -> String {
    if dev {
        format!(r"\\.\pipe\wb-broker-dev-{tag}")
    } else {
        format!(r"\\.\pipe\wb-broker-{tag}")
    }
}
pub fn default_data_dir() -> std::path::PathBuf {
    data_dir_for(dev_channel())
}
pub fn default_pipe_name() -> String {
    pipe_name_for(dev_channel(), &user_tag())
}

// ---- handles, pipes, processes and the console: the rest of the crate calls these and holds no unsafe code ---------------
// Every function here wraps one Win32 call (or a short fixed sequence of them) with its own SAFETY argument, so a reviewer
// reads this file and `host.rs` (the ConPTY host) and nothing else (docs/design/hardening-plan.md, step 5).

/// A kernel handle that may move between threads: it is an opaque identifier, never dereferenced by Rust.
#[derive(Clone, Copy)]
pub struct SendHandle(pub HANDLE);
// SAFETY: a HANDLE is an opaque kernel identifier; Win32 allows using it from any thread.
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

pub fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error value.
    unsafe { GetLastError() }
}

/// Close a handle this caller owns. Closing an invalid handle only fails; it cannot corrupt memory.
pub fn close_handle(h: HANDLE) {
    // SAFETY: CloseHandle on a value this caller owns; a bad value just returns an error.
    unsafe { CloseHandle(h) };
}

/// Cancel the I/O another thread has in flight on a handle (the deadline watchdogs use this to unblock a hung call).
pub fn cancel_io(raw: usize) {
    // SAFETY: CancelIoEx with a null overlapped cancels this process's pending I/O on the handle; an invalid handle only fails.
    unsafe { windows_sys::Win32::System::IO::CancelIoEx(raw as HANDLE, std::ptr::null()) };
}

fn open_pipe_once(name: &[u16]) -> HANDLE {
    // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
    unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, null_mut()) }
}

/// Open a named pipe, retrying every 50 ms until `timeout`.
pub fn connect_pipe(name: &str, timeout: std::time::Duration) -> Option<HANDLE> {
    let w = wide(name);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let h = open_pipe_once(&w);
        if valid(h) {
            return Some(h);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Open a named pipe as a client: waits for a busy pipe, gives up at `timeout` with the Windows error.
pub fn open_pipe(name: &str, timeout: std::time::Duration) -> std::io::Result<HANDLE> {
    let w = wide(name);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let h = open_pipe_once(&w);
        if valid(h) {
            return Ok(h);
        }
        let e = last_error();
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::from_raw_os_error(i32::try_from(e).unwrap_or(i32::MAX)));
        }
        if e == ERROR_PIPE_BUSY {
            // SAFETY: `w` is a NUL-terminated wide string that outlives the call.
            unsafe { windows_sys::Win32::System::Pipes::WaitNamedPipeW(w.as_ptr(), 500) };
        } else {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// A pipe stream over a handle the caller owns. CONTRACT (no longer checked by the compiler): the caller keeps the handle open
/// for as long as the stream is used and never closes it while another thread still reads or writes it; a handle closed early
/// can be reused by Windows for another object. Every call site holds the handle in an owner whose `Drop` closes it
/// (`HostConn`, `PipeCloser`, `Client`) and builds the stream from that owner.
pub fn pipe_io(h: HANDLE) -> PipeIo {
    // SAFETY: see the doc comment: the worst a bad handle does is fail I/O.
    unsafe { PipeIo::from_raw(h) }
}

/// One more instance of the broker's named pipe, protected for the current user only and local clients only.
pub fn create_pipe_instance(name: &str, first: bool) -> Result<HANDLE, u32> {
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    // Never create the pipe without its user-only protection.
    let Some(sa) = user_only_sa(false) else { return Err(ERROR_NO_TOKEN) };
    let w = wide(name);
    // SAFETY: `w` and `sa` outlive the call; the pipe copies the descriptor.
    let h = unsafe {
        CreateNamedPipeW(
            w.as_ptr(),
            PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            sa.attrs(),
        )
    };
    if valid(h) {
        Ok(h)
    } else {
        Err(last_error())
    }
}

/// Wait for a client on a pipe instance (blocks). True when one is connected.
pub fn accept_pipe(h: HANDLE) -> bool {
    // SAFETY: ConnectNamedPipe on an instance this caller created; a null overlapped makes it block.
    unsafe { windows_sys::Win32::System::Pipes::ConnectNamedPipe(h, null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED }
}

/// Let the client read what was sent, then drop the client from the instance.
pub fn flush_and_disconnect_pipe(h: HANDLE) {
    // SAFETY: FlushFileBuffers and DisconnectNamedPipe on a pipe instance this caller owns.
    unsafe {
        FlushFileBuffers(h);
        windows_sys::Win32::System::Pipes::DisconnectNamedPipe(h);
    }
}

pub fn disconnect_pipe(h: HANDLE) {
    // SAFETY: DisconnectNamedPipe on a pipe instance this caller owns.
    unsafe { windows_sys::Win32::System::Pipes::DisconnectNamedPipe(h) };
}

/// Start a host process (`cmdline` is the whole command line): detached from this process, with no inherited handles, outside
/// this process's Job when Windows allows it. Returns the pid and the process's creation time. A parent Job that forbids
/// breakaway would make the host die with the broker, so that case is refused.
pub fn spawn_host_process(cmdline: &str) -> Result<(u32, u64), String> {
    use windows_sys::Win32::System::Threading::{CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW};
    let mut line = wide(cmdline);
    // SAFETY: zeroed STARTUPINFOW/PROCESS_INFORMATION are valid initial values; `line` is a writable NUL-terminated wide string
    // that outlives each call; bInheritHandles is FALSE, so the host starts with an empty handle table.
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size32::<STARTUPINFOW>();
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        let mut last = 0;
        for (flags, breakaway) in [(base | CREATE_BREAKAWAY_FROM_JOB, true), (base, false)] {
            let mut pi: PROCESS_INFORMATION = zeroed();
            if CreateProcessW(std::ptr::null(), line.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, flags, std::ptr::null(), std::ptr::null(), &si, &mut pi) != 0 {
                let ct = creation_time(pi.dwProcessId).ok_or("host vanished")?;
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
                if !breakaway {
                    // A parent Job forbidding breakaway means this host dies with the broker's Job: refuse a Persistent session.
                    kill_pid(pi.dwProcessId);
                    return Err("persistent sessions unavailable: the broker runs inside a Job that forbids breakaway".into());
                }
                return Ok((pi.dwProcessId, ct));
            }
            last = GetLastError();
            if !(breakaway && last == ERROR_ACCESS_DENIED) {
                break;
            }
        }
        Err(format!("host launch failed ({last})"))
    }
}

// ---- the console (wb attach) ----------------------------------------------------------------------------------------------

/// The process's standard input and output handles.
pub fn std_handles() -> (HANDLE, HANDLE) {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    // SAFETY: GetStdHandle only reads the process's standard handle slots.
    unsafe { (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE)) }
}

/// The visible size of the console window (columns, rows), or `None` when `out` is not a console.
pub fn console_size(out: HANDLE) -> Option<(u16, u16)> {
    use windows_sys::Win32::System::Console::{GetConsoleScreenBufferInfo, CONSOLE_SCREEN_BUFFER_INFO};
    // SAFETY: a zeroed CONSOLE_SCREEN_BUFFER_INFO is a valid out-parameter; a non-console handle just fails.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(out, &mut info) } == 0 {
        return None;
    }
    let cols = u16::try_from((info.srWindow.Right - info.srWindow.Left + 1).clamp(20, 400)).unwrap_or(120);
    let rows = u16::try_from((info.srWindow.Bottom - info.srWindow.Top + 1).clamp(5, 200)).unwrap_or(30);
    Some((cols, rows))
}

/// Write all of `data` to a handle (a console or a pipe). False when the write fails.
pub fn write_all_to(out: HANDLE, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let mut n = 0u32;
        // SAFETY: `data` is valid for `len32(data.len())` bytes and `n` is a valid out-parameter.
        if unsafe { WriteFile(out, data.as_ptr(), len32(data.len()), &mut n, null_mut()) } == 0 || n == 0 {
            return false;
        }
        data = data.get(usize::try_from(n).unwrap_or(usize::MAX)..).unwrap_or(&[]);
    }
    true
}

/// Read up to `buf.len()` bytes; `None` at the end of input or when the handle goes away.
pub fn read_some(input: HANDLE, buf: &mut [u8]) -> Option<usize> {
    let mut n = 0u32;
    // SAFETY: `buf` is valid for `len32(buf.len())` bytes and `n` is a valid out-parameter.
    let ok = unsafe { ReadFile(input, buf.as_mut_ptr(), len32(buf.len()), &mut n, null_mut()) };
    if ok == 0 || n == 0 {
        None
    } else {
        Some(usize::try_from(n).unwrap_or(0))
    }
}

/// Console modes saved when a console is attached and restored on every way out (including a panic).
pub struct ConsoleGuard {
    input: HANDLE,
    output: HANDLE,
    input_mode: Option<u32>,
    output_mode: Option<u32>,
    output_cp: Option<u32>,
}
// SAFETY: console HANDLEs are opaque kernel identifiers usable from any thread.
unsafe impl Send for ConsoleGuard {}
impl ConsoleGuard {
    /// Raw keys as VT sequences (no line editing, echo or Ctrl+C handling here: they belong to the terminal's program), VT output,
    /// UTF-8 code pages. A handle that is not a console is left alone.
    pub fn enter(input: HANDLE, output: HANDLE) -> ConsoleGuard {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetConsoleOutputCP, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        };
        let mut g = ConsoleGuard { input, output, input_mode: None, output_mode: None, output_cp: None };
        // SAFETY: console mode calls on handles from GetStdHandle; each only fails for a non-console handle.
        unsafe {
            let mut mode = 0u32;
            if GetConsoleMode(input, &mut mode) != 0 {
                g.input_mode = Some(mode);
                SetConsoleMode(input, (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT);
            }
            if GetConsoleMode(output, &mut mode) != 0 {
                g.output_mode = Some(mode);
                SetConsoleMode(output, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                g.output_cp = Some(GetConsoleOutputCP());
                SetConsoleOutputCP(65001);
                SetConsoleCP(65001);
            }
        }
        g
    }
}
impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{SetConsoleMode, SetConsoleOutputCP};
        // SAFETY: restores the modes saved by `enter`.
        unsafe {
            if let Some(m) = self.input_mode {
                SetConsoleMode(self.input, m);
            }
            if let Some(m) = self.output_mode {
                SetConsoleMode(self.output, m);
            }
            if let Some(cp) = self.output_cp {
                SetConsoleOutputCP(cp);
            }
        }
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    #[test]
    fn the_development_channel_has_its_own_folder_and_pipe() {
        assert_ne!(data_dir_for(true), data_dir_for(false));
        assert!(data_dir_for(true).ends_with("wb-dev"));
        assert!(data_dir_for(false).ends_with("wb"));
        assert_ne!(pipe_name_for(true, "ab12"), pipe_name_for(false, "ab12"));
        assert_eq!(pipe_name_for(false, "ab12"), r"\\.\pipe\wb-broker-ab12", "the normal name is unchanged (running services use it)");
        assert!(pipe_name_for(true, "ab12").contains("dev"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_pipe_name_is_a_valid_local_pipe_path_and_stable() {
        let name = default_pipe_name();
        assert!(name.starts_with(r"\\.\pipe\wb-broker-"), "{name}");
        assert_eq!(name, default_pipe_name());
        assert_eq!(name.len(), r"\\.\pipe\wb-broker-".len() + 8);
        assert!(default_data_dir().ends_with(r"AgentWorkbench\wb"));
    }
}

/// Is this process running elevated? Unknowable counts as elevated (fail closed).
pub fn self_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return true;
        }
        let mut e: TOKEN_ELEVATION = zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, std::ptr::from_mut(&mut e).cast::<c_void>(), size32::<TOKEN_ELEVATION>(), &mut len);
        CloseHandle(token);
        ok == 0 || e.TokenIsElevated != 0
    }
}
