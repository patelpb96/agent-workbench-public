//! Opt-in "start the terminal service when I sign in", through the per-user Run key.
//! Deliberately NOT a Task Scheduler task: a task runs the daemon inside a Job that
//! forbids breakaway, so it could not create persistent sessions (docs/design/
//! session-daemon.md section 14.1). The Run key starts it from the shell with no Job.
use std::{os::windows::ffi::OsStrExt, path::Path, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::Registry::{RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ},
};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "AgentWorkbenchTerminalService";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn open(access: u32) -> Option<HKEY> {
    let mut key: HKEY = null_mut();
    let sub = wide(RUN_KEY);
    let rc = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, null_mut(), REG_OPTION_NON_VOLATILE, access, null_mut(), &mut key, null_mut()) };
    (rc == ERROR_SUCCESS).then_some(key)
}

pub fn is_enabled(name: &str) -> bool {
    let Some(key) = open(KEY_READ) else { return false };
    let n = wide(name);
    let rc = unsafe { RegQueryValueExW(key, n.as_ptr(), null_mut(), null_mut(), null_mut(), null_mut()) };
    unsafe { RegCloseKey(key) };
    rc == ERROR_SUCCESS
}

/// The value is the quoted daemon path only; no arguments, nothing user-supplied.
pub fn set(name: &str, daemon: &Path, enabled: bool) -> bool {
    let Some(key) = open(KEY_SET_VALUE) else { return false };
    let n = wide(name);
    let rc = if enabled {
        let mut text: Vec<u16> = std::ffi::OsString::from(format!("\"{}\"", daemon.display())).encode_wide().collect();
        text.push(0);
        unsafe { RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, text.as_ptr() as *const u8, (text.len() * 2) as u32) }
    } else {
        let rc = unsafe { RegDeleteValueW(key, n.as_ptr()) };
        if rc == 2 { ERROR_SUCCESS } else { rc } // already absent is fine
    };
    unsafe { RegCloseKey(key) };
    rc == ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_and_disabling_round_trips_and_disabling_twice_is_fine() {
        let name = format!("AgentWorkbenchTest{}", std::process::id());
        let exe = Path::new(r"C:\Program Files\Example App\wb-daemon.exe");
        assert!(!is_enabled(&name));
        assert!(set(&name, exe, true));
        assert!(is_enabled(&name));
        assert!(set(&name, exe, false));
        assert!(!is_enabled(&name));
        assert!(set(&name, exe, false), "removing an absent value succeeds");
    }
}
