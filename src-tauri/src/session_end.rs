//! Windows ending the session (restart, shutdown, sign-out). Windows tells every top-level window with `WM_QUERYENDSESSION` and then
//! `WM_ENDSESSION`, and ends the process soon after; it does NOT close the window, so the "save on a normal close" path never
//! runs. This module owns a hidden top-level window on a thread of its own whose only job is to hear that message and call the
//! app's save routine in time. (A message-only window is not sent these messages, which is why this one is an ordinary hidden
//! window.) It saves nothing itself: the caller passes the routine. Design: docs/design/restart-restore.md.
use std::{
    mem::zeroed,
    ptr::{null, null_mut},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_EX_TOOLWINDOW,
        WS_POPUP,
    },
};

pub const CLASS_NAME: &str = "agent-workbench-session-end";

/// The routine to run when the session is really ending, and the logger, both set once by `start`.
static ON_END: AtomicUsize = AtomicUsize::new(0);
static LOG: AtomicUsize = AtomicUsize::new(0);

fn log(text: &str) {
    let routine = LOG.load(Ordering::SeqCst);
    if routine != 0 {
        // SAFETY: `LOG` only ever holds a `fn(&str)` stored by `start`.
        let routine: fn(&str) = unsafe { std::mem::transmute::<usize, fn(&str)>(routine) };
        routine(text);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        // "May the session end?" Always yes: this app never vetoes a shutdown. Nothing is saved yet, because another program
        // can still say no and the session would carry on.
        WM_QUERYENDSESSION => {
            log("session end: Windows asked whether the session may end");
            1
        }
        // The session is ending (wparam true) or the end was cancelled (false). Only a real end saves, and the save happens here,
        // before the message returns, because the process may be ended as soon as it does.
        WM_ENDSESSION => {
            log(if wparam != 0 { "session end: the session is ending" } else { "session end: the end was cancelled" });
            if wparam != 0 {
                let routine = ON_END.load(Ordering::SeqCst);
                if routine != 0 {
                    // SAFETY: `ON_END` only ever holds a `fn()` stored by `start`.
                    let routine: fn() = unsafe { std::mem::transmute::<usize, fn()>(routine) };
                    routine();
                }
            }
            0
        }
        // SAFETY: the default procedure for a message this window does not handle.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Start listening. `on_end` runs on the listener thread when Windows really is ending the session. Returns false if the window
/// could not be created (the app then simply has no save-on-shutdown).
pub fn start(on_end: fn(), logger: fn(&str)) -> bool {
    ON_END.store(on_end as usize, Ordering::SeqCst);
    LOG.store(logger as usize, Ordering::SeqCst);
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = thread::Builder::new().name("wb-session-end".into()).stack_size(256 << 10).spawn(move || {
        let class = wide(CLASS_NAME);
        // SAFETY: plain window-class and window creation with locals that outlive the calls; the message loop below runs on
        // this thread, which owns the window.
        unsafe {
            let instance = GetModuleHandleW(null());
            let mut wc: WNDCLASSW = zeroed();
            wc.lpfnWndProc = Some(window_proc);
            wc.hInstance = instance;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), instance, null());
            let _ = sender.send(!hwnd.is_null());
            if hwnd.is_null() {
                return;
            }
            let mut msg: MSG = zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
    spawned.is_ok() && receiver.recv().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use windows_sys::Win32::{
        System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId, SendMessageW},
    };

    /// The listener window of THIS process. The running app has a window of the same class; a test must never message that one.
    fn own_window(class: &[u16]) -> HWND {
        let mut prev: HWND = null_mut();
        loop {
            // SAFETY: plain window enumeration by class name.
            let next = unsafe { FindWindowExW(null_mut(), prev, class.as_ptr(), null()) };
            if next.is_null() {
                return next;
            }
            let mut pid = 0u32;
            // SAFETY: reads the owner of a window handle just returned by the enumeration.
            unsafe { GetWindowThreadProcessId(next, &mut pid) };
            if pid == unsafe { GetCurrentProcessId() } {
                return next;
            }
            prev = next;
        }
    }

    static SAVED: AtomicU32 = AtomicU32::new(0);
    fn saved() {
        SAVED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn only_a_session_that_really_ends_runs_the_save_and_a_shutdown_is_never_vetoed() {
        fn quiet(_: &str) {}
        assert!(start(saved, quiet), "the hidden window is created");
        let class = wide(CLASS_NAME);
        let hwnd = own_window(&class);
        assert!(!hwnd.is_null(), "the window can be found by its class");
        // SAFETY: SendMessageW on a window of this process; it is delivered synchronously to the listener thread.
        unsafe {
            assert_eq!(SendMessageW(hwnd, WM_QUERYENDSESSION, 0, 0), 1, "a shutdown is never vetoed");
            assert_eq!(SAVED.load(Ordering::SeqCst), 0, "asking whether the session may end saves nothing");
            SendMessageW(hwnd, WM_ENDSESSION, 0, 0);
            assert_eq!(SAVED.load(Ordering::SeqCst), 0, "an end that was cancelled saves nothing");
            SendMessageW(hwnd, WM_ENDSESSION, 1, 0);
        }
        assert_eq!(SAVED.load(Ordering::SeqCst), 1, "an end that is happening saves once, before the message returns");
    }
}
