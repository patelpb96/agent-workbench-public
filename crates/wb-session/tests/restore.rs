//! Restoring terminals that did not survive a restart (docs/design/restart-restore.md). A restart is simulated the way Windows
//! does it: every host and the service are killed, then a new service starts on the same data folder.
#![cfg(windows)]
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wb_session::{
    client::Client,
    proto::{b64_decode, ClientKind, Request, Response, Shell},
    win::kill_pid,
};

struct Env {
    dir: PathBuf,
    pipe: String,
    work: PathBuf,
}
impl Env {
    fn new() -> Env {
        let id = format!("restore-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}"), work: std::env::temp_dir().join(format!("wbtest-{id}-work")) }
    }
    fn gui(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Gui).expect("a client connects")
    }
    fn folder(&self, name: &str) -> PathBuf {
        let p = self.work.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

/// A service that is stopped abruptly; nothing is ended first (that is the point: it is a restart).
struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(env: &Env) -> (Service, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wb-daemon")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    (Service(child), line)
}

fn lost_ids(c: &mut Client) -> Vec<(String, String)> {
    let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!("list") };
    items.into_iter().filter(|s| s.state == "lost").map(|s| (s.id, s.label)).collect()
}

fn output(c: &mut Client, id: &str) -> String {
    let (mut text, mut from) = (String::new(), 0u64);
    loop {
        let Response::Output { data, next, .. } = c.call(&Request::Read { session: id.into(), from }).unwrap() else { panic!("read") };
        text.push_str(&String::from_utf8_lossy(&b64_decode(&data).unwrap_or_default()));
        if next == from {
            return text;
        }
        from = next;
    }
}

fn wait_for(c: &mut Client, id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !output(c, id).contains(needle) {
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; saw {:?}", output(c, id));
        thread::sleep(Duration::from_millis(100));
    }
}

fn code_of(r: &Response) -> String {
    match r {
        Response::Error { code, .. } => code.clone(),
        other => format!("not an error: {other:?}"),
    }
}

#[test]
fn terminals_lost_in_a_restart_come_back_under_their_old_ids_with_their_old_shape() {
    let env = Env::new();
    let (first, _) = start(&env);
    let mut gui = env.gui();
    let kept = env.folder("kept folder");
    let doomed = env.folder("doomed");
    let make = |gui: &mut Client, label: &str, cols: u16, rows: u16, extended: bool, cwd: &PathBuf| {
        let Response::Created { session } = gui.call(&Request::NewTerminal { cols, rows, label: Some(label.into()), shell: Some(Shell::Cmd), extended_history: Some(extended), cwd: Some(cwd.display().to_string()) }).unwrap() else { panic!("create") };
        session
    };
    let a = make(&mut gui, "build", 110, 33, true, &kept);
    let b = make(&mut gui, "second", 90, 25, false, &doomed);

    // The restart: every host and the service go; a new service starts on the same data folder.
    drop(gui);
    kill_pid(a.host_pid);
    kill_pid(b.host_pid);
    drop(first);
    std::fs::remove_dir_all(&doomed).unwrap(); // and one of the start folders is gone by the time of the restore
    let (_second, ready) = start(&env);
    assert!(ready.contains("lost=2"), "both terminals are remembered as lost: {ready}");
    let mut gui = env.gui();
    let mut lost = lost_ids(&mut gui);
    lost.sort();
    let mut expected = vec![(a.id.clone(), "build".to_owned()), (b.id.clone(), "second".to_owned())];
    expected.sort();
    assert_eq!(lost, expected);

    // An agent connection cannot restore anything; unknown and running ids are refused.
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").unwrap();
    assert_eq!(code_of(&agent.call(&Request::Restore { session: a.id.clone(), shell: None }).unwrap()), "forbidden");
    assert_eq!(code_of(&gui.call(&Request::Restore { session: "deadbeef".into(), shell: None }).unwrap()), "no_such_session");

    // Restore the first: same id, label, size and history size; its shell starts in the same folder.
    let Response::Created { session } = gui.call(&Request::Restore { session: a.id.clone(), shell: None }).unwrap() else { panic!("restore") };
    assert_eq!((session.id.as_str(), session.label.as_str(), session.cols, session.rows, session.history_mib, session.state.as_str()), (a.id.as_str(), "build", 110, 33, 32, "running"));
    wait_for(&mut gui, &a.id, "kept folder");
    // It is a working terminal again.
    gui.call(&Request::Input { session: a.id.clone(), text: "echo RESTORED-OK\r".into() }).unwrap();
    wait_for(&mut gui, &a.id, "RESTORED-OK");
    // It is no longer lost, so a second restore of it is refused; the other is still waiting.
    assert_eq!(code_of(&gui.call(&Request::Restore { session: a.id.clone(), shell: None }).unwrap()), "no_such_session");
    assert_eq!(lost_ids(&mut gui), vec![(b.id.clone(), "second".to_owned())]);

    // The second one's folder is gone: it comes back in the user's folder instead of failing.
    let Response::Created { session } = gui.call(&Request::Restore { session: b.id.clone(), shell: None }).unwrap() else { panic!("restore") };
    assert_eq!((session.cols, session.rows, session.history_mib), (90, 25, 1));
    wait_for(&mut gui, &b.id, &std::env::var("USERPROFILE").unwrap());
    assert!(lost_ids(&mut gui).is_empty());

    // A restored terminal is recorded like any other: lose it again and it can be restored again, folder and all.
    let hosts: Vec<u32> = {
        let Response::Sessions { items } = gui.call(&Request::List {}).unwrap() else { panic!("list") };
        items.iter().map(|s| s.host_pid).collect()
    };
    drop(gui);
    for pid in hosts {
        kill_pid(pid);
    }
    drop(_second);
    let (_third, ready) = start(&env);
    assert!(ready.contains("lost=2"), "{ready}");
    let mut gui = env.gui();
    assert!(matches!(gui.call(&Request::Restore { session: a.id.clone(), shell: None }).unwrap(), Response::Created { .. }));
    wait_for(&mut gui, &a.id, "kept folder");
    for id in [&a.id, &b.id] {
        let _ = gui.call(&Request::End { session: id.clone() });
    }
}

#[test]
fn a_registry_written_by_an_older_broker_is_still_read_and_its_lost_terminals_can_be_restored() {
    let env = Env::new();
    std::fs::create_dir_all(&env.dir).unwrap();
    // 7 fields (no history size), 8 fields (live), 9 fields (marked lost): the formats older brokers wrote. Hosts that do not exist.
    std::fs::write(env.dir.join("sessions.txt"), "aaaa0001;seven;999991;1;nonce001;100;30\nbbbb0002;eight;999992;1;nonce002;120;40;32\ncccc0003;nine;999993;1;nonce003;80;24;1;lost\n").unwrap();
    let (_svc, ready) = start(&env);
    assert!(ready.contains("lost=3"), "{ready}");
    let mut gui = env.gui();
    let Response::Sessions { items } = gui.call(&Request::List {}).unwrap() else { panic!("list") };
    let find = |id: &str| items.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("{id} listed"));
    assert_eq!((find("aaaa0001").label.as_str(), find("aaaa0001").history_mib), ("seven", 1));
    assert_eq!((find("bbbb0002").cols, find("bbbb0002").rows, find("bbbb0002").history_mib), (120, 40, 32));
    // Restoring one with no recorded shell or folder gives a cmd terminal in the user's folder.
    let Response::Created { session } = gui.call(&Request::Restore { session: "bbbb0002".into(), shell: None }).unwrap() else { panic!("restore") };
    assert_eq!((session.id.as_str(), session.cols, session.rows, session.history_mib), ("bbbb0002", 120, 40, 32));
    wait_for(&mut gui, "bbbb0002", &std::env::var("USERPROFILE").unwrap());
    for id in ["aaaa0001", "bbbb0002", "cccc0003"] {
        let _ = gui.call(&Request::End { session: id.into() });
    }
}

#[test]
fn wb_restore_all_brings_back_every_lost_terminal_and_reports_what_it_did() {
    let env = Env::new();
    let (first, _) = start(&env);
    let mut gui = env.gui();
    let mut hosts = Vec::new();
    let mut ids = Vec::new();
    for label in ["one", "two"] {
        let Response::Created { session } = gui.call(&Request::NewTerminal { cols: 100, rows: 30, label: Some(label.into()), shell: Some(Shell::Cmd), extended_history: None, cwd: None }).unwrap() else { panic!("create") };
        hosts.push(session.host_pid);
        ids.push(session.id);
    }
    drop(gui);
    for pid in hosts {
        kill_pid(pid);
    }
    drop(first);
    let (_second, _) = start(&env);
    let wb = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_wb")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).args(args).stdin(Stdio::null()).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let (code, out, err) = wb(&["--json", "restore", "--all"]);
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let mut restored: Vec<String> = v["restored"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned()).collect();
    restored.sort();
    ids.sort();
    assert_eq!(restored, ids);
    assert!(v["failed"].as_array().unwrap().is_empty());
    // Nothing left to restore: a second run does nothing and succeeds; one id that is not lost is refused with the "no such" code.
    let (code, out, _) = wb(&["--json", "restore", "--all"]);
    assert_eq!(code, 0);
    assert!(serde_json::from_str::<serde_json::Value>(out.trim()).unwrap()["restored"].as_array().unwrap().is_empty());
    assert_eq!(wb(&["--json", "restore", &ids[0]]).0, 3);
    for id in &ids {
        wb(&["end", id]);
    }
}

#[test]
fn a_restore_can_name_the_shell_for_a_terminal_whose_shell_was_never_recorded() {
    let env = Env::new();
    std::fs::create_dir_all(&env.dir).unwrap();
    std::fs::write(env.dir.join("sessions.txt"), "dddd0004;legacy agent;999994;1;nonce004;100;30;1\n").unwrap();
    let (_svc, _) = start(&env);
    let mut gui = env.gui();
    let Response::Created { .. } = gui.call(&Request::Restore { session: "dddd0004".into(), shell: Some(Shell::PowerShell) }).unwrap() else { panic!("restore") };
    // A PowerShell prompt, not cmd's.
    wait_for(&mut gui, "dddd0004", "PS ");
    let _ = gui.call(&Request::End { session: "dddd0004".into() });
}
