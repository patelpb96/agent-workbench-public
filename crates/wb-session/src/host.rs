//! Per-session host process: owns one ConPTY shell, a Job with KILL_ON_JOB_CLOSE (the
//! host is the unit of survival), an always-drained output ring with absolute stream
//! offsets, and one named pipe accepting a single, epoch-fenced broker connection.
//! Terminals only: no store, no network. Evidence for each Win32 choice is in
//! docs/design/spike-session-host-results.md.
//!
//! Hardening rules for this file (docs/design/hardening-plan.md): a host never panics on anything a peer can
//! send (every frame is length-checked by `frames`, every body is parsed with a bounds-checked parser, an
//! oversized or truncated frame drops the connection and nothing else), no lock is held across blocking I/O,
//! and the output history is a bounded `Ring`. The one place output leaves the host's memory is the opt-in log
//! (`log` module): it is written by the reader thread, under the logging lock, because it must see every byte once.
#![cfg(windows)]
use crate::{
    frames::{self, FrameError, MAX_REPLY_FRAME, MAX_REQUEST_FRAME},
    log::{self, LogWriter},
    ring::Ring,
    sync::LockExt,
    win::*,
};
use std::{
    ffi::c_void,
    io::Write,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Console::*, JobObjects::*, Pipes::*, Threading::*, IO::CancelIoEx},
};

pub use crate::frames::READ_CHUNK;
pub const RING_CAP: usize = 1 << 20;
/// Extended history: a larger in-memory ring (never written to disk).
pub const EXTENDED_RING_CAP: usize = 32 << 20;
/// The ring may overshoot its cap by this much before trimming, so trimming is amortized
/// instead of moving the whole buffer on every read.
const RING_SLACK: usize = 64 << 10;

pub const OP_HELLO: u8 = 1;
pub const OP_READ: u8 = 2;
pub const OP_INPUT: u8 = 3;
pub const OP_INFO: u8 = 4;
pub const OP_END: u8 = 5;
pub const OP_RESIZE: u8 = 6;
/// Start recording output to the file named by the body (UTF-8, chosen by the broker, never by a client).
pub const OP_LOG_START: u8 = 7;
/// Stop recording (a no-op when nothing is being recorded).
pub const OP_LOG_STOP: u8 = 8;
/// Report the logging state without changing it.
pub const OP_LOG_STATUS: u8 = 9;

/// The logging state, as a host reports it: `LOG on=<0|1> bytes=<n> reason=<code|-> path=<hex UTF-8|->`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogReply {
    pub on: bool,
    pub bytes: u64,
    /// Why it stopped, when it stopped by itself (`cap`, `write_error`) or was stopped (`stopped`).
    pub reason: Option<crate::log::StopReason>,
    /// The file being written (still known after a broker restart, which is how a re-adopted host's log is found again).
    pub path: Option<String>,
}

fn log_reply(on: bool, bytes: u64, reason: Option<crate::log::StopReason>, path: Option<&str>) -> Vec<u8> {
    format!(
        "LOG on={} bytes={bytes} reason={} path={}",
        u8::from(on),
        reason.map_or("-", crate::log::StopReason::code),
        path.map_or_else(|| "-".to_owned(), |p| crate::auth::hex(p.as_bytes()))
    )
    .into_bytes()
}

/// What this host says about its log (`None`: it never started one).
fn slot_reply(l: Option<&LogOn>) -> Vec<u8> {
    match l {
        Some(l) => log_reply(l.writer.on(), l.writer.bytes(), l.writer.stop_reason(), Some(&l.path)),
        None => log_reply(false, 0, None, None),
    }
}

/// Parse a LOG_* reply. `None` for anything that is not one (a short, foreign or corrupted reply is never trusted).
pub fn parse_log_reply(reply: &[u8]) -> Option<LogReply> {
    let text = std::str::from_utf8(reply).ok()?;
    let mut tokens = text.split_whitespace();
    if tokens.next()? != "LOG" {
        return None;
    }
    let mut fields = std::collections::HashMap::new();
    for token in tokens {
        let (key, value) = token.split_once('=')?;
        fields.insert(key, value);
    }
    let on = match *fields.get("on")? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    let bytes = fields.get("bytes")?.parse::<u64>().ok()?;
    let reason = match *fields.get("reason")? {
        "-" => None,
        code => Some(crate::log::StopReason::from_code(code)?),
    };
    let path = match *fields.get("path")? {
        "-" => None,
        hex => Some(String::from_utf8(crate::auth::unhex(hex)?).ok()?),
    };
    Some(LogReply { on, bytes, reason, path })
}

pub fn host_pipe_name(id: &str, nonce: &str) -> String {
    format!(r"\\.\pipe\wb-host-{id}-{nonce}")
}

/// Broker side: send one request frame to a host (capped at `MAX_REQUEST_FRAME`).
pub fn write_request(io: &mut PipeIo, payload: &[u8]) -> bool {
    frames::write_frame(io, payload, MAX_REQUEST_FRAME)
}
/// Broker side: read one reply frame from a host (capped at `MAX_REPLY_FRAME`).
pub fn read_reply(io: &mut PipeIo) -> Result<Vec<u8>, FrameError> {
    frames::read_frame(io, MAX_REPLY_FRAME)
}
fn read_request(io: &mut PipeIo) -> Result<Vec<u8>, FrameError> {
    frames::read_frame(io, MAX_REQUEST_FRAME)
}
fn write_reply(io: &mut PipeIo, payload: &[u8]) -> bool {
    frames::write_frame(io, payload, MAX_REPLY_FRAME)
}

fn job_active(job: HANDLE) -> u32 {
    unsafe {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
        let ok = QueryInformationJobObject(job, JobObjectBasicAccountingInformation, std::ptr::from_mut(&mut info).cast::<c_void>(), size32::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(), null_mut());
        if ok == 0 {
            u32::MAX
        } else {
            info.ActiveProcesses
        }
    }
}

#[derive(Clone, Copy)]
struct H(HANDLE);
// SAFETY: a HANDLE is an opaque kernel identifier; Win32 allows using it from any thread.
unsafe impl Send for H {}
unsafe impl Sync for H {}

/// A log open in this host: the file it writes and what it has written.
struct LogOn {
    path: String,
    writer: LogWriter,
}

pub struct HostArgs {
    pub ring_bytes: usize,
    pub id: String,
    pub nonce: String,
    pub cols: i16,
    pub rows: i16,
    pub shell: String,
    /// Folder the shell starts in; `None` means the user's folder.
    pub start_dir: Option<String>,
}

/// Runs until the session ends. Never returns normally.
pub fn run_host(a: HostArgs) -> ! {
    unsafe {
        let Some(sa) = user_only_sa(false) else { std::process::exit(2) };
        let name = wide(&host_pipe_name(&a.id, &a.nonce));
        let pipe = CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            sa.attrs(),
        );
        if !valid(pipe) {
            std::process::exit(2);
        }

        // Job with KILL_ON_JOB_CLOSE, held by this host.
        let job = CreateJobObjectW(null(), null());
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, std::ptr::from_ref(&limits).cast::<c_void>(), size32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) == 0 {
            std::process::exit(3);
        }

        let (mut in_read, mut in_write, mut out_read, mut out_write): (HANDLE, HANDLE, HANDLE, HANDLE) = (null_mut(), null_mut(), null_mut(), null_mut());
        if CreatePipe(&mut in_read, &mut in_write, null(), 0) == 0 || CreatePipe(&mut out_read, &mut out_write, null(), 0) == 0 {
            std::process::exit(4);
        }
        let mut hpc: HPCON = zeroed();
        if CreatePseudoConsole(COORD { X: a.cols, Y: a.rows }, in_read, out_write, 0, &mut hpc) < 0 {
            std::process::exit(5);
        }
        CloseHandle(in_read);
        CloseHandle(out_write);

        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        let mut attr_buf = vec![0u8; size];
        let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_mut_ptr().cast::<c_void>();
        if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0
            || UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, hpc as *const c_void, size_of::<HPCON>(), null_mut(), null()) == 0
        {
            std::process::exit(6);
        }
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size32::<STARTUPINFOEXW>();
        si.lpAttributeList = attrs;
        let mut pi: PROCESS_INFORMATION = zeroed();
        // The shell inherits this host's environment: put the folder holding `wb` first on PATH (so `wb ls`,
        // `wb attach` and agent tools work inside a Workbench terminal) and tell it which terminal it is.
        if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.to_path_buf())) {
            let old = std::env::var_os("PATH").unwrap_or_default();
            let paths = std::iter::once(dir).chain(std::env::split_paths(&old));
            if let Ok(joined) = std::env::join_paths(paths) {
                std::env::set_var("PATH", joined);
            }
        }
        std::env::set_var("WB_TERMINAL", &a.id);
        // Start in the user's folder, not in whatever folder the app or service happened to be started from
        // (that was C:\Windows\System32 when launched from Explorer).
        let requested = a.start_dir.as_deref().filter(|d| crate::proto::valid_start_dir(d) && std::path::Path::new(d).is_dir()).map(std::ffi::OsString::from);
        let start_dir: Option<Vec<u16>> = requested.or_else(|| std::env::var_os("USERPROFILE")).filter(|p| std::path::Path::new(p).is_dir()).map(|p| wide(&p.to_string_lossy()));
        let mut cmd = wide(&a.shell);
        // bInheritHandles = FALSE: the shell inherits nothing from this host.
        if CreateProcessW(null(), cmd.as_mut_ptr(), null(), null(), 0, EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED, null(), start_dir.as_ref().map_or(null(), |d| d.as_ptr()), &si.StartupInfo, &mut pi) == 0
            || AssignProcessToJobObject(job, pi.hProcess) == 0
        {
            std::process::exit(7);
        }
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);
        let shell = H(pi.hProcess);
        let shell_pid = pi.dwProcessId;

        // The reader always drains, so a slow or absent client never blocks the shell. The ring is bounded
        // (`cap` plus slack); a recovered poisoned lock re-establishes that bound before anyone reads.
        let cap = a.ring_bytes.clamp(RING_CAP, EXTENDED_RING_CAP);
        let ring = Arc::new(Mutex::new(Ring::new(cap, (cap / 8).max(RING_SLACK))));
        // Logging state. Only the reader thread below writes output bytes into it, so every byte is recorded exactly
        // once; the control ops only open, close and look at it. The lock is held across the append (a single write to
        // a file this process owns): that is the same cost the drain pays anyway, and the terminal is never affected
        // because a failed or stopped log is dropped from the path, not reported back into the shell.
        let log_slot: Arc<Mutex<Option<LogOn>>> = Arc::new(Mutex::new(None));
        let r2 = ring.clone();
        let l2 = log_slot.clone();
        let out = H(out_read);
        // Tells the broker "there is new output" without it having to ask. Optional: without it the broker asks.
        // (`WB_NO_OUTPUT_SIGNAL` turns it off: the tests use it to stand in for a host that predates the event.)
        let signal = if std::env::var_os("WB_NO_OUTPUT_SIGNAL").is_some() { None } else { Signal::create(&output_signal_name(&a.id, &a.nonce)) };
        thread::spawn(move || {
            let out = out;
            let mut buf = [0u8; 8192];
            loop {
                let mut n = 0u32;
                if ReadFile(out.0, buf.as_mut_ptr(), len32(buf.len()), &mut n, null_mut()) == 0 || n == 0 {
                    break;
                }
                let got = usize::try_from(n).unwrap_or(0).min(buf.len());
                let bytes = buf.get(..got).unwrap_or(&[]);
                r2.locked_repairing(Ring::repair).append(bytes);
                if let Some(l) = l2.locked().as_mut() {
                    l.writer.append(bytes);
                }
                if let Some(signal) = &signal {
                    signal.set();
                }
            }
            // No more output can come: a log still open is complete, and says why it ended.
            if let Some(l) = l2.locked().as_mut() {
                l.writer.end();
            }
        });

        let jobh = H(job);
        let mut input = PipeIo::from_raw(in_write);
        let mut conn = PipeIo::from_raw(pipe);
        let mut last_epoch = 0u64;
        let host_pid = GetCurrentProcessId();
        let nonce = a.nonce.clone();
        loop {
            let connected = ConnectNamedPipe(pipe, null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED;
            if !connected {
                // Do not spin if the pipe is in a persistent error state.
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let mut authed = false;
            // A connected client that never completes the handshake is cut off, so it cannot
            // hold the single pipe instance (the legitimate broker would see ERROR_PIPE_BUSY).
            let flag = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicBool::new(false));
            // The timer is joined when the connection ends (below), so it can never outlive its connection and
            // cancel the I/O of the NEXT one on this same pipe handle.
            let watcher = {
                let (flag, done, raw) = (flag.clone(), done.clone(), pipe as usize);
                thread::spawn(move || {
                    let end = Instant::now() + Duration::from_secs(5);
                    while !done.load(Ordering::SeqCst) && !flag.load(Ordering::SeqCst) {
                        let now = Instant::now();
                        if now >= end {
                            CancelIoEx(raw as HANDLE, null());
                            return;
                        }
                        thread::park_timeout(end - now);
                    }
                })
            };
            loop {
                let frame = match read_request(&mut conn) {
                    Ok(frame) => frame,
                    Err(FrameError::TooLarge { .. }) => {
                        // A corrupt or hostile length: nothing was allocated, the stream cannot be re-aligned.
                        write_reply(&mut conn, b"ERR frame_too_large");
                        FlushFileBuffers(pipe);
                        break;
                    }
                    // Closed between frames, or the peer died mid-frame: either way this connection is over.
                    Err(FrameError::Closed | FrameError::Truncated) => break,
                };
                let Some((op, body)) = frame.split_first() else {
                    write_reply(&mut conn, b"ERR empty_frame");
                    FlushFileBuffers(pipe); // let the client read the refusal before the pipe is disconnected
                    break;
                };
                let reply: Vec<u8> = match *op {
                    OP_HELLO => match frames::parse_hello(body) {
                        Some((epoch, their_nonce)) if their_nonce == nonce.as_bytes() && epoch > last_epoch => {
                            last_epoch = epoch;
                            authed = true;
                            flag.store(true, Ordering::SeqCst);
                            b"OK".to_vec()
                        }
                        _ => {
                            // Failed handshake (stale epoch, wrong nonce or a malformed hello): answer once, then drop the connection.
                            write_reply(&mut conn, b"ERR stale_epoch_or_nonce");
                            // DisconnectNamedPipe discards unread data: let the client read the
                            // refusal first. The handshake watchdog cancels this if it never does.
                            FlushFileBuffers(pipe);
                            break;
                        }
                    },
                    _ if !authed => {
                        write_reply(&mut conn, b"ERR unauthenticated");
                        FlushFileBuffers(pipe);
                        break;
                    }
                    OP_READ => match frames::parse_read_request(body) {
                        Some(from) => {
                            // Copy out under the lock (memory only), release it, then answer: no lock is held while writing.
                            let (next, skipped, data) = {
                                let ring = ring.locked_repairing(Ring::repair);
                                let (next, skipped, data) = ring.read_from(from, READ_CHUNK);
                                (next, skipped, data.to_vec())
                            };
                            let mut v = Vec::with_capacity(16 + data.len());
                            v.extend_from_slice(&next.to_le_bytes());
                            v.extend_from_slice(&skipped.to_le_bytes());
                            v.extend_from_slice(&data);
                            v
                        }
                        None => b"ERR malformed".to_vec(),
                    },
                    OP_INPUT => {
                        // `write_all` retries short writes; the body is at most MAX_REQUEST_FRAME.
                        if input.write_all(body).is_ok() {
                            b"OK".to_vec()
                        } else {
                            b"ERR write".to_vec()
                        }
                    }
                    OP_RESIZE => match frames::parse_resize(body) {
                        Some((cols, rows)) if cols > 0 && rows > 0 && ResizePseudoConsole(hpc, COORD { X: cols, Y: rows }) >= 0 => b"OK".to_vec(),
                        Some(_) => b"ERR resize".to_vec(),
                        None => b"ERR malformed".to_vec(),
                    },
                    OP_INFO => {
                        let total = ring.locked_repairing(Ring::repair).total();
                        let exited = WaitForSingleObject(shell.0, 0) == WAIT_OBJECT_0;
                        format!("host_pid={host_pid} shell_pid={shell_pid} job_active={} exited={} total={total} epoch={last_epoch}", job_active(jobh.0), u8::from(exited)).into_bytes()
                    }
                    // The shell exiting does not close the output pipe (the pseudoconsole stays open so the scrollback
                    // stays readable), so a log is ended here once the shell and everything it started are gone.
                    OP_LOG_START | OP_LOG_STOP | OP_LOG_STATUS if WaitForSingleObject(shell.0, 0) == WAIT_OBJECT_0 && job_active(jobh.0) == 0 => {
                        let mut slot = log_slot.locked();
                        if let Some(l) = slot.as_mut() {
                            l.writer.end();
                        }
                        if *op == OP_LOG_START { b"ERR terminal_ended".to_vec() } else { slot_reply(slot.as_ref()) }
                    }
                    OP_LOG_START => match std::str::from_utf8(body).ok().filter(|p| !p.is_empty() && !p.contains('\0')) {
                        Some(path) => {
                            let mut slot = log_slot.locked();
                            // Starting twice is a no-op: the file already being written stays the one, so neither a
                            // second client nor a re-sent request can redirect the log.
                            if !slot.as_ref().is_some_and(|l| l.writer.on()) {
                                // A stopped log (cap or write error) stays as it was; the broker is told it is off.
                                if let Ok(file) = create_private_file(std::path::Path::new(path)) {
                                    *slot = Some(LogOn { path: path.to_owned(), writer: LogWriter::new(file, log::LOG_CAP_BYTES) });
                                }
                            }
                            if slot.as_ref().is_some_and(|l| l.writer.on()) {
                                slot_reply(slot.as_ref())
                            } else {
                                b"ERR log_unavailable".to_vec()
                            }
                        }
                        None => b"ERR malformed".to_vec(),
                    },
                    OP_LOG_STOP => {
                        let mut slot = log_slot.locked();
                        if let Some(l) = slot.as_mut() {
                            l.writer.stop();
                        }
                        slot_reply(slot.as_ref())
                    }
                    OP_LOG_STATUS => slot_reply(log_slot.locked().as_ref()),
                    OP_END => {
                        TerminateJobObject(jobh.0, 1);
                        let root_signaled = WaitForSingleObject(shell.0, 5000) == WAIT_OBJECT_0;
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while job_active(jobh.0) != 0 && Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(20));
                        }
                        let active = job_active(jobh.0);
                        write_reply(&mut conn, format!("ENDED active={active} root_signaled={}", u8::from(root_signaled)).as_bytes());
                        FlushFileBuffers(pipe);
                        ClosePseudoConsole(hpc);
                        std::process::exit(if active == 0 && root_signaled { 0 } else { 3 });
                    }
                    _ => b"ERR unknown_op".to_vec(),
                };
                if !write_reply(&mut conn, &reply) {
                    break;
                }
            }
            done.store(true, Ordering::SeqCst);
            watcher.thread().unpark();
            let _ = watcher.join();
            DisconnectNamedPipe(pipe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::StopReason;

    #[test]
    fn the_log_reply_round_trips_and_refuses_anything_else() {
        let parsed = parse_log_reply(&log_reply(true, 4096, None, Some(r"C:\data\logs\ab-20261025T101112Z.log"))).expect("a reply this host wrote parses");
        assert_eq!(parsed, LogReply { on: true, bytes: 4096, reason: None, path: Some(r"C:\data\logs\ab-20261025T101112Z.log".into()) });
        assert_eq!(parse_log_reply(&slot_reply(None)), Some(LogReply { on: false, bytes: 0, reason: None, path: None }));
        for reason in [StopReason::Stopped, StopReason::Cap, StopReason::WriteError, StopReason::Ended] {
            assert_eq!(parse_log_reply(&log_reply(false, 4096, Some(reason), None)).and_then(|r| r.reason), Some(reason));
        }
        for bad in [b"".as_slice(), b"OK", b"LOG on=2 bytes=1 reason=- path=-", b"LOG on=1 bytes=x reason=- path=-", b"LOG on=1 bytes=1 reason=nonsense path=-", b"LOG on=1 bytes=1 path=-", b"LOG on=1 bytes=1 reason=- path=zz"] {
            assert_eq!(parse_log_reply(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }
}
