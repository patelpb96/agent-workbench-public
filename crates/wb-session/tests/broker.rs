//! End-to-end tests of the real broker, host and client over named pipes.
//! Each test uses its own data directory and pipe name, so tests run in parallel
//! and never touch a real install.
#![cfg(windows)]
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use wb_session::{
    broker::{self, BrokerConfig, Shared, StartError},
    client::Client,
    proto::{b64_decode, Admin, ClientKind, Duration as GrantFor, Request, Response},
    win::{kill_pid, pid_running},
};

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new(tag: &str) -> Env {
        let id = format!("{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let dir = std::env::temp_dir().join(format!("wbtest-{id}"));
        Env { dir, pipe: format!(r"\\.\pipe\wbtest-{id}") }
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

fn host_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wb-host"))
}

/// Broker running in this process (so tests can call the in-process agent API).
fn in_process(env: &Env, gate: &str) -> (Arc<Shared>, thread::JoinHandle<Result<(), StartError>>) {
    let cfg = BrokerConfig { data_dir: env.dir.clone(), pipe_name: env.pipe.clone(), host_exe: host_exe(), gate_phrase: gate.into() };
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || broker::run(cfg, move |s| tx.send(s.clone()).unwrap()));
    (rx.recv_timeout(Duration::from_secs(10)).expect("broker ready"), h)
}

/// Kills the daemon when dropped, so a failing test never leaks a process.
struct Daemon(Child);
impl std::ops::Deref for Daemon {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for Daemon {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

fn new_terminal(c: &mut Client) -> String {
    match c.call(&Request::NewTerminal { cols: 100, rows: 30, label: Some("t".into()), shell: None, extended_history: None, cwd: None }).unwrap() {
        Response::Created { session } => session.id,
        other => panic!("create failed: {other:?}"),
    }
}

fn read_all(c: &mut Client, id: &str) -> (String, u64) {
    let (mut text, mut from) = (String::new(), 0u64);
    loop {
        match c.call(&Request::Read { session: id.into(), from }).unwrap() {
            Response::Output { data, next, .. } => {
                let bytes = b64_decode(&data).unwrap();
                text.push_str(&String::from_utf8_lossy(&bytes));
                if next == from {
                    return (text, next);
                }
                from = next;
            }
            other => panic!("read failed: {other:?}"),
        }
    }
}

fn wait_for(c: &mut Client, id: &str, marker: &str, count: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (text, _) = read_all(c, id);
        if text.matches(marker).count() >= count {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {marker:?} x{count}; got {text:?}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn host_pids(c: &mut Client) -> Vec<u32> {
    match c.call(&Request::List {}).unwrap() {
        Response::Sessions { items } => items.iter().map(|s| s.host_pid).collect(),
        other => panic!("{other:?}"),
    }
}

fn end_all(c: &mut Client) {
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { return };
    for s in items.iter().filter(|s| s.state != "lost") {
        let _ = c.call(&Request::End { session: s.id.clone() });
    }
}

#[test]
fn auth_rejects_wrong_secret_and_forged_or_malformed_requests() {
    let env = Env::new("auth");
    let (shared, _h) = in_process(&env, "");
    let secret = wb_session::win::load_secret(&env.dir, false).unwrap();
    assert!(Client::connect_with_secret(&env.pipe, &[0u8; 32], ClientKind::Cli).is_err(), "wrong secret");
    assert!(Client::connect_with_secret(&env.pipe, &secret[..16], ClientKind::Cli).is_err(), "short secret");
    let mut c = Client::connect_with_secret(&env.pipe, &secret, ClientKind::Gui).expect("right secret");
    assert!(matches!(c.call(&Request::List {}).unwrap(), Response::Sessions { .. }));
    // A client cannot claim an origin: the field is unknown, the request is dropped.
    let forged = br#"{"op":"input","session":"x","text":"id\r","origin":{"agent":"a1"}}"#;
    assert_eq!(c.raw(forged).unwrap(), None, "connection is closed on a forged frame");
    // Fresh connection still works; Hello cannot be repeated.
    let mut c2 = Client::connect_with_secret(&env.pipe, &secret, ClientKind::Cli).unwrap();
    let hello = br#"{"op":"hello","version":1,"client":"cli","proof":"00"}"#;
    assert!(matches!(c2.raw(hello).unwrap(), Some(Response::Error { code, .. }) if code == "already_authenticated"));
    assert_eq!(shared.human_clients(), 1, "only the authenticated, still-open connection counts");
    shared.stop();
}

#[test]
fn second_daemon_is_refused_and_a_terminal_works_end_to_end() {
    let env = Env::new("flow");
    let (mut d1, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let (mut d2, line2) = daemon(&env);
    assert!(line2.starts_with("ALREADY_RUNNING"), "{line2}");
    assert_eq!(d2.wait().unwrap().code(), Some(3));

    let mut c = env.client();
    let id = new_terminal(&mut c);
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo MARKER1\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "MARKER1", 2); // typed echo + program output
    assert!(matches!(c.call(&Request::Resize { session: id.clone(), cols: 90, rows: 25 }).unwrap(), Response::Ok));
    assert!(matches!(c.call(&Request::Resize { session: id.clone(), cols: 5, rows: 25 }).unwrap(), Response::Error { code, .. } if code == "invalid_size"));
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!() };
    assert_eq!((items[0].cols, items[0].rows, items[0].state.as_str()), (90, 25, "running"));
    end_all(&mut c);
    let _ = d1.kill();
}

#[test]
fn sessions_survive_a_hard_killed_broker_and_are_readopted() {
    let env = Env::new("survive");
    let (mut d1, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    c.call(&Request::Input { session: id.clone(), text: "echo BEFORE_KILL\r".into() }).unwrap();
    wait_for(&mut c, &id, "BEFORE_KILL", 2);
    let host = host_pids(&mut c)[0];
    drop(c);
    kill_pid(d1.id()); // hard-kill the broker
    let _ = d1.wait();
    assert!(pid_running(host), "the host outlives its broker");

    let (mut d2, ready2) = daemon(&env);
    assert!(ready2.contains("sessions=1") && ready2.contains("lost=0"), "{ready2}");
    let mut c = env.client();
    let (text, _) = read_all(&mut c, &id);
    assert!(text.contains("BEFORE_KILL"), "scrollback survived the broker restart");
    c.call(&Request::Input { session: id.clone(), text: "echo AFTER_RESTART\r".into() }).unwrap();
    wait_for(&mut c, &id, "AFTER_RESTART", 2);

    // Hard-kill the host: a restarted broker reports it lost, never revives it.
    drop(c);
    kill_pid(d2.id());
    let _ = d2.wait();
    kill_pid(host);
    thread::sleep(Duration::from_millis(500));
    let (mut d3, ready3) = daemon(&env);
    assert!(ready3.contains("sessions=0") && ready3.contains("lost=1"), "{ready3}");
    let mut c = env.client();
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!() };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].state, "lost");
    let _ = d3.kill();
}

#[test]
fn agents_need_grants_drafts_need_a_human_and_revoke_is_instant() {
    let env = Env::new("grants");
    let (shared, _h) = in_process(&env, "Ab1!");
    let mut c = env.client();
    let id = new_terminal(&mut c);

    // No grant: an agent cannot type; it can only stage a draft.
    assert_eq!(shared.agent_input("a1", &id, "echo NOPE\r"), Err("no_grant"));
    // The gate is enforced when creating a grant.
    let wrong = c.call(&Request::GrantCreate { agent: "a1".into(), session: id.clone(), duration: GrantFor::OneMinute, keep_after_detach: false, gate: "nope".into() }).unwrap();
    assert!(matches!(wrong, Response::Error { code, .. } if code == "gate_failed"));
    assert_eq!(shared.grants_len(), 0);
    let ok = c.call(&Request::GrantCreate { agent: "a1".into(), session: id.clone(), duration: GrantFor::OneMinute, keep_after_detach: false, gate: "Ab1!".into() }).unwrap();
    assert!(matches!(ok, Response::Ok));
    assert_eq!(shared.agent_input("a1", &id, "echo GRANTED\r"), Ok(()));
    wait_for(&mut c, &id, "GRANTED", 2);
    assert_eq!(shared.agent_input("a2", &id, "x"), Err("no_grant"), "per agent");
    assert_eq!(shared.agent_input("a1", "other", "x"), Err("no_grant"), "per session");

    // Revoke needs no gate and takes effect immediately.
    c.call(&Request::GrantRevoke { agent: "a1".into(), session: id.clone() }).unwrap();
    assert_eq!(shared.agent_input("a1", &id, "echo AFTER_REVOKE\r"), Err("no_grant"));

    // A grant lapses while no human client is attached, unless kept.
    c.call(&Request::GrantCreate { agent: "a1".into(), session: id.clone(), duration: GrantFor::OneHour, keep_after_detach: false, gate: "Ab1!".into() }).unwrap();
    c.call(&Request::GrantCreate { agent: "a3".into(), session: id.clone(), duration: GrantFor::OneHour, keep_after_detach: true, gate: "Ab1!".into() }).unwrap();
    let mut c2 = env.client(); // keep the broker reachable for cleanup while c disconnects
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.human_clients() > 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    drop(c2.call(&Request::List {}));
    // c2 is still a human client, so the grants hold; detach it too.
    assert_eq!(shared.agent_input("a1", &id, "echo STILL_ATTACHED\r"), Ok(()));
    wait_for(&mut c2, &id, "STILL_ATTACHED", 2);
    let mut c3 = env.client();
    drop(c2);
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.human_clients() != 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(shared.human_clients(), 1);
    // Still one human (c3): allowed. Now detach the last human.
    assert_eq!(shared.agent_input("a1", &id, "echo ONE_HUMAN\r"), Ok(()));
    wait_for(&mut c3, &id, "ONE_HUMAN", 2);
    let secret = wb_session::win::load_secret(&env.dir, false).unwrap();
    let observer = Client::connect_with_secret(&env.pipe, &secret, ClientKind::Gui).unwrap();
    drop(observer);
    // Draft flow: staged text is only typed when a human accepts, and never runs without Enter.
    shared.agent_stage("a1", &id, "echo STAGED").unwrap();
    assert!(matches!(c3.call(&Request::AcceptDraft { session: id.clone() }).unwrap(), Response::Ok));
    let before = wait_for(&mut c3, &id, "STAGED", 1);
    thread::sleep(Duration::from_millis(600));
    let (still, _) = read_all(&mut c3, &id);
    assert_eq!(still.matches("STAGED").count(), before.matches("STAGED").count(), "typed but not executed");
    c3.call(&Request::Input { session: id.clone(), text: "\r".into() }).unwrap();
    wait_for(&mut c3, &id, "STAGED", before.matches("STAGED").count() + 1);
    assert!(matches!(c3.call(&Request::AcceptDraft { session: id.clone() }).unwrap(), Response::Error { code, .. } if code == "no_draft"), "a draft is single-use");
    assert_eq!(shared.agent_stage("a1", &id, "rm\r"), Err("invalid_draft"), "control characters are never staged");

    // With every human gone, the non-kept grant lapses and the kept one holds.
    drop(c3);
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.human_clients() != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(shared.human_clients(), 0);
    assert_eq!(shared.agent_input("a1", &id, "echo UNATTENDED\r"), Err("no_grant"));
    assert_eq!(shared.agent_input("a3", &id, "echo KEPT\r"), Ok(()));

    let mut cleanup = env.client();
    end_all(&mut cleanup);
    shared.stop();
}

#[test]
fn admin_actions_need_the_countdown_and_the_typed_count_and_stopping_keeps_hosts() {
    let env = Env::new("admin");
    let (shared, h) = in_process(&env, "");
    let mut c = env.client();
    let a = new_terminal(&mut c);
    let b = new_terminal(&mut c);
    assert_ne!(a, b);
    assert_eq!(shared.session_count(), 2);

    // Confirm without begin, then too early, then mismatch.
    assert!(matches!(c.call(&Request::AdminConfirm { typed: "2".into() }).unwrap(), Response::Error { code, .. } if code == "admin_no_request"));
    let pending = c.call(&Request::AdminBegin { action: Admin::EndAllSessions }).unwrap();
    assert!(matches!(pending, Response::AdminPending { affected: 2, .. }));
    assert!(matches!(c.call(&Request::AdminConfirm { typed: "2".into() }).unwrap(), Response::Error { code, .. } if code == "admin_too_early"));
    thread::sleep(Duration::from_millis(3_200));
    assert!(matches!(c.call(&Request::AdminConfirm { typed: "3".into() }).unwrap(), Response::Error { code, .. } if code == "admin_mismatch"));
    assert_eq!(shared.session_count(), 2, "nothing ended by wrong attempts");
    assert!(matches!(c.call(&Request::AdminConfirm { typed: "2".into() }).unwrap(), Response::Ok));
    assert_eq!(shared.session_count(), 0);

    // Stopping the daemon never ends sessions.
    let keep = new_terminal(&mut c);
    let host = host_pids(&mut c)[0];
    c.call(&Request::AdminBegin { action: Admin::StopDaemon }).unwrap();
    thread::sleep(Duration::from_millis(3_200));
    assert!(matches!(c.call(&Request::AdminConfirm { typed: "0".into() }).unwrap(), Response::Ok));
    assert!(h.join().unwrap().is_ok(), "broker exited cleanly");
    assert!(pid_running(host), "host survived the daemon stopping ({keep})");
    kill_pid(host);
}

// ---- host hardening (Argon spike review) --------------------------------

/// A test's connection to a host pipe; the handle is closed when it is dropped.
struct HostPipe {
    io: wb_session::win::PipeIo,
    h: windows_sys::Win32::Foundation::HANDLE,
}
impl Drop for HostPipe {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.h) };
    }
}

fn open_host_pipe(id: &str, nonce: &str) -> HostPipe {
    use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
    let name = wb_session::win::wide(&wb_session::host::host_pipe_name(id, nonce));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if wb_session::win::valid(h) {
            // SAFETY: `h` is a valid pipe handle owned by the HostPipe, which closes it.
            return HostPipe { io: unsafe { wb_session::win::PipeIo::from_raw(h) }, h };
        }
        assert!(Instant::now() < deadline, "host pipe unavailable");
        thread::sleep(Duration::from_millis(50));
    }
}

fn hello(h: &mut HostPipe, epoch: u64, nonce: &str) -> Option<Vec<u8>> {
    let mut f = vec![wb_session::host::OP_HELLO];
    f.extend_from_slice(&epoch.to_le_bytes());
    f.extend_from_slice(nonce.as_bytes());
    wb_session::host::write_request(&mut h.io, &f).then_some(())?;
    wb_session::host::read_reply(&mut h.io).ok()
}

#[test]
fn a_host_drops_stale_epochs_bad_nonces_and_silent_squatters_but_stays_usable() {
    let env = Env::new("fence");
    let (d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    drop(c);
    let line = std::fs::read_to_string(env.dir.join("sessions.txt")).unwrap();
    let nonce = line.trim().split(';').nth(4).unwrap().to_owned();
    kill_pid(d.id()); // the dead broker's connection to the host is gone
    thread::sleep(Duration::from_millis(500));

    // Stale epoch: refused once, then the connection is closed (not left holding the host).
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, 1, &nonce).as_deref(), Some(&b"ERR stale_epoch_or_nonce"[..]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok(), None, "connection dropped after a failed handshake");
    drop(h);

    // Wrong nonce with a huge epoch: also refused and dropped.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, u64::MAX - 1, "wrong").as_deref(), Some(&b"ERR stale_epoch_or_nonce"[..]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok(), None);
    drop(h);

    // A frame before any handshake is refused and dropped.
    let mut h = open_host_pipe(&id, &nonce);
    assert!(wb_session::host::write_request(&mut h.io, &[wb_session::host::OP_INFO]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR unauthenticated"[..]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok(), None);
    drop(h);

    // A silent squatter is cut off by the host's handshake deadline.
    let mut h = open_host_pipe(&id, &nonce);
    let started = Instant::now();
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok(), None);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(4) && waited < Duration::from_secs(9), "{waited:?}");
    drop(h);

    // Still usable: a higher epoch with the right nonce is accepted.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, wb_session::win::now_ms() + 1_000_000, &nonce).as_deref(), Some(&b"OK"[..]));
    let mut f = vec![wb_session::host::OP_INPUT];
    f.extend_from_slice(b"exit\r");
    assert!(wb_session::host::write_request(&mut h.io, &f));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"OK"[..]));
    // Natural shell exit: the host stays up, keeps scrollback, and reports it.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(wb_session::host::write_request(&mut h.io, &[wb_session::host::OP_INFO]));
        let info = String::from_utf8_lossy(&wb_session::host::read_reply(&mut h.io).ok().unwrap()).to_string();
        if info.contains("exited=1") {
            break;
        }
        assert!(Instant::now() < deadline, "shell exit not detected: {info}");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(wb_session::host::write_request(&mut h.io, &[wb_session::host::OP_END]));
    let ended = String::from_utf8_lossy(&wb_session::host::read_reply(&mut h.io).ok().unwrap()).to_string();
    assert!(ended.starts_with("ENDED active=0 root_signaled=1"), "{ended}");
    drop(h);
}

#[test]
fn a_host_survives_corrupt_frames_and_malformed_bodies_and_stays_usable() {
    use std::io::Write;
    let env = Env::new("malformed");
    let (d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    drop(c);
    let line = std::fs::read_to_string(env.dir.join("sessions.txt")).unwrap();
    let nonce = line.trim().split(';').nth(4).unwrap().to_owned();
    kill_pid(d.id());
    thread::sleep(Duration::from_millis(500));
    let mut epoch = wb_session::win::now_ms() + 10_000_000;
    let mut next_epoch = || { epoch += 1; epoch };
    let info_ok = |h: &mut HostPipe| {
        assert!(wb_session::host::write_request(&mut h.io, &[wb_session::host::OP_INFO]));
        let info = String::from_utf8_lossy(&wb_session::host::read_reply(&mut h.io).unwrap()).to_string();
        assert!(info.contains("exited=0"), "{info}");
    };

    // A corrupt length header (4 GiB): refused with a clear error, nothing allocated, connection dropped.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    h.io.write_all(&u32::MAX.to_le_bytes()).unwrap();
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR frame_too_large"[..]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok(), None, "the connection is dropped");
    drop(h);

    // Just above the request cap is refused the same way.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    h.io.write_all(&((128u32 << 10) + 1).to_le_bytes()).unwrap();
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR frame_too_large"[..]));
    drop(h);

    // A frame cut off half way (header promises 100 bytes, 10 arrive, the peer goes away): the host just drops it.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    h.io.write_all(&100u32.to_le_bytes()).unwrap();
    h.io.write_all(&[wb_session::host::OP_INPUT; 10]).unwrap();
    drop(h);

    // An empty frame is refused and dropped.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    h.io.write_all(&0u32.to_le_bytes()).unwrap();
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR empty_frame"[..]));
    drop(h);

    // Well-framed requests with bodies of the wrong size get an error reply and the connection carries on.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    for (op, body) in [(wb_session::host::OP_READ, vec![1u8, 2, 3]), (wb_session::host::OP_READ, vec![0u8; 9]), (wb_session::host::OP_READ, vec![]), (wb_session::host::OP_RESIZE, vec![1u8]), (wb_session::host::OP_RESIZE, vec![1u8; 5])] {
        let mut f = vec![op];
        f.extend_from_slice(&body);
        assert!(wb_session::host::write_request(&mut h.io, &f));
        assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR malformed"[..]), "op {op} body of {} bytes", body.len());
    }
    // An unknown op is answered too; a resize to nonsense sizes is refused; a valid read still works.
    assert!(wb_session::host::write_request(&mut h.io, &[250]));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR unknown_op"[..]));
    let mut f = vec![wb_session::host::OP_RESIZE];
    f.extend_from_slice(&0i16.to_le_bytes());
    f.extend_from_slice(&(-5i16).to_le_bytes());
    assert!(wb_session::host::write_request(&mut h.io, &f));
    assert_eq!(wb_session::host::read_reply(&mut h.io).ok().as_deref(), Some(&b"ERR resize"[..]));
    let mut f = vec![wb_session::host::OP_READ];
    f.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(wb_session::host::write_request(&mut h.io, &f));
    let reply = wb_session::host::read_reply(&mut h.io).unwrap();
    assert_eq!(wb_session::frames::parse_read_reply(&reply).map(|(_, skipped, data)| (skipped, data.len())), Some((0, 0)), "an offset far past the end reads nothing");
    drop(h);

    // After all of that the host is healthy: its shell is still running and it still answers.
    let mut h = open_host_pipe(&id, &nonce);
    assert_eq!(hello(&mut h, next_epoch(), &nonce).as_deref(), Some(&b"OK"[..]));
    info_ok(&mut h);
    assert!(wb_session::host::write_request(&mut h.io, &[wb_session::host::OP_END]));
    let ended = String::from_utf8_lossy(&wb_session::host::read_reply(&mut h.io).unwrap()).to_string();
    assert!(ended.starts_with("ENDED active=0 root_signaled=1"), "{ended}");
}

#[test]
fn heavy_output_while_detached_never_blocks_the_shell_and_the_broker_epoch_never_goes_backwards() {
    let env = Env::new("flood");
    std::fs::create_dir_all(&env.dir).unwrap();
    // The wall clock "stepped back": a persisted epoch far in the future must still be exceeded.
    let future = wb_session::win::now_ms() + 10 * 365 * 24 * 3600 * 1000;
    std::fs::write(env.dir.join("epoch.txt"), future.to_string()).unwrap();
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let persisted: u64 = std::fs::read_to_string(env.dir.join("epoch.txt")).unwrap().trim().parse().unwrap();
    assert_eq!(persisted, future + 1, "new epoch = previous + 1 when the clock is behind");

    let mut c = env.client();
    let id = new_terminal(&mut c);
    // About 4 MiB of output with nobody reading, more than the 1 MiB ring.
    c.call(&Request::Input { session: id.clone(), text: "for /L %i in (1,1,200000) do @echo flood line %i\r".into() }).unwrap();
    c.call(&Request::Input { session: id.clone(), text: "echo FLOOD_DONE\r".into() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let tail = loop {
        let (text, next) = read_all(&mut c, &id);
        if text.contains("FLOOD_DONE") {
            break (text, next);
        }
        assert!(Instant::now() < deadline, "flood did not finish");
        thread::sleep(Duration::from_millis(250));
    };
    assert!(tail.1 > (1 << 20), "more output than the ring holds passed through: {}", tail.1);
    let Response::Output { skipped, .. } = c.call(&Request::Read { session: id.clone(), from: 0 }).unwrap() else { panic!() };
    assert!(skipped > 0, "older output was dropped from the ring, and the reader was told");
    // The shell is still responsive afterwards.
    c.call(&Request::Input { session: id.clone(), text: "echo STILL_ALIVE\r".into() }).unwrap();
    wait_for(&mut c, &id, "STILL_ALIVE", 2);
    end_all(&mut c);
}

#[test]
fn powershell_sessions_handle_unicode_and_flood_and_unknown_shells_are_unrepresentable() {
    let env = Env::new("pwsh");
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = match c.call(&Request::NewTerminal { cols: 120, rows: 30, label: Some("ps".into()), shell: Some(wb_session::proto::Shell::PowerShell), extended_history: None, cwd: None }).unwrap() {
        Response::Created { session } => session.id,
        other => panic!("{other:?}"),
    };
    // Unicode output round trips as UTF-8 through ConPTY.
    c.call(&Request::Input { session: id.clone(), text: "Write-Output ('caf' + [char]0xE9 + ' ' + [char]0x2713 + ' PS_UNICODE')\r".into() }).unwrap();
    let text = wait_for(&mut c, &id, "PS_UNICODE", 2);
    assert!(text.contains("caf\u{e9} \u{2713} PS_UNICODE"), "unicode lost: {text:?}");
    // A few MiB while nobody reads, then the shell still answers.
    c.call(&Request::Input { session: id.clone(), text: "1..100000 | ForEach-Object { \"flood line $_ ................\" }; Write-Output PS_FLOOD_DONE\r".into() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (t, _) = read_all(&mut c, &id);
        if t.contains("PS_FLOOD_DONE") {
            break;
        }
        assert!(Instant::now() < deadline, "powershell flood did not finish");
        thread::sleep(Duration::from_millis(300));
    }
    c.call(&Request::Input { session: id.clone(), text: "Write-Output PS_ALIVE\r".into() }).unwrap();
    wait_for(&mut c, &id, "PS_ALIVE", 2);
    // A shell outside the fixed menu cannot even be expressed.
    let forged = br#"{"op":"new_terminal","cols":100,"rows":30,"label":"x","shell":"calc.exe"}"#;
    assert_eq!(c.raw(forged).unwrap(), None, "unknown shell is a malformed request and drops the connection");
    let mut c = env.client();
    end_all(&mut c);
}

#[test]
fn extended_history_keeps_far_more_output_than_the_standard_ring_and_survives_reattach() {
    let env = Env::new("history");
    let (_d, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let mut make = |extended: bool| -> String {
        match c.call(&Request::NewTerminal { cols: 120, rows: 30, label: None, shell: None, extended_history: Some(extended), cwd: None }).unwrap() {
            Response::Created { session } => {
                assert_eq!(session.history_mib, if extended { 32 } else { 1 });
                session.id
            }
            other => panic!("{other:?}"),
        }
    };
    let (standard, extended) = (make(false), make(true));
    // About 3.5 MiB of output into each, with a unique first line and a terminal marker.
    for id in [&standard, &extended] {
        c.call(&Request::Input { session: id.clone(), text: "echo FIRST_LINE_MARK\r".into() }).unwrap();
        c.call(&Request::Input { session: id.clone(), text: "for /L %i in (1,1,100000) do @echo history line number %i padding padding padding\r".into() }).unwrap();
        c.call(&Request::Input { session: id.clone(), text: "echo LAST_LINE_MARK\r".into() }).unwrap();
    }
    for id in [&standard, &extended] {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let (t, _) = read_all(&mut c, id);
            if t.contains("LAST_LINE_MARK") {
                break;
            }
            assert!(Instant::now() < deadline, "output did not finish");
            thread::sleep(Duration::from_millis(300));
        }
    }
    let first_skip = |c: &mut Client, id: &str| match c.call(&Request::Read { session: id.into(), from: 0 }).unwrap() {
        Response::Output { skipped, .. } => skipped,
        other => panic!("{other:?}"),
    };
    assert!(first_skip(&mut c, &standard) > 0, "the standard 1 MiB ring dropped the start");
    assert_eq!(first_skip(&mut c, &extended), 0, "the 32 MiB ring kept everything");
    let (text, _) = read_all(&mut c, &extended);
    assert!(text.contains("FIRST_LINE_MARK"), "the very first line is still readable");
    // Reattach: the history is still there after the client disconnects and reconnects.
    drop(c);
    let mut c = env.client();
    let (again, _) = read_all(&mut c, &extended);
    assert!(again.contains("FIRST_LINE_MARK") && again.contains("LAST_LINE_MARK"));
    end_all(&mut c);
}

// ---- agent connections: propose, grants, gate persistence ---------------

fn err_code(r: Response) -> String {
    match r {
        Response::Error { code, .. } => code,
        other => format!("not an error: {other:?}"),
    }
}

#[test]
fn agents_can_only_list_and_propose_until_a_human_grants_more_and_never_count_as_a_human() {
    let env = Env::new("agentconn");
    let (shared, _h) = in_process(&env, "Ab1!");
    let mut human = env.client();
    let id = new_terminal(&mut human);
    assert_eq!(shared.human_clients(), 1);

    // Invalid or missing agent identity cannot connect as an agent; non-agents cannot carry one.
    let secret = wb_session::win::load_secret(&env.dir, false).unwrap();
    for bad in ["", "has space", "x".repeat(65).as_str()] {
        assert!(Client::connect_as(&env.pipe, &secret, ClientKind::Agent, Some(bad.to_owned())).is_err(), "{bad:?}");
    }
    assert!(Client::connect_as(&env.pipe, &secret, ClientKind::Agent, None).is_err());
    assert!(Client::connect_as(&env.pipe, &secret, ClientKind::Gui, Some("claude".into())).is_err(), "a human kind cannot name itself an agent");

    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").expect("agent connects");
    assert_eq!(shared.human_clients(), 1, "an agent connection is not a human");
    // Allowed: listing. Everything else is refused.
    assert!(matches!(agent.call(&Request::List {}).unwrap(), Response::Sessions { .. }));
    assert_eq!(err_code(agent.call(&Request::Input { session: id.clone(), text: "id\r".into() }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: None }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::End { session: id.clone() }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::AdminBegin { action: Admin::EndAllSessions }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::GrantCreate { agent: "claude".into(), session: id.clone(), duration: GrantFor::OneDay, keep_after_detach: true, gate: "Ab1!".into() }).unwrap()), "forbidden", "an agent cannot grant itself");
    assert_eq!(err_code(agent.call(&Request::SetGate { current: "Ab1!".into(), new: String::new() }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::GetDraft { session: id.clone() }).unwrap()), "forbidden");
    assert_eq!(err_code(agent.call(&Request::AcceptDraft { session: id.clone() }).unwrap()), "forbidden");
    // Reading output and typing need a live grant.
    assert_eq!(err_code(agent.call(&Request::Read { session: id.clone(), from: 0 }).unwrap()), "no_grant");
    assert_eq!(err_code(agent.call(&Request::AgentInput { session: id.clone(), text: "echo NOPE\r".into() }).unwrap()), "no_grant");

    // Proposing is allowed; the draft is visible to a human and typed only when they accept.
    assert!(matches!(agent.call(&Request::Propose { session: id.clone(), text: "echo PROPOSED".into() }).unwrap(), Response::Ok));
    assert_eq!(err_code(agent.call(&Request::Propose { session: id.clone(), text: "rm\r".into() }).unwrap()), "invalid_draft");
    assert_eq!(err_code(human.call(&Request::Propose { session: id.clone(), text: "x".into() }).unwrap()), "forbidden", "humans do not propose");
    let Response::Draft { draft: Some(d) } = human.call(&Request::GetDraft { session: id.clone() }).unwrap() else { panic!("no draft") };
    assert_eq!((d.agent.as_str(), d.text.as_str(), d.multiline), ("claude", "echo PROPOSED", false));
    assert!(matches!(human.call(&Request::DiscardDraft { session: id.clone() }).unwrap(), Response::Ok));
    assert!(matches!(human.call(&Request::GetDraft { session: id.clone() }).unwrap(), Response::Draft { draft: None }));
    agent.call(&Request::Propose { session: id.clone(), text: "echo ACCEPTED_DRAFT".into() }).unwrap();
    assert!(matches!(human.call(&Request::AcceptDraft { session: id.clone() }).unwrap(), Response::Ok));
    human.call(&Request::Input { session: id.clone(), text: "\r".into() }).unwrap();
    wait_for(&mut human, &id, "ACCEPTED_DRAFT", 2);

    // The human grants: now the agent can read and type, and the grant is listed, then revoked.
    assert_eq!(err_code(human.call(&Request::GrantCreate { agent: "claude".into(), session: id.clone(), duration: GrantFor::FifteenMinutes, keep_after_detach: false, gate: "wrong".into() }).unwrap()), "gate_failed");
    assert!(matches!(human.call(&Request::GrantCreate { agent: "claude".into(), session: id.clone(), duration: GrantFor::FifteenMinutes, keep_after_detach: false, gate: "Ab1!".into() }).unwrap(), Response::Ok));
    let Response::Grants { items } = human.call(&Request::Grants { session: id.clone() }).unwrap() else { panic!() };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].agent, "claude");
    assert!(items[0].remaining_ms > 14 * 60_000 && items[0].remaining_ms <= 15 * 60_000);
    assert!(matches!(agent.call(&Request::Read { session: id.clone(), from: 0 }).unwrap(), Response::Output { .. }));
    assert!(matches!(agent.call(&Request::AgentInput { session: id.clone(), text: "echo AGENT_TYPED\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut human, &id, "AGENT_TYPED", 2);
    // Another agent name has no grant.
    let mut other = Client::connect_agent(&env.pipe, &env.dir, "codex").unwrap();
    assert_eq!(err_code(other.call(&Request::AgentInput { session: id.clone(), text: "x".into() }).unwrap()), "no_grant");
    human.call(&Request::GrantRevoke { agent: "claude".into(), session: id.clone() }).unwrap();
    assert_eq!(err_code(agent.call(&Request::AgentInput { session: id.clone(), text: "echo LATE\r".into() }).unwrap()), "no_grant");
    assert_eq!(err_code(agent.call(&Request::Read { session: id.clone(), from: 0 }).unwrap()), "no_grant");

    // With only agents connected, a non-kept grant lapses: agents never keep grants alive.
    human.call(&Request::GrantCreate { agent: "claude".into(), session: id.clone(), duration: GrantFor::OneHour, keep_after_detach: false, gate: "Ab1!".into() }).unwrap();
    drop(human);
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.human_clients() != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(err_code(agent.call(&Request::AgentInput { session: id.clone(), text: "echo UNATTENDED\r".into() }).unwrap()), "no_grant");
    let mut cleanup = env.client();
    end_all(&mut cleanup);
    shared.stop();
}

#[test]
fn the_confirmation_phrase_can_be_changed_with_the_old_one_and_survives_a_restart() {
    let env = Env::new("gatepersist");
    let (mut d1, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    let grant = |c: &mut Client, gate: &str| c.call(&Request::GrantCreate { agent: "a1".into(), session: id.clone(), duration: GrantFor::OneMinute, keep_after_detach: false, gate: gate.into() }).unwrap();
    assert!(matches!(grant(&mut c, ""), Response::Ok), "the default phrase is empty: just press Enter");
    assert_eq!(err_code(c.call(&Request::SetGate { current: "nope".into(), new: "Zz9".into() }).unwrap()), "gate_failed");
    assert_eq!(err_code(c.call(&Request::SetGate { current: String::new(), new: "has space".into() }).unwrap()), "invalid_phrase");
    assert!(matches!(c.call(&Request::SetGate { current: String::new(), new: "Zz9!".into() }).unwrap(), Response::Ok));
    assert_eq!(err_code(grant(&mut c, "")), "gate_failed", "the old phrase no longer works");
    assert!(matches!(grant(&mut c, "Zz9!"), Response::Ok));
    drop(c);
    kill_pid(d1.id());
    let _ = d1.wait();
    let (_d2, ready2) = daemon(&env);
    assert!(ready2.contains("sessions=1"), "{ready2}");
    let mut c = env.client();
    assert_eq!(err_code(grant(&mut c, "")), "gate_failed", "the changed phrase survived the restart");
    assert!(matches!(grant(&mut c, "Zz9!"), Response::Ok));
    end_all(&mut c);
}

// ---- wb attach -------------------------------------------------------------------------

/// `wb attach` with piped stdio (no real console): bytes pass through unchanged and Ctrl+] detaches.
struct Attached {
    child: Child,
    out: Arc<std::sync::Mutex<String>>,
}
impl Attached {
    fn start(env: &Env, id: &str, fresh: bool) -> Attached {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wb"));
        cmd.arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).arg("attach").arg(id);
        if fresh {
            cmd.arg("--fresh");
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let out = Arc::new(std::sync::Mutex::new(String::new()));
        let (sink, mut stdout) = (out.clone(), child.stdout.take().unwrap());
        thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Attached { child, out }
    }
    fn send(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }
    fn wait_for(&self, marker: &str, count: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = self.out.lock().unwrap().clone();
            if text.matches(marker).count() >= count {
                return text;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {marker:?} x{count}; got {text:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn detach(mut self) -> std::process::ExitStatus {
        self.send(&[0x1d]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("wb attach did not exit after the detach key");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn wb_attach_replays_history_follows_output_types_as_the_human_and_detaches_without_ending_the_terminal() {
    let env = Env::new("attach");
    let (_daemon, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    c.call(&Request::Input { session: id.clone(), text: "echo BEFORE_ATTACH\r".into() }).unwrap();
    wait_for(&mut c, &id, "BEFORE_ATTACH", 2);

    // Attached: the history is replayed, new output follows, keystrokes reach the shell.
    let mut a = Attached::start(&env, &id, false);
    a.wait_for("BEFORE_ATTACH", 2);
    a.send(b"echo FROM_ATTACH\r");
    a.wait_for("FROM_ATTACH", 2);
    // A character split across two writes is held back until it is complete.
    let smile = "\u{1F600}".as_bytes();
    a.send(&smile[..2]);
    thread::sleep(Duration::from_millis(100));
    a.send(&smile[2..]);
    a.send(b"\r");
    let status = a.detach();
    assert!(status.success(), "detaching is a normal exit: {status:?}");
    // The terminal kept running and saw exactly what was typed (the detach key itself is never sent).
    assert!(matches!(c.call(&Request::List {}).unwrap(), Response::Sessions { items } if items.iter().any(|s| s.id == id && s.state == "running")));
    let (text, _) = read_all(&mut c, &id);
    assert!(text.contains("FROM_ATTACH") && !text.contains('\u{1d}'), "{text:?}");

    // --fresh shows only what comes next, not the history.
    let mut b = Attached::start(&env, &id, true);
    thread::sleep(Duration::from_millis(500));
    b.send(b"echo SECOND_ATTACH\r");
    let shown = b.wait_for("SECOND_ATTACH", 2);
    assert!(!shown.contains("BEFORE_ATTACH"), "--fresh must not replay history: {shown:?}");
    assert!(b.detach().success());

    // Attaching to a terminal that does not exist fails cleanly.
    let missing = Command::new(env!("CARGO_BIN_EXE_wb")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).arg("attach").arg("00000000").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert_eq!(missing.code(), Some(1));
    end_all(&mut c);
}

#[test]
fn wb_attach_ends_by_itself_when_the_terminal_is_ended() {
    let env = Env::new("attachend");
    let (_daemon, _ready) = daemon(&env);
    let mut c = env.client();
    let id = new_terminal(&mut c);
    let mut a = Attached::start(&env, &id, false);
    a.send(b"echo ALIVE\r");
    a.wait_for("ALIVE", 2);
    c.call(&Request::End { session: id.clone() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = a.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "wb attach kept running after its terminal ended");
        thread::sleep(Duration::from_millis(100));
    };
    // Both ways the attach notices the end (a failed read, the periodic state check) mean "the terminal ended": exit code 0.
    assert_eq!(status.code(), Some(0), "{status:?}");
}

#[test]
fn the_audit_log_records_who_created_and_ended_terminals_without_any_text() {
    let env = Env::new("audit");
    let (_daemon, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = match c.call(&Request::NewTerminal { cols: 100, rows: 30, label: Some("PRIVATE-LABEL-TEXT".into()), shell: None, extended_history: None, cwd: None }).unwrap() {
        Response::Created { session } => session.id,
        other => panic!("{other:?}"),
    };
    c.call(&Request::Input { session: id.clone(), text: "echo PRIVATE-TYPED-TEXT\r".into() }).unwrap();
    assert!(matches!(c.call(&Request::End { session: id.clone() }).unwrap(), Response::Ok));
    let log = std::fs::read_to_string(env.dir.join("events.log")).expect("events.log");
    assert!(log.lines().next().unwrap().contains("daemon started adopted=0 lost=0"), "{log}");
    assert!(log.contains(&format!("created {id} ")) && log.contains("by=cli"), "{log}");
    assert!(log.contains(&format!("ended {id} by=cli proof=true")), "{log}");
    assert!(!log.contains("PRIVATE"), "labels and typed text never reach the log: {log}");
    // Every line starts with a UTC timestamp.
    assert!(log.lines().all(|l| l.len() > 21 && l.as_bytes()[4] == b'-' && l.as_bytes()[10] == b'T' && l.as_bytes()[19] == b'Z'), "{log}");
}

#[test]
fn a_new_terminal_starts_in_the_users_folder_with_wb_on_the_path_and_knows_its_own_id() {
    let env = Env::new("shellenv");
    let (_daemon, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    c.call(&Request::Input { session: id.clone(), text: "echo ID_IS_%WB_TERMINAL%_END\r".into() }).unwrap();
    wait_for(&mut c, &id, &format!("ID_IS_{id}_END"), 1);
    c.call(&Request::Input { session: id.clone(), text: "cd\r".into() }).unwrap();
    let profile = std::env::var("USERPROFILE").expect("USERPROFILE");
    wait_for(&mut c, &id, &profile, 1);
    // `wb.exe` is found on PATH: it lives next to wb-host.exe.
    c.call(&Request::Input { session: id.clone(), text: "where wb.exe\r".into() }).unwrap();
    let dir = host_exe().parent().unwrap().display().to_string();
    let text = wait_for(&mut c, &id, "wb.exe", 2);
    assert!(text.to_lowercase().contains(&dir.to_lowercase()), "{text}");
    end_all(&mut c);
}

#[test]
fn a_terminal_can_start_in_a_chosen_folder_and_bad_folders_are_refused() {
    let env = Env::new("startdir");
    let (_daemon, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut c = env.client();
    // A folder with spaces (and no quoting problems): the shell's first prompt is in it.
    let dir = std::env::temp_dir().join(format!("wb start dir {}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shown = dir.display().to_string();
    let id = match c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: Some(shown.clone()) }).unwrap() {
        Response::Created { session } => session.id,
        other => panic!("{other:?}"),
    };
    c.call(&Request::Input { session: id.clone(), text: "cd\r".into() }).unwrap();
    let text = wait_for(&mut c, &id, &shown, 2);
    assert!(text.matches(&shown).count() >= 2, "{text}");
    // Refused before anything is spawned.
    let before = host_pids(&mut c).len();
    let too_long = format!(r"C:\{}", "a".repeat(250));
    for bad in [r"relative\dir", "C:", r"\\server\share", r"C:\x\..\y", "C:\\bad\"quote", "C:\\ctl\u{1}char", r"Z:\definitely\not\here\wb", too_long.as_str()] {
        let reply = c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: Some(bad.to_owned()) }).unwrap();
        assert_eq!(err_code(reply), "invalid_folder", "{bad:?}");
    }
    assert_eq!(host_pids(&mut c).len(), before, "no host was started for a refused folder");
    // Without a folder it starts in the user's folder, as before.
    let plain = new_terminal(&mut c);
    c.call(&Request::Input { session: plain.clone(), text: "cd\r".into() }).unwrap();
    wait_for(&mut c, &plain, &std::env::var("USERPROFILE").unwrap(), 1);
    end_all(&mut c);
    let _ = std::fs::remove_dir_all(&dir);
}
