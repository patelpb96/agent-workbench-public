//! A hung (suspended) host must never stall the broker, other sessions or listings for long,
//! and the session must recover when the host resumes.
#![cfg(windows)]
use std::{
    ffi::c_void,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::OpenProcess};
use wb_session::{
    client::Client,
    proto::{b64_decode, ClientKind, Request, Response},
};

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(h: *mut c_void) -> i32;
    fn NtResumeProcess(h: *mut c_void) -> i32;
}

const PROCESS_SUSPEND_RESUME: u32 = 0x0800;

fn suspend(pid: u32, on: bool) {
    unsafe {
        let h = OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid);
        assert!(!h.is_null(), "open host process");
        let status = if on { NtSuspendProcess(h) } else { NtResumeProcess(h) };
        assert!(status >= 0, "suspend/resume status {status:#x}");
        CloseHandle(h);
    }
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new() -> Env {
        let id = format!("hung-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}") }
    }
    fn client(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Cli).expect("connect")
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn daemon(env: &Env) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wb-daemon"))
        .arg("--data-dir")
        .arg(&env.dir)
        .arg("--pipe-name")
        .arg(&env.pipe)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    Daemon(child)
}

fn create(c: &mut Client) -> (String, u32) {
    let Response::Created { session } = c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: None }).unwrap() else { panic!("create") };
    (session.id, session.host_pid)
}

fn states(c: &mut Client) -> Vec<(String, String)> {
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!("list") };
    items.into_iter().map(|s| (s.id, s.state)).collect()
}

fn state_of(list: &[(String, String)], id: &str) -> String {
    list.iter().find(|(i, _)| i == id).map(|(_, s)| s.clone()).unwrap_or_default()
}

fn output(c: &mut Client, id: &str) -> String {
    let (mut text, mut from) = (String::new(), 0u64);
    loop {
        let Response::Output { data, next, .. } = c.call(&Request::Read { session: id.into(), from }).unwrap() else { panic!("read") };
        text.push_str(&String::from_utf8_lossy(&b64_decode(&data).unwrap()));
        if next == from {
            return text;
        }
        from = next;
    }
}

#[test]
fn a_suspended_host_is_reported_quickly_does_not_block_others_and_recovers() {
    let env = Env::new();
    let _d = daemon(&env);
    let mut c = env.client();
    let (a, host_a) = create(&mut c);
    let (b, _) = create(&mut c);
    assert_eq!(state_of(&states(&mut c), &a), "running");

    suspend(host_a, true);
    // A second client works against B while A's call is stuck waiting for its deadline.
    let env_pipe = env.pipe.clone();
    let dir = env.dir.clone();
    let slow = thread::spawn(move || {
        let mut c2 = Client::connect(&env_pipe, &dir, ClientKind::Cli).unwrap();
        let t = Instant::now();
        let list = states(&mut c2);
        (t.elapsed(), list)
    });
    thread::sleep(Duration::from_millis(700));
    let mut c3 = env.client();
    let t = Instant::now();
    assert!(matches!(c3.call(&Request::Input { session: b.clone(), text: "echo B_STILL_OK\r".into() }).unwrap(), Response::Ok));
    assert!(t.elapsed() < Duration::from_secs(2), "another session is not blocked by the hung one: {:?}", t.elapsed());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output(&mut c3, &b).contains("B_STILL_OK") {
        assert!(Instant::now() < deadline, "B never answered");
        thread::sleep(Duration::from_millis(100));
    }

    let (took, list) = slow.join().unwrap();
    assert!(took >= Duration::from_secs(4) && took < Duration::from_secs(9), "the listing waits for A's deadline once: {took:?}");
    assert_eq!(state_of(&list, &a), "unresponsive");
    assert_eq!(state_of(&list, &b), "running");
    // Known-unresponsive sessions are reported without asking the host again.
    let t = Instant::now();
    assert_eq!(state_of(&states(&mut c), &a), "unresponsive");
    assert!(t.elapsed() < Duration::from_secs(1), "repeat listing is fast: {:?}", t.elapsed());

    // The host resumes: after the retry interval the broker reconnects with a higher epoch.
    suspend(host_a, false);
    thread::sleep(Duration::from_millis(5_500));
    assert_eq!(state_of(&states(&mut c), &a), "running");
    assert!(matches!(c.call(&Request::Input { session: a.clone(), text: "echo A_BACK\r".into() }).unwrap(), Response::Ok));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output(&mut c, &a).contains("A_BACK") {
        assert!(Instant::now() < deadline, "A never recovered");
        thread::sleep(Duration::from_millis(100));
    }
    for id in [a, b] {
        let _ = c.call(&Request::End { session: id });
    }
}
