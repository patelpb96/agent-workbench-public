//! `wb attach ID`: use a Workbench terminal from an ordinary console (like `tmux attach`).
//!
//! The terminal keeps running in its host either way; attaching only connects THIS console to it: output
//! is replayed and followed, keystrokes are sent as the human, and the console's size is applied to the
//! terminal. Ctrl+] detaches (the terminal keeps running). When stdin/stdout are not consoles (tests,
//! pipes) the bytes are passed through unchanged and end of input detaches.
#![cfg(windows)]
use crate::{
    client::Client,
    proto::{b64_decode, ClientKind, Request, Response},
    win::{console_size as console_size_of, read_some, std_handles, write_all_to, ConsoleGuard},
};
use windows_sys::Win32::Foundation::HANDLE;
use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Ctrl+] (as in telnet): detach without ending the terminal.
pub const DETACH: u8 = 0x1d;
const POLL_IDLE: Duration = Duration::from_millis(15);
/// A host that does not answer is given this long (retries times wait) to be taken off the service's list before attach gives up.
const HOST_SILENT_RETRIES: u32 = 20;
const HOST_SILENT_WAIT: Duration = Duration::from_millis(150);
const STATE_EVERY: Duration = Duration::from_millis(1000);
const SIZE_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
struct Handles {
    input: usize,
    output: usize,
}
fn raw(h: usize) -> HANDLE {
    h as HANDLE
}

/// The visible size of the console window (columns, rows), or `None` when stdout is not a console.
fn console_size(out: usize) -> Option<(u16, u16)> {
    console_size_of(raw(out))
}

fn write_out(out: usize, data: &[u8]) -> bool {
    write_all_to(raw(out), data)
}

/// Split `bytes` (carry + new input) into the longest valid UTF-8 prefix to send now and an incomplete
/// tail to keep for the next read. Invalid bytes in the middle are replaced, never dropped silently.
pub fn utf8_split(bytes: &[u8]) -> (String, Vec<u8>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), Vec::new()),
        Err(e) if e.error_len().is_none() => {
            let (ok, rest) = bytes.split_at(e.valid_up_to());
            (String::from_utf8_lossy(ok).into_owned(), rest.to_vec())
        }
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), Vec::new()),
    }
}

/// Keystrokes up to (not including) the detach key, and whether it was pressed.
pub fn split_detach(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.iter().position(|b| *b == DETACH) {
        Some(i) => (&bytes[..i], true),
        None => (bytes, false),
    }
}

/// Attach this console to terminal `id`. Returns the process exit code.
pub fn run(pipe: &str, data_dir: &Path, id: &str, fresh: bool) -> i32 {
    let (mut input, mut output) = match (Client::connect(pipe, data_dir, ClientKind::Cli), Client::connect(pipe, data_dir, ClientKind::Cli)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("cannot reach the Workbench daemon ({e}); is wb-daemon running?");
            return 2;
        }
    };
    let label = match input.call(&Request::List {}) {
        Ok(Response::Sessions { items }) => match items.into_iter().find(|s| s.id == id) {
            Some(s) if s.state == "running" => s.label,
            Some(s) => {
                eprintln!("terminal {id} is {} and cannot be attached.", s.state);
                return 1;
            }
            None => {
                eprintln!("no such terminal: {id} (see `wb ls`)");
                return 1;
            }
        },
        _ => {
            eprintln!("the terminal list could not be read");
            return 1;
        }
    };
    let (stdin_raw, stdout_raw) = std_handles();
    let (stdin_h, stdout_h) = (stdin_raw as usize, stdout_raw as usize);
    let handles = Handles { input: stdin_h, output: stdout_h };
    eprintln!("[attached to \"{label}\" ({id}). Ctrl+] detaches; the terminal keeps running.]");
    if let Some((cols, rows)) = console_size(stdout_h) {
        let _ = input.call(&Request::Resize { session: id.to_owned(), cols, rows });
    }
    // Start of the replay: the whole retained history (the screen is rebuilt from it), or only what comes next.
    let mut from = 0u64;
    if fresh {
        if let Ok(Response::Output { next, .. }) = output.call(&Request::Read { session: id.to_owned(), from: u64::MAX }) {
            from = next;
        }
    }
    let guard = ConsoleGuard::enter(raw(handles.input), raw(handles.output));
    let finished = std::sync::Arc::new(AtomicBool::new(false));
    let exit_code = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));

    // Output side: follow the terminal, mirror the console size, notice when the terminal ends.
    let session = id.to_owned();
    let (done, code) = (finished.clone(), exit_code.clone());
    let out_thread = thread::spawn(move || {
        let (mut last_state, mut last_size, mut last_dims) = (Instant::now(), Instant::now(), console_size(handles.output));
        // How many reads in a row found the session's host not answering (see the arm below).
        let mut host_silent = 0u32;
        while !done.load(Ordering::SeqCst) {
            match output.call(&Request::Read { session: session.clone(), from }) {
                Ok(Response::Output { data, next, .. }) => {
                    host_silent = 0;
                    let bytes = b64_decode(&data).unwrap_or_default();
                    if !bytes.is_empty() && !write_out(handles.output, &bytes) {
                        code.store(1, Ordering::SeqCst);
                        break;
                    }
                    if next == from {
                        thread::sleep(POLL_IDLE);
                    }
                    from = next;
                }
                // The terminal no longer exists (ended by someone, or its shell exited and it was ended): the same outcome
                // as the periodic state check below, so the same exit code (0), whichever notices first.
                Ok(Response::Error { code: error, .. }) if error == "no_such_session" => {
                    let _ = std::io::stderr().write_all(b"\r\n[the terminal has ended]\r\n");
                    break;
                }
                // The host stops answering for a moment before the service takes an ended terminal off its list (ending a
                // terminal kills the host first). That is the end of the terminal, not a lost connection: wait for the
                // service's own answer ("no such session") for a few seconds before calling it anything else.
                Ok(Response::Error { code: error, .. }) if error == "host_error" && host_silent < HOST_SILENT_RETRIES => {
                    host_silent += 1;
                    thread::sleep(HOST_SILENT_WAIT);
                    continue;
                }
                // Anything else (the service stopped, the connection broke): exit code 1.
                Ok(Response::Error { .. }) | Err(_) => {
                    let _ = std::io::stderr().write_all(b"\r\n[the connection to the terminal service was lost]\r\n");
                    code.store(1, Ordering::SeqCst);
                    break;
                }
                Ok(_) => thread::sleep(POLL_IDLE),
            }
            if last_size.elapsed() >= SIZE_EVERY {
                last_size = Instant::now();
                let dims = console_size(handles.output);
                if dims.is_some() && dims != last_dims {
                    last_dims = dims;
                    if let Some((cols, rows)) = dims {
                        let _ = output.call(&Request::Resize { session: session.clone(), cols, rows });
                    }
                }
            }
            if last_state.elapsed() >= STATE_EVERY {
                last_state = Instant::now();
                if let Ok(Response::Sessions { items }) = output.call(&Request::List {}) {
                    match items.iter().find(|s| s.id == session) {
                        Some(s) if s.state == "running" => {}
                        Some(_) | None => {
                            // Drain what is left, then say so.
                            thread::sleep(Duration::from_millis(100));
                            while let Ok(Response::Output { data, next, .. }) = output.call(&Request::Read { session: session.clone(), from }) {
                                let bytes = b64_decode(&data).unwrap_or_default();
                                if bytes.is_empty() || !write_out(handles.output, &bytes) {
                                    break;
                                }
                                from = next;
                            }
                            let _ = std::io::stderr().write_all(b"\r\n[the terminal has ended]\r\n");
                            break;
                        }
                    }
                }
            }
        }
        done.store(true, Ordering::SeqCst);
        // The main thread may be blocked reading the keyboard: leave the console as we found it, then exit.
        std::process::exit(code.load(Ordering::SeqCst));
    });
    let _ = out_thread; // never joined: the output thread ends the process when the terminal ends

    // Input side: keystrokes as the human, until the detach key (or end of input when not a console).
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    // Ends at end of input (pipe closed) or when the console goes away.
    while let Some(n) = read_some(raw(handles.input), &mut buf) {
        let (keys, detach) = split_detach(buf.get(..n).unwrap_or(&[]));
        let mut all = std::mem::take(&mut carry);
        all.extend_from_slice(keys);
        let (text, rest) = utf8_split(&all);
        carry = rest;
        if !text.is_empty() && input.call(&Request::Input { session: id.to_owned(), text }).is_err() {
            break;
        }
        if detach {
            break;
        }
    }
    finished.store(true, Ordering::SeqCst);
    drop(guard);
    eprintln!("\r\n[detached from {id}; it is still running]");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_character_is_held_back_until_complete() {
        let smile = "😀".as_bytes();
        let (text, rest) = utf8_split(&smile[..2]);
        assert_eq!((text.as_str(), rest.as_slice()), ("", &smile[..2]));
        let mut joined = rest;
        joined.extend_from_slice(&smile[2..]);
        let (text, rest) = utf8_split(&joined);
        assert_eq!((text.as_str(), rest.len()), ("😀", 0));
        let (text, rest) = utf8_split(b"ab\xffcd");
        assert_eq!(text, "ab\u{fffd}cd", "an invalid byte in the middle is replaced, not dropped");
        assert!(rest.is_empty());
        let (text, rest) = utf8_split(b"ls\r\x1b[A");
        assert_eq!((text.as_str(), rest.len()), ("ls\r\x1b[A", 0), "VT key sequences pass through");
    }

    #[test]
    fn the_detach_key_ends_input_and_is_never_sent() {
        assert_eq!(split_detach(b"ab\x1dcd"), (&b"ab"[..], true));
        assert_eq!(split_detach(b"abcd"), (&b"abcd"[..], false));
        assert_eq!(split_detach(b"\x1d"), (&b""[..], true));
    }
}
