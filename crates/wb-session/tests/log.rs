//! Opt-in output logging end to end (tmux `pipe-pane` style): `log start` records the bytes a terminal prints from
//! that moment, `log stop` ends the file, `log status` says where it is and how far it got. Each test uses its own
//! data directory and pipe name, so tests run in parallel and never touch a real install.
#![cfg(windows)]
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use wb_session::{
    broker::{self, BrokerConfig, Shared, StartError},
    client::Client,
    proto::{b64_decode, ClientKind, Duration as GrantFor, Request, Response},
};

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new(tag: &str) -> Env {
        let id = format!("{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}") }
    }
    fn client(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Cli).expect("connect")
    }
    fn logs(&self) -> PathBuf {
        wb_session::log::dir(&self.dir)
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

/// Broker running in this process, so a test can stop it and read the shared state.
fn in_process(env: &Env, gate: &str) -> (Arc<Shared>, thread::JoinHandle<Result<(), StartError>>) {
    let cfg = BrokerConfig { data_dir: env.dir.clone(), pipe_name: env.pipe.clone(), host_exe: host_exe(), gate_phrase: gate.into() };
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || broker::run(cfg, move |s| {
        let _ = tx.send(s.clone());
    }));
    (rx.recv_timeout(Duration::from_secs(10)).expect("broker ready"), h)
}

fn new_terminal(c: &mut Client) -> String {
    match c.call(&Request::NewTerminal { cols: 100, rows: 30, label: Some("t".into()), shell: None, extended_history: None, cwd: None }).expect("create") {
        Response::Created { session } => session.id,
        other => panic!("create failed: {other:?}"),
    }
}

/// Output bytes from `from` to the end of the stream, and the offset after them.
fn read_from(c: &mut Client, id: &str, from: u64) -> (Vec<u8>, u64) {
    let (mut bytes, mut at) = (Vec::new(), from);
    loop {
        match c.call(&Request::Read { session: id.into(), from: at }).expect("read") {
            Response::Output { data, next, .. } => {
                bytes.extend_from_slice(&b64_decode(&data).unwrap_or_default());
                if next == at {
                    return (bytes, next);
                }
                at = next;
            }
            other => panic!("read failed: {other:?}"),
        }
    }
}

fn wait_for(c: &mut Client, id: &str, marker: &str, count: usize) -> (Vec<u8>, u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (bytes, at) = read_from(c, id, 0);
        if String::from_utf8_lossy(&bytes).matches(marker).count() >= count {
            return (bytes, at);
        }
        assert!(Instant::now() < deadline, "timed out waiting for {marker:?} x{count}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn log_reply(r: Response) -> (bool, Option<String>, u64, Option<String>) {
    match r {
        Response::Logging { on, path, bytes, stopped_reason } => (on, path, bytes, stopped_reason),
        other => panic!("expected a logging answer, got {other:?}"),
    }
}

/// End every terminal the test created (a host outlives its broker on purpose, so a test must not leave one behind).
fn end_all(c: &mut Client) {
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap_or(Response::Ok) else { return };
    for s in items.iter().filter(|s| s.state != "lost") {
        let _ = c.call(&Request::End { session: s.id.clone() });
    }
}

/// A `wb` invocation against this test's daemon (the CLI end of `log`).
fn wb(env: &Env, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_wb")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).args(args).stdin(Stdio::null()).output().expect("run wb");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_log_holds_exactly_what_the_terminal_printed_while_it_was_on() {
    let env = Env::new("log-io");
    let (_shared, _h) = in_process(&env, "");
    let mut c = env.client();
    let id = new_terminal(&mut c);

    // Output from before logging started must not be in the file.
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo BEFORE-LOG\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "BEFORE-LOG", 2);
    let (_, from) = read_from(&mut c, &id, 0);
    // Let the prompt that follows settle, so the start offset is the end of the output that the log will hold.
    thread::sleep(Duration::from_millis(400));
    let (_, from) = read_from(&mut c, &id, from);

    let (on, path, bytes, reason) = log_reply(c.call(&Request::LogStart { session: id.clone() }).unwrap());
    let path = path.expect("the service says where the log is");
    assert!(on && bytes == 0 && reason.is_none(), "a new log starts empty and on");
    assert!(path.ends_with(".log") && path.contains("logs"), "{path}");
    assert!(path.contains(&id), "the file is named after the terminal: {path}");
    assert!(std::path::Path::new(&path).exists(), "the file exists as soon as logging starts");
    assert!(env.logs().exists(), "the logs folder was created under the data folder");

    // Starting again is a no-op that answers with the same file.
    let (on, again, bytes, _) = log_reply(c.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on && again.as_deref() == Some(path.as_str()) && bytes == 0, "starting twice keeps one file");

    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo LOGGED-MARK-1\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "LOGGED-MARK-1", 2);
    thread::sleep(Duration::from_millis(400));

    // Status tells the truth before the stop.
    let (on, status_path, status_bytes, _) = log_reply(c.call(&Request::LogStatus { session: id.clone() }).unwrap());
    assert!(on && status_path.as_deref() == Some(path.as_str()) && status_bytes > 0);

    let (on, stopped_path, stopped_bytes, reason) = log_reply(c.call(&Request::LogStop { session: id.clone() }).unwrap());
    assert!(!on && reason.is_none(), "a requested stop is not a failure");
    assert_eq!(stopped_path.as_deref(), Some(path.as_str()));
    assert!(stopped_bytes > 0);

    // Exactly the output since the start: nothing from before it, nothing missed.
    let (since, _) = read_from(&mut c, &id, from);
    let file = std::fs::read(&path).expect("read the log");
    let text = String::from_utf8_lossy(&file);
    assert!(text.contains("LOGGED-MARK-1"), "{text:?}");
    assert!(!text.contains("BEFORE-LOG"), "output from before the start is not in the log");
    assert_eq!(file, since, "the log is exactly the output printed while it was on");
    assert_eq!(stopped_bytes, u64::try_from(file.len()).unwrap_or(0));

    // Stopping again is fine, and does not pretend to be recording.
    let (on, _, _, _) = log_reply(c.call(&Request::LogStop { session: id.clone() }).unwrap());
    assert!(!on, "stopping when not logging is Ok");
    end_all(&mut c);
}

#[test]
fn a_log_keeps_recording_after_the_client_that_started_it_disconnects() {
    let env = Env::new("log-survive");
    let (_shared, _h) = in_process(&env, "");
    let mut first = env.client();
    let id = new_terminal(&mut first);
    let (on, path, _, _) = log_reply(first.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on);
    let path = path.expect("a path");
    drop(first); // the GUI window closing, not the terminal

    let mut second = env.client();
    // The log is still on, and it is the same file (a second connection does not start another one).
    let (on, seen, _, _) = log_reply(second.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on && seen.as_deref() == Some(path.as_str()));
    assert!(matches!(second.call(&Request::Input { session: id.clone(), text: "echo AFTER-DETACH\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut second, &id, "AFTER-DETACH", 2);
    thread::sleep(Duration::from_millis(300));
    let (on, _, _, _) = log_reply(second.call(&Request::LogStop { session: id.clone() }).unwrap());
    assert!(!on);
    let text = String::from_utf8_lossy(&std::fs::read(&path).expect("read the log")).into_owned();
    assert!(text.contains("AFTER-DETACH"), "output printed after the first client left is in the log: {text:?}");
    end_all(&mut second);
}

#[test]
fn an_agent_cannot_start_or_query_a_log_even_with_a_grant() {
    let env = Env::new("log-agent");
    let (shared, _h) = in_process(&env, "");
    let mut human = env.client();
    let id = new_terminal(&mut human);
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "helper").expect("agent connect");
    // A live grant gives an agent output and input, but logging is a person's decision.
    let granted = human.call(&Request::GrantCreate { agent: "helper".into(), session: id.clone(), duration: GrantFor::FifteenMinutes, keep_after_detach: true, gate: String::new() }).unwrap();
    assert!(matches!(granted, Response::Ok), "{granted:?}");
    for req in [Request::LogStart { session: id.clone() }, Request::LogStop { session: id.clone() }, Request::LogStatus { session: id.clone() }] {
        let answer = agent.call(&req).unwrap();
        assert!(matches!(&answer, Response::Error { code, .. } if code == "forbidden"), "{answer:?}");
    }
    assert!(!env.logs().exists(), "nothing was created for the agent");
    let (on, path, _, _) = log_reply(human.call(&Request::LogStatus { session: id.clone() }).unwrap());
    assert!(!on && path.is_none(), "a person still sees that nothing is recorded");
    end_all(&mut human);
    shared.stop();
}

#[test]
fn status_still_says_where_the_log_is_after_the_terminal_ends() {
    let env = Env::new("log-end");
    let (_shared, _h) = in_process(&env, "");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    let (on, path, _, _) = log_reply(c.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on);
    let path = path.expect("a path");
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo LOGGED-THEN-ENDED\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "LOGGED-THEN-ENDED", 2);
    thread::sleep(Duration::from_millis(300));

    assert!(matches!(c.call(&Request::End { session: id.clone() }).unwrap(), Response::Ok), "the terminal ends");
    let (on, after_path, bytes, reason) = log_reply(c.call(&Request::LogStatus { session: id.clone() }).unwrap());
    assert!(!on, "logging stops with the terminal");
    assert_eq!(after_path.as_deref(), Some(path.as_str()), "the file is still reported");
    assert!(bytes > 0 && reason.as_ref().is_some_and(|r| r.contains("ended")), "and why it stopped: {reason:?}");
    let text = String::from_utf8_lossy(&std::fs::read(&path).expect("the log outlives the terminal")).into_owned();
    assert!(text.contains("LOGGED-THEN-ENDED"), "{text:?}");
    assert_eq!(bytes, u64::try_from(text.len()).unwrap_or(0), "the count is the whole file, taken before the host ended");

    // Stopping a log whose terminal is gone is fine; a terminal that never logged has no log to report.
    let (on, _, _, _) = log_reply(c.call(&Request::LogStop { session: id.clone() }).unwrap());
    assert!(!on);
    let gone = c.call(&Request::LogStatus { session: "deadbeef".into() }).unwrap();
    assert!(matches!(gone, Response::Error { code, .. } if code == "no_such_session"));
}

#[test]
fn a_restarted_service_finds_a_running_log_and_can_stop_it() {
    let env = Env::new("log-restart");
    let (shared, h) = in_process(&env, "");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    let (on, path, _, _) = log_reply(c.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on);
    let path = path.expect("a path");
    drop(c);
    // The service restarts; the terminal (and its log) keep running in their host.
    shared.stop();
    let _ = h.join();
    // The old service's state (and its connections to the host) must be gone, as after a real restart.
    drop(shared);
    let (_shared, _h) = in_process(&env, "");
    let mut c = env.client();
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo BEFORE-STOP\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "BEFORE-STOP", 2);
    thread::sleep(Duration::from_millis(300));
    // Stop first, with no status or start before it: the restarted service must already know about the log.
    let (on, stopped, _, _) = log_reply(c.call(&Request::LogStop { session: id.clone() }).unwrap());
    assert!(!on && stopped.as_deref() == Some(path.as_str()), "stop reaches the host, not just the service's table");
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo AFTER-STOP\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "AFTER-STOP", 2);
    thread::sleep(Duration::from_millis(300));
    let text = String::from_utf8_lossy(&std::fs::read(&path).expect("read the log")).into_owned();
    assert!(text.contains("BEFORE-STOP") && !text.contains("AFTER-STOP"), "the host really stopped writing: {text:?}");
    end_all(&mut c);
}

#[test]
fn a_shell_that_exits_by_itself_ends_its_log() {
    let env = Env::new("log-exit");
    let (_shared, _h) = in_process(&env, "");
    let mut c = env.client();
    let id = new_terminal(&mut c);
    let (on, path, _, _) = log_reply(c.call(&Request::LogStart { session: id.clone() }).unwrap());
    assert!(on);
    let path = path.expect("a path");
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo LAST-WORDS\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "LAST-WORDS", 2);
    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "exit\r".into() }).unwrap(), Response::Ok));
    // No End: the shell just exits. Status must stop saying "on" by itself.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (bytes, reason) = loop {
        let (on, _, bytes, reason) = log_reply(c.call(&Request::LogStatus { session: id.clone() }).unwrap());
        if !on {
            break (bytes, reason);
        }
        assert!(Instant::now() < deadline, "the log still says on after the shell exited");
        thread::sleep(Duration::from_millis(200));
    };
    assert!(reason.as_ref().is_some_and(|r| r.contains("ended")), "{reason:?}");
    let file = std::fs::read(&path).expect("read the log");
    assert!(String::from_utf8_lossy(&file).contains("LAST-WORDS"));
    assert_eq!(bytes, u64::try_from(file.len()).unwrap_or(0));
    let again = c.call(&Request::LogStart { session: id.clone() }).unwrap();
    assert!(matches!(&again, Response::Error { code, .. } if code == "terminal_ended"), "nothing more can be recorded: {again:?}");
    end_all(&mut c);
}

#[test]
fn the_cli_starts_stops_and_reports_a_log() {
    let env = Env::new("log-cli");
    let (_shared, _h) = in_process(&env, "");
    let mut c = env.client();
    let id = new_terminal(&mut c);

    let (code, out) = wb(&env, &["--json", "log", &id, "start"]);
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.trim()).expect("one JSON object");
    let path = v["path"].as_str().unwrap_or_default().to_owned();
    assert_eq!((v["on"].as_bool(), v["bytes"].as_u64()), (Some(true), Some(0)), "{v}");
    assert!(path.ends_with(".log"), "{path}");

    assert!(matches!(c.call(&Request::Input { session: id.clone(), text: "echo CLI-LOGGED\r".into() }).unwrap(), Response::Ok));
    wait_for(&mut c, &id, "CLI-LOGGED", 2);
    thread::sleep(Duration::from_millis(300));

    let (code, out) = wb(&env, &["--json", "log", &id, "status"]);
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert_eq!((v["on"].as_bool(), v["path"].as_str()), (Some(true), Some(path.as_str())), "{v}");

    let (code, out) = wb(&env, &["--json", "log", &id, "stop"]);
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert_eq!((v["on"].as_bool(), v["path"].as_str()), (Some(false), Some(path.as_str())), "{v}");
    assert!(v["bytes"].as_u64().unwrap_or(0) > 0);
    assert!(v["stopped_reason"].is_null(), "a requested stop has no reason: {v}");

    // Text mode and the exit codes a script branches on.
    let (code, out) = wb(&env, &["log", &id, "status"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("off\t") && out.contains(&path), "{out:?}");
    assert_eq!(wb(&env, &["--json", "log", "deadbeef", "start"]).0, 3, "no such terminal");
    assert_eq!(wb(&env, &["--json", "log", &id, "nonsense"]).0, 64, "usage");
    assert_eq!(wb(&env, &["--json", "log", &id]).0, 64, "usage");
    end_all(&mut c);
}
