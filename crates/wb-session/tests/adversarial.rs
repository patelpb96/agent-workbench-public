//! Regression tests for the defects found by the Step 4 adversarial review (summarised in
//! docs/reviews/step4-adversarial-results.md). Each test states the requirement it checks. All of them pass; they were
//! written first, run against the code to confirm the defect, and the code was then fixed. A new confirmed defect
//! should be added the same way: test first, marked `#[ignore = "..."]` with what is wrong until it is fixed.
#![cfg(windows)]
use std::{
    ffi::c_void,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Storage::FileSystem::{CreateFileW, OPEN_EXISTING},
    System::Threading::OpenProcess,
};
use wb_session::{
    client::Client,
    proto::{Admin, ClientKind, Request, Response},
    win::{kill_pid, pid_running},
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

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new(tag: &str) -> Env {
        let id = format!("{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
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

/// Kills the daemon when dropped.
struct Daemon(Child);
impl Daemon {
    fn pid(&self) -> u32 {
        self.0.id()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// Kills the listed host processes when dropped, so a failing test never leaves a terminal behind.
#[derive(Default)]
struct Hosts(Vec<u32>);
impl Drop for Hosts {
    fn drop(&mut self) {
        for pid in &self.0 {
            kill_pid(*pid);
        }
    }
}

fn daemon(env: &Env) -> (Daemon, String) {
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
    (Daemon(child), line)
}
fn create(c: &mut Client) -> (String, u32) {
    let Response::Created { session } = c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: None }).unwrap() else { panic!("create") };
    (session.id, session.host_pid)
}
fn list(c: &mut Client) -> Vec<wb_session::proto::SessionInfo> {
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!("list") };
    items
}
fn code_of(r: &Response) -> String {
    match r {
        Response::Error { code, .. } => code.clone(),
        other => format!("not an error: {other:?}"),
    }
}

// ---- F1: what happens to a staged draft when the host cannot take it ------------------------------------------------

/// The host cannot be reached at all (it died): nothing was typed, so the draft must still be staged for a retry.
#[test]
fn a_draft_is_kept_when_the_host_could_not_be_reached() {
    let env = Env::new("f1a");
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut human = env.client();
    let (id, host) = create(&mut human);
    let _hosts = Hosts(vec![host]);
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").unwrap();
    assert!(matches!(agent.call(&Request::Propose { session: id.clone(), text: "echo IMPORTANT_DRAFT".into() }).unwrap(), Response::Ok));
    kill_pid(host);
    thread::sleep(Duration::from_millis(500));

    let accepted = human.call(&Request::AcceptDraft { session: id.clone() }).unwrap();
    assert_eq!(code_of(&accepted), "host_error", "the host is gone, so the accept cannot succeed");
    let Response::Draft { draft } = human.call(&Request::GetDraft { session: id.clone() }).unwrap() else { panic!("get draft") };
    assert_eq!(draft.map(|d| d.text).as_deref(), Some("echo IMPORTANT_DRAFT"), "nothing was typed, so the draft must still be staged");
}

/// The host was reached but never confirmed (it is suspended): the draft may still be typed when it resumes, so it must
/// NOT be kept (a retry could type it twice), the error must say so, and it must arrive exactly once.
#[test]
fn an_unconfirmed_draft_is_not_kept_because_it_may_still_arrive_and_arrives_once() {
    let env = Env::new("f1b");
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut human = env.client();
    let (id, host) = create(&mut human);
    let _hosts = Hosts(vec![host]);
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").unwrap();
    assert!(matches!(agent.call(&Request::Propose { session: id.clone(), text: "echo UNCONFIRMED_MARK".into() }).unwrap(), Response::Ok));

    suspend(host, true);
    let accepted = human.call(&Request::AcceptDraft { session: id.clone() }).unwrap();
    suspend(host, false);
    assert_eq!(code_of(&accepted), "host_unconfirmed");
    let Response::Draft { draft } = human.call(&Request::GetDraft { session: id.clone() }).unwrap() else { panic!("get draft") };
    assert!(draft.is_none(), "an unconfirmed draft must not stay staged (a retry could type it twice)");

    // The text reaches the shell once the host runs again (the request was already in the pipe).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut from = 0u64;
    let mut seen = String::new();
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(300));
        let Ok(Response::Output { data, next, .. }) = human.call(&Request::Read { session: id.clone(), from }) else { continue };
        seen.push_str(&String::from_utf8_lossy(&wb_session::proto::b64_decode(&data).unwrap_or_default()));
        from = next;
        if seen.matches("UNCONFIRMED_MARK").count() >= 2 {
            break; // typed (echoed) and run (printed)
        }
    }
    assert!(seen.matches("UNCONFIRMED_MARK").count() >= 1, "the draft never arrived: {seen:?}");
    assert!(seen.matches("echo UNCONFIRMED_MARK").count() <= 1, "the draft was typed more than once: {seen:?}");
}

// ---- F2: ending a session whose host does not answer must not forget a still-running host -------------------------

#[test]
fn ending_an_unresponsive_session_must_not_leave_an_untracked_running_host() {
    let env = Env::new("f2");
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let (id, host) = create(&mut c);
    let _hosts = Hosts(vec![host]);

    suspend(host, true);
    let ended = c.call(&Request::End { session: id.clone() }).unwrap();
    if pid_running(host) {
        suspend(host, false); // (it was stopped by force when it never answered, so usually there is nothing to resume)
    }
    // The host did not answer. It is either already ended by the request it had in its pipe, or stopped by force.
    assert!(matches!(ended, Response::Ok), "ending a session whose host does not answer should still end it, got {ended:?}");
    thread::sleep(Duration::from_secs(4)); // the host may still act on an End it had already received
    let tracked = list(&mut c).iter().any(|s| s.id == id);
    let alive = pid_running(host);
    assert!(!alive || tracked, "host {host} is still running but the broker has forgotten session {id}");
}

// ---- F3 / F4: sessions whose host died ----------------------------------------------------------------------------

#[test]
fn a_lost_session_stays_visible_across_more_than_one_broker_restart() {
    let env = Env::new("f3");
    let (d1, _) = daemon(&env);
    let mut c = env.client();
    let (id, host) = create(&mut c);
    drop(c);
    kill_pid(d1.pid());
    kill_pid(host);
    thread::sleep(Duration::from_millis(500));
    let (d2, ready2) = daemon(&env);
    assert!(ready2.contains("lost=1"), "the first restart reports the loss: {ready2}");
    kill_pid(d2.pid());
    thread::sleep(Duration::from_millis(300));
    let (_d3, ready3) = daemon(&env);
    let mut c = env.client();
    let items = list(&mut c);
    assert!(items.iter().any(|s| s.id == id && s.state == "lost"), "the lost session vanished at the second restart ({ready3}): {items:?}");
}

#[test]
fn a_session_whose_host_died_can_be_dismissed_while_the_broker_runs() {
    let env = Env::new("f4a");
    let (_d, _) = daemon(&env);
    let mut c = env.client();
    let (id, host) = create(&mut c);
    kill_pid(host);
    thread::sleep(Duration::from_millis(500));
    let ended = c.call(&Request::End { session: id.clone() }).unwrap();
    assert!(matches!(ended, Response::Ok), "ending a session whose host is already gone should simply dismiss it, got {ended:?}");
    assert!(list(&mut c).iter().all(|s| s.id != id));
}

#[test]
fn a_lost_session_can_be_dismissed_after_a_broker_restart() {
    let env = Env::new("f4b");
    let (d1, _) = daemon(&env);
    let mut c = env.client();
    let (id, host) = create(&mut c);
    drop(c);
    kill_pid(d1.pid());
    kill_pid(host);
    thread::sleep(Duration::from_millis(500));
    let (_d2, ready) = daemon(&env);
    assert!(ready.contains("lost=1"), "{ready}");
    let mut c = env.client();
    let ended = c.call(&Request::End { session: id.clone() }).unwrap();
    assert!(matches!(ended, Response::Ok), "a lost session should be dismissible, got {ended:?}");
    assert!(list(&mut c).iter().all(|s| s.id != id), "it is still listed");
}

// ---- F5: a resize is part of the session's recorded state ---------------------------------------------------------

#[test]
fn a_resized_terminal_keeps_its_size_across_a_broker_restart() {
    let env = Env::new("f5");
    let (d1, _) = daemon(&env);
    let mut c = env.client();
    let (id, host) = create(&mut c);
    let _hosts = Hosts(vec![host]);
    assert!(matches!(c.call(&Request::Resize { session: id.clone(), cols: 160, rows: 50 }).unwrap(), Response::Ok));
    drop(c);
    kill_pid(d1.pid());
    thread::sleep(Duration::from_millis(300));
    let (_d2, _) = daemon(&env);
    let mut c = env.client();
    let items = list(&mut c);
    let s = items.iter().find(|s| s.id == id).expect("re-adopted");
    assert_eq!((s.cols, s.rows), (160, 50), "the size changed before the restart is reported as the old one afterwards");
}

// ---- F6: the admin "end all" confirmation must mean what it said --------------------------------------------------

#[test]
fn confirming_end_all_must_not_end_sessions_created_after_the_count_was_shown() {
    let env = Env::new("f6");
    let (_d, _) = daemon(&env);
    let mut c = env.client();
    let (s1, h1) = create(&mut c);
    let mut hosts = Hosts(vec![h1]);
    assert!(matches!(c.call(&Request::AdminBegin { action: Admin::EndAllSessions }).unwrap(), Response::AdminPending { affected: 1, .. }));
    let (s2, h2) = create(&mut c);
    hosts.0.push(h2);
    thread::sleep(Duration::from_millis(3_300));
    let _ = c.call(&Request::AdminConfirm { typed: "1".into() }).unwrap();
    let items = list(&mut c);
    let alive = |id: &str| items.iter().any(|s| s.id == id && s.state == "running");
    assert!(alive(&s2), "the session created after the count was shown ({s2}) was ended by confirming a count of 1 (s1 alive: {})", alive(&s1));
    assert!(!alive(&s1), "the session that WAS counted must have been ended");
}

// ---- F7: a connection that failed or never authenticated must not leave a timer that fires on a reused handle -----

#[test]
fn stale_handshake_timers_do_not_cancel_the_reads_of_later_connections() {
    let env = Env::new("f7");
    let (_d, _) = daemon(&env);
    // Connections that stay long enough to be given a handshake timer, then go away without authenticating (a client
    // that connects, reads the challenge and disappears). Their pipe handles are then freed inside the broker.
    let name = wb_session::win::wide(&env.pipe);
    let raw_name = name.as_ptr() as usize;
    let openers: Vec<_> = (0..60)
        .map(|_| {
            thread::spawn(move || {
                let h = unsafe { CreateFileW(raw_name as *const u16, 0xC000_0000, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
                if wb_session::win::valid(h) {
                    thread::sleep(Duration::from_millis(250));
                    unsafe { CloseHandle(h) };
                }
            })
        })
        .collect();
    for o in openers {
        o.join().unwrap();
    }
    thread::sleep(Duration::from_millis(200));
    // Live, authenticated, idle connections: the broker is blocked reading each one, on handles that may reuse the freed values.
    let mut clients: Vec<Client> = (0..60).map(|_| env.client()).collect();
    // The unauthenticated connections' 5 second timers now fire. Nobody sends anything meanwhile.
    thread::sleep(Duration::from_millis(5_800));
    let mut broken = 0;
    for c in clients.iter_mut() {
        if c.call(&Request::List {}).is_err() {
            broken += 1;
        }
    }
    assert_eq!(broken, 0, "{broken} of 60 idle connections were cut by a timer belonging to a connection that was already closed");
}

// ---- epoch.txt: the broker's fencing number must survive a damaged file ------------------------------------------

#[test]
fn an_emptied_epoch_file_must_not_fence_the_broker_out_of_its_running_hosts() {
    let env = Env::new("epoch");
    std::fs::create_dir_all(&env.dir).unwrap();
    // The counter is ahead of the wall clock (a clock that stepped back, or many reconnects).
    std::fs::write(env.dir.join("epoch.txt"), (wb_session::win::now_ms() + 3_600_000).to_string()).unwrap();
    let (d1, _) = daemon(&env);
    let mut c = env.client();
    let (_, host) = create(&mut c);
    let _hosts = Hosts(vec![host]);
    drop(c);
    kill_pid(d1.pid());
    thread::sleep(Duration::from_millis(300));
    std::fs::write(env.dir.join("epoch.txt"), "").unwrap(); // as left by a write cut short
    let (_d2, ready) = daemon(&env);
    assert!(ready.contains("sessions=1") && ready.contains("lost=0"), "every running terminal was reported lost after the epoch file was emptied: {ready}");
}

// ---- connections are capped ------------------------------------------------------------------------------------

#[test]
fn the_broker_serves_at_most_its_connection_limit_and_recovers_when_they_close() {
    use wb_session::{broker::MAX_CONNECTIONS, proto::recv_response, win::PipeIo};
    let env = Env::new("cap");
    let (_d, _) = daemon(&env);
    let name = wb_session::win::wide(&env.pipe);
    let mut handles = Vec::new();
    for _ in 0..(MAX_CONNECTIONS + 40) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let h = unsafe { CreateFileW(name.as_ptr(), 0xC000_0000, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
            if wb_session::win::valid(h) {
                handles.push(h);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "could not open connection {} of {}", handles.len() + 1, MAX_CONNECTIONS + 40);
            thread::sleep(Duration::from_millis(5));
        }
    }
    // Each connection is either given a challenge (served) or closed by the broker (refused).
    let (mut served, mut refused) = (0, 0);
    for h in &handles {
        // SAFETY: the handle stays open until closed below.
        let mut io = unsafe { PipeIo::from_raw(*h) };
        match recv_response(&mut io) {
            Ok(Some(Response::Challenge { .. })) => served += 1,
            _ => refused += 1,
        }
    }
    assert!(served <= MAX_CONNECTIONS, "{served} connections were served; the limit is {MAX_CONNECTIONS}");
    assert!(refused >= 30, "only {refused} of {} connections were refused", MAX_CONNECTIONS + 40);
    for h in handles {
        unsafe { CloseHandle(h) };
    }
    // Once the idle connections are gone the broker serves normal clients again.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut c) = Client::connect(&env.pipe, &env.dir, ClientKind::Cli) {
            if matches!(c.call(&Request::List {}), Ok(Response::Sessions { .. })) {
                break;
            }
        }
        assert!(std::time::Instant::now() < deadline, "the broker did not recover after the idle connections closed");
        thread::sleep(Duration::from_millis(200));
    }
}
