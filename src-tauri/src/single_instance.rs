//! Single-instance GUI launch gate (`docs/design/startup-login-plan.md`, P1).
//!
//! A second launch must not replay startup, create a second window, or start a second
//! execution coordinator. The authority is a named kernel mutex: the first process creates
//! `Local\agent-workbench.single-instance.<identifier>` and holds the handle for its whole
//! life, so the name is taken while it runs and is freed by the kernel when it exits, even
//! after a crash. No plugin, dependency or config change is involved.
//!
//! A duplicate does not exit immediately *before* it has anything to show for itself. Once it
//! has raised the running window it exits at once; the budget only covers the case where
//! nothing could be raised yet. Closing the app runs a bounded graceful shutdown (up to
//! `SHUTDOWN_BOUND + 1 s`), and "close, then immediately reopen" has to end with one usable
//! window rather than a silent no-op, so the wait inside [`RECLAIM_BUDGET`] exists for that
//! close and never outlives a successful raise.
//!
//! The decisions ([`mutex_name`], [`same_image`], [`choose`], [`startup`]) are plain data and
//! pure functions, so they are testable without a desktop. Only the thin glue below calls
//! Win32; every call is checked and degrades to "carry on as if unguarded" (fail open).
use std::{
    ptr::null,
    time::{Duration, Instant},
};
use windows_sys::{
    core::BOOL,
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM},
        System::Threading::{
            CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
            SetForegroundWindow, ShowWindow, SHOW_WINDOW_CMD, SW_RESTORE, SW_SHOW,
        },
    },
};

/// Scope of the mutex name. `Local\` means "this logon session", so a second Windows user
/// gets their own instance instead of being redirected into someone else's desktop.
const MUTEX_PREFIX: &str = r"Local\agent-workbench.single-instance.";

/// How long a duplicate launch keeps trying while it has raised no window.
///
/// `main` allows a clean close up to `integration::SHUTDOWN_BOUND + 1 s` (9 s). Without this
/// wait, "close, then reopen at once" would find the name still held and exit, leaving no
/// window at all. 12 s covers that shutdown with margin without keeping an invisible process
/// around for long. It is a ceiling on waiting, not a linger time: a window that is found and
/// restored ends the wait immediately, whatever the foreground request did (see [`startup`]).
pub const RECLAIM_BUDGET: Duration = Duration::from_secs(12);

/// Pause between probes: 48 probes fit in the budget, and it is short enough that a takeover
/// feels immediate.
pub const RECLAIM_INTERVAL: Duration = Duration::from_millis(250);

/// Cap on the image-path buffer, in UTF-16 units, far above any real install path. A longer
/// path is simply not treated as ours, which only costs a redirect.
const MAX_IMAGE_UNITS: u32 = 4096;
/// Cap on the window-title buffer, likewise; the title only ranks candidates.
const MAX_TITLE_UNITS: u32 = 512;

/// The live claim on the single-instance slot: the named mutex, reserved by this process.
///
/// The name lives while any handle to the object is open, so this must be kept alive for as
/// long as the process is the one instance. Dropping it (or the process exiting) frees the
/// name for the next launch. The handle is not inheritable, so a surviving child process (a
/// terminal host, for example) cannot keep the name alive after this process dies.
pub struct Claim {
    handle: HANDLE,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `handle` came from `CreateMutexW` and is stored once; this is the only
            // place it is closed, and `Claim` is neither cloned nor copied.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// The kernel object name that marks "a GUI for this app is running".
///
/// The identifier is filtered down to `[A-Za-z0-9._-]`: a separator would open a nested
/// namespace and address a different object, and a NUL would silently end the name at the OS
/// boundary. Nothing else from the identifier can reach the name.
pub fn mutex_name(identifier: &str) -> String {
    let mut name = String::with_capacity(MUTEX_PREFIX.len() + identifier.len());
    name.push_str(MUTEX_PREFIX);
    for character in identifier.chars() {
        let safe = character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_');
        name.push(if safe { character } else { '_' });
    }
    name
}

/// Whether two image paths name the same executable, used to decide that a window belongs to
/// this app. Windows paths are case-insensitive, and a `\\?\` prefix or trailing separator
/// carries no meaning.
///
/// This is string normalisation, not a filesystem query: one file reached through a junction
/// or through its 8.3 name stays "different", and an empty path never matches. Both
/// conservatisms only lose the redirect; they never raise a foreign window.
pub fn same_image(a: &str, b: &str) -> bool {
    let a = normalise_image(a);
    !a.is_empty() && a == normalise_image(b)
}

fn normalise_image(path: &str) -> String {
    let trimmed = path.trim();
    let without_prefix = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    without_prefix
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

/// A top-level window as plain data. `window` is an opaque OS handle that the selection rule
/// never reads; the tests pass a null handle.
pub struct Candidate {
    pub window: HWND,
    pub image: String,
    pub title: String,
    pub visible: bool,
    pub minimized: bool,
}

/// Pick the window a duplicate launch should raise, if there is one.
///
/// Only a visible window of this same image qualifies: raising another application would be
/// worse than doing nothing. A configured-title match then outranks a minimized window, so
/// the main window wins over a helper dialog and a restored window wins over a minimized one.
/// Ties keep enumeration order, so the choice is stable across retries.
pub fn choose<'a>(candidates: &'a [Candidate], image: &str, title: &str) -> Option<&'a Candidate> {
    let mut best: Option<(u8, &Candidate)> = None;
    for candidate in candidates {
        if !candidate.visible || !same_image(&candidate.image, image) {
            continue;
        }
        let score = u8::from(candidate.title == title) * 2 + u8::from(!candidate.minimized);
        let better = match best {
            Some((best_score, _)) => score > best_score,
            None => true,
        };
        if better {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Result of one probe of the named mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// This process created the object: hold the claim and run.
    Claimed,
    /// Another process holds the name.
    Held,
    /// `CreateMutexW` failed, so ownership cannot be known.
    Unavailable,
}

/// What the launch should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Build the app: the slot is ours, or ownership is unknowable and the app runs anyway.
    Run,
    /// Another instance owns the slot and no window of ours has been found and restored yet;
    /// wait, then probe again.
    Retry,
    /// Stop and exit: either the running instance's window was raised, or the budget is spent.
    /// The redirect has already been attempted; this process leaves without building the app.
    Yield,
}

/// The relaunch policy: run when the slot is ours or the mutex is unavailable (fail open),
/// stop as soon as the running instance's window was raised, otherwise retry while another
/// instance holds the name inside [`RECLAIM_BUDGET`], then yield.
///
/// `focused` reports whether the last redirect found a window of ours and restored it; it does
/// not report whether Windows accepted the foreground request, which is best effort. That is
/// decided before the budget on purpose: a raised window is the whole job of a duplicate launch,
/// so there is nothing left to wait for, and the process must not linger invisibly behind a
/// Workbench the user can already see. `Held` with nothing raised is the only state that waits,
/// because the other instance may still be creating its window or may be closing.
pub fn startup(attempt: Attempt, waited: Duration, focused: bool) -> Step {
    match attempt {
        Attempt::Claimed | Attempt::Unavailable => Step::Run,
        Attempt::Held if focused => Step::Yield,
        Attempt::Held if waited < RECLAIM_BUDGET => Step::Retry,
        Attempt::Held => Step::Yield,
    }
}

/// What the launch gate decided.
pub enum Startup {
    /// Render the GUI. Keep the claim alive for as long as this process runs.
    Primary(Claim),
    /// Another instance holds the slot; its window was raised when one was found.
    Duplicate { focused: bool },
    /// The mutex could not be created, so this process runs unguarded (fail open).
    Unclaimed,
}

/// Claim the single-instance slot, raising the running instance's window when there is one.
///
/// `identifier` is the app identifier and `title` the main window's title, both taken from the
/// one Tauri `Context` the app is built with, so the mutex name cannot drift from the app it
/// protects.
pub fn launch(identifier: &str, title: &str) -> Startup {
    let name = mutex_name(identifier);
    let image = current_image();
    let started = Instant::now();
    let mut focused = false;
    loop {
        let (attempt, claim) = probe(&name);
        if let Some(claim) = claim {
            return Startup::Primary(claim);
        }
        match startup(attempt, started.elapsed(), focused) {
            Step::Run => return Startup::Unclaimed,
            Step::Yield => return Startup::Duplicate { focused },
            Step::Retry => {
                // `Retry` means nothing has been raised yet, because the other instance may
                // still be creating its window (or may be closing, in which case no window is
                // left to raise). A window that was found and restored is a raise whatever the
                // foreground request did, and the next `startup` call yields on it; pausing
                // first would keep an invisible duplicate alive behind a window that is
                // already on screen.
                if let Some(image) = image.as_deref() {
                    focused = focus_existing(image, title);
                }
                if !focused {
                    std::thread::sleep(RECLAIM_INTERVAL);
                }
            }
        }
    }
}

/// One probe of the slot, exposed so the kernel behaviour is testable without a desktop.
pub fn try_claim(identifier: &str) -> (Attempt, Option<Claim>) {
    probe(&mutex_name(identifier))
}

/// Create the named mutex once, never leaving a handle behind on refusal.
fn probe(name: &str) -> (Attempt, Option<Claim>) {
    let wide_name = wide(name);
    // SAFETY: `wide_name` is NUL-terminated UTF-16 that outlives the call, and a null security
    // descriptor asks for the default creator/owner protection.
    let handle = unsafe { CreateMutexW(null(), 0, wide_name.as_ptr()) };
    // SAFETY: `GetLastError` has no preconditions, and it is read here before any other call
    // could overwrite the status the creation just reported.
    let status = unsafe { GetLastError() };
    if handle.is_null() {
        return (Attempt::Unavailable, None);
    }
    if status == ERROR_ALREADY_EXISTS {
        // Closing the probe handle immediately is required, not tidiness: the object and its
        // name live while any handle is open, so keeping it would make every later probe
        // report `Held` even after the owner has exited.
        // SAFETY: `handle` is a live handle this process owns, closed exactly once here.
        unsafe { CloseHandle(handle) };
        return (Attempt::Held, None);
    }
    (Attempt::Claimed, Some(Claim { handle }))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// This executable's path, or `None` when it cannot be resolved; without a known image there
/// is no candidate window that could safely be raised.
fn current_image() -> Option<String> {
    std::env::current_exe().ok()?.to_str().map(str::to_owned)
}

/// The `ShowWindow` command that raises a window without changing its size.
///
/// `SW_RESTORE` is only for a minimized window: on a maximized one it un-maximizes it, which made
/// a second launch shrink the running Workbench as if its restore button had been clicked.
/// `SW_SHOW` keeps the current placement, maximized or not.
pub fn show_command(minimized: bool) -> SHOW_WINDOW_CMD {
    if minimized {
        SW_RESTORE
    } else {
        SW_SHOW
    }
}

/// Raise the running instance's window. Returns whether a window of ours was found and
/// restored — **not** whether the foreground request was accepted.
///
/// A restore is the raise: the window is on screen, so the user already has the instance and
/// the caller stops. `SetForegroundWindow` is still called afterwards, but only as best effort,
/// because Windows may refuse to hand focus over (foreground lock); that refusal does not
/// un-restore the window, so it is not a reason to keep an invisible duplicate alive, and its
/// result is ignored. A failed `ShowWindow` is different: nothing was put on screen, so it must
/// not report a raise. Neither call is the uniqueness guarantee; the mutex is.
fn focus_existing(image: &str, title: &str) -> bool {
    let candidates = visible_windows();
    let Some(target) = choose(&candidates, image, title) else {
        return false;
    };
    // Re-read the minimized bit: the window may have changed state since it was enumerated.
    // SAFETY: the same live handle; this only reads a window state bit.
    let minimized = (unsafe { IsIconic(target.window) }) != 0;
    // SAFETY: `target.window` is a live top-level handle collected moments ago; restoring it
    // is what makes a minimized instance visible again. `ShowWindow` reports whether the window
    // was previously visible, and only visible windows are candidates, so a zero means the
    // handle died since it was enumerated: nothing is on screen and no raise is reported.
    // The parentheses group the block's value for the comparison.
    let restored = (unsafe { ShowWindow(target.window, show_command(minimized)) }) != 0;
    if !restored {
        return false;
    }
    // SAFETY: the same live handle; this call only requests activation, and the request is best
    // effort. The parentheses group the block's value; the result is deliberately discarded
    // because the window is already restored.
    let _foreground = (unsafe { SetForegroundWindow(target.window) }) != 0;
    true
}

/// Every visible top-level window with the image path of the process that owns it.
fn visible_windows() -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    // SAFETY: `collect` is the callback `EnumWindows` requires, and `lparam` carries a valid
    // `*mut Vec<Candidate>` for the whole call because `candidates` outlives it. The callback
    // neither unwraps nor indexes, so it cannot unwind across the FFI boundary.
    // A zero result means the enumeration stopped early; the windows collected before that are
    // still valid, and an incomplete list only costs a redirect, so the result is not acted on.
    let _enumerated = unsafe {
        EnumWindows(Some(collect), &mut candidates as *mut Vec<Candidate> as isize)
    };
    candidates
}

/// `EnumWindows` callback: keep every visible window whose owning image can be read. Only
/// visible windows are collected, and [`choose`] re-checks visibility so a caller that passes
/// windows from elsewhere still cannot raise a hidden one.
unsafe extern "system" fn collect(window: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `*mut Vec<Candidate>` passed to `EnumWindows`, which outlives
    // every callback, and no other reference to that vector exists during the enumeration.
    let candidates = unsafe { &mut *(lparam as *mut Vec<Candidate>) };
    // SAFETY: `window` is a live top-level handle supplied by the enumeration.
    let visible = (unsafe { IsWindowVisible(window) }) != 0;
    if visible {
        if let Some(image) = image_of(window) {
            candidates.push(Candidate {
                window,
                image,
                title: window_title(window),
                visible: true,
                // SAFETY: the same live handle; this only reads a window state bit.
                minimized: (unsafe { IsIconic(window) }) != 0,
            });
        }
    }
    1 // keep enumerating
}

/// Full image path of the process that owns `window`, or `None` when it cannot be read: an
/// exit race, another user's or an elevated process, or a path longer than the buffer.
fn image_of(window: HWND) -> Option<String> {
    let mut pid: u32 = 0;
    // SAFETY: `window` came from `EnumWindows` and the out-parameter is a live `u32`.
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if pid == 0 {
        return None;
    }
    // SAFETY: opening the pid we were just given only asks for limited query rights; the handle
    // is closed below on every path.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; MAX_IMAGE_UNITS as usize];
    let mut units = MAX_IMAGE_UNITS;
    // SAFETY: `process` is a live handle with query rights, the buffer holds `units` writable
    // UTF-16 units, and `units` is a live counter the call updates.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut units) };
    // SAFETY: `process` came from `OpenProcess` and is closed here, on every path.
    unsafe { CloseHandle(process) };
    if ok == 0 {
        return None;
    }
    let Ok(length) = usize::try_from(units) else {
        return None;
    };
    Some(String::from_utf16_lossy(buffer.get(..length)?))
}

/// The window text, capped. It only ranks candidates, so a truncated title is harmless.
fn window_title(window: HWND) -> String {
    let mut buffer = vec![0u16; MAX_TITLE_UNITS as usize];
    // SAFETY: `window` came from `EnumWindows`, and the buffer holds exactly the count of
    // writable UTF-16 units passed with it. The bound is far below `i32::MAX`, so the cast is
    // exact and cannot truncate.
    let written = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), MAX_TITLE_UNITS as i32) };
    let Ok(count) = usize::try_from(written) else {
        return String::new();
    };
    buffer
        .get(..count)
        .map_or_else(String::new, String::from_utf16_lossy)
}
