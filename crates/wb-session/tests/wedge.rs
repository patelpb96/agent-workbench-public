//! The product's central story, end to end, in one scenario (real broker, real host, real shell, real child processes,
//! the real `wb attach`):
//!
//! 1. A terminal runs "agent work": a background process that keeps writing a heartbeat file, plus a long-running worker.
//! 2. The GUI is closed: the work keeps going. The broker is restarted too: it still keeps going.
//! 3. The GUI is reopened and attaches live: the history is replayed and new output follows; typing works; detaching leaves it running.
//! 4. An external agent proposes a command; the human accepts it; the human grants the agent 15 minutes; the agent types.
//!    With no human attached the grant does not let the agent type; when a human is back, it does again.
//! 5. The terminal is ended: the host, the shell, the agent process and the worker are all provably gone and the work stops.
//!
//! Each step of the story has narrower tests elsewhere; this one exists so the whole claim is proven together.
#![cfg(windows)]
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use wb_session::{
    client::Client,
    proto::{b64_decode, ClientKind, Duration as GrantFor, Request, Response, Shell},
    win::{kill_pid, pid_running},
};

struct Env {
    dir: PathBuf,
    pipe: String,
    ticks: PathBuf,
}
impl Env {
    fn new() -> Env {
        let id = format!("wedge-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}"), ticks: std::env::temp_dir().join(format!("wbtest-{id}-ticks.txt")) }
    }
    fn gui(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Gui).expect("the GUI connects")
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.ticks);
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
/// Kills these processes when dropped, so a failing test never leaves work running.
#[derive(Default)]
struct Pids(Vec<u32>);
impl Drop for Pids {
    fn drop(&mut self) {
        for pid in &self.0 {
            kill_pid(*pid);
        }
    }
}

fn daemon(env: &Env) -> (Daemon, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wb-daemon")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    (Daemon(child), line)
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

fn wait_until(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// The number printed after `label=` (ignoring the echo of the typed command, where the label is followed by `$`).
fn number_after(text: &str, label: &str) -> Option<u32> {
    text.match_indices(label).find_map(|(at, _)| {
        let digits: String = text[at + label.len()..].chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

fn ticks(env: &Env) -> u64 {
    std::fs::metadata(&env.ticks).map(|m| m.len()).unwrap_or(0)
}
fn heartbeat_grows(env: &Env, within: Duration) -> bool {
    let before = ticks(env);
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if ticks(env) > before {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// `wb attach` as a real process with piped stdio.
struct Attached {
    child: Child,
    out: Arc<Mutex<String>>,
}
impl Attached {
    fn start(env: &Env, id: &str) -> Attached {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wb")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).arg("attach").arg(id).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        let (sink, mut stdout) = (out.clone(), child.stdout.take().unwrap());
        thread::spawn(move || {
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
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }
    fn text(&self) -> String {
        self.out.lock().unwrap().clone()
    }
}
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn code_of(r: &Response) -> String {
    match r {
        Response::Error { code, .. } => code.clone(),
        other => format!("not an error: {other:?}"),
    }
}

#[test]
fn the_whole_wedge_close_the_gui_the_agent_keeps_working_reopen_attach_propose_grant_and_a_provable_teardown() {
    let env = Env::new();
    let mut cleanup = Pids::default();

    // ---- 1. a terminal running agent work -------------------------------------------------------------------------
    let (d1, ready) = daemon(&env);
    assert!(ready.starts_with("READY"), "{ready}");
    let mut gui = env.gui();
    let Response::Created { session } = gui.call(&Request::NewTerminal { cols: 120, rows: 30, label: Some("agent".into()), shell: Some(Shell::PowerShell), extended_history: None, cwd: None }).unwrap() else { panic!("create") };
    let (id, host) = (session.id, session.host_pid);
    cleanup.0.push(host);
    // The "agent": a hidden PowerShell that appends a heartbeat every 300 ms, and a long-running worker. Both are children
    // of the terminal's shell, so both belong to the terminal's process tree.
    let ticks_path = env.ticks.display().to_string().replace('\'', "''");
    let work = format!(
        "$a = Start-Process powershell -ArgumentList '-NoProfile','-Command','while($true){{ Add-Content -LiteralPath ''{ticks_path}'' -Value (Get-Date -Format o); Start-Sleep -Milliseconds 300 }}' -WindowStyle Hidden -PassThru; \"AGENT_PID=$($a.Id)\"; $w = Start-Process ping -ArgumentList '-t','127.0.0.1' -WindowStyle Hidden -PassThru; \"WORKER_PID=$($w.Id)\"\r"
    );
    assert!(matches!(gui.call(&Request::Input { session: id.clone(), text: work }).unwrap(), Response::Ok));
    let (mut agent_pid, mut worker_pid) = (0, 0);
    wait_until("the agent and worker to start", 30, || {
        let text = output(&mut gui, &id);
        agent_pid = number_after(&text, "AGENT_PID=").unwrap_or(0);
        worker_pid = number_after(&text, "WORKER_PID=").unwrap_or(0);
        agent_pid != 0 && worker_pid != 0
    });
    cleanup.0.extend([agent_pid, worker_pid]);
    assert!(pid_running(agent_pid) && pid_running(worker_pid), "the agent ({agent_pid}) and worker ({worker_pid}) are running");
    assert!(heartbeat_grows(&env, Duration::from_secs(10)), "the agent is writing its heartbeat");
    println!("1. terminal {id}: host {host}, agent process {agent_pid}, worker {worker_pid}, heartbeat file growing");

    // ---- 2. the GUI is closed; then the broker is restarted too ----------------------------------------------------
    drop(gui);
    thread::sleep(Duration::from_millis(500));
    assert!(heartbeat_grows(&env, Duration::from_secs(5)), "the agent keeps working with no GUI connected");
    kill_pid(d1.pid());
    drop(d1);
    thread::sleep(Duration::from_millis(500));
    assert!(heartbeat_grows(&env, Duration::from_secs(5)), "the agent keeps working with the broker stopped");
    assert!(pid_running(host) && pid_running(agent_pid) && pid_running(worker_pid));
    println!("2. GUI closed, broker stopped: the work is still running");
    let (_d2, ready2) = daemon(&env);
    assert!(ready2.contains("sessions=1") && ready2.contains("lost=0"), "the new broker re-adopted the running terminal: {ready2}");

    // ---- 3. reopen the GUI and attach live --------------------------------------------------------------------------
    let mut gui = env.gui();
    let Response::Sessions { items } = gui.call(&Request::List {}).unwrap() else { panic!("list") };
    let listed = items.iter().find(|s| s.id == id).expect("the terminal is listed after the restart");
    assert_eq!((listed.state.as_str(), listed.host_pid), ("running", host));
    assert!(output(&mut gui, &id).contains("AGENT_PID="), "the history survived: what happened before the GUI closed is still there");
    println!("3. broker restarted and re-adopted the terminal; GUI reopened; attaching with wb attach");
    let mut attached = Attached::start(&env, &id);
    wait_until("the history to be replayed by wb attach", 15, || attached.text().contains("AGENT_PID="));
    attached.send(b"echo ATTACHED_LIVE\r");
    wait_until("typing through wb attach to work", 15, || attached.text().matches("ATTACHED_LIVE").count() >= 2);
    attached.send(&[0x1d]); // Ctrl+] detaches
    wait_until("wb attach to detach", 15, || attached.child.try_wait().ok().flatten().is_some());
    assert!(pid_running(host) && pid_running(agent_pid), "detaching leaves the terminal and the agent running");
    assert!(heartbeat_grows(&env, Duration::from_secs(5)), "and still working");

    // ---- 4. an agent proposes, the human accepts and grants 15 minutes ----------------------------------------------
    println!("   detached; the work is still running");
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").expect("the agent connects with its restricted identity");
    assert_eq!(code_of(&agent.call(&Request::AgentInput { session: id.clone(), text: "echo NO_GRANT_YET\r".into() }).unwrap()), "no_grant", "an agent cannot type without a grant");
    assert!(matches!(agent.call(&Request::Propose { session: id.clone(), text: "echo AGENT_PROPOSED".into() }).unwrap(), Response::Ok));
    let Response::Draft { draft: Some(draft) } = gui.call(&Request::GetDraft { session: id.clone() }).unwrap() else { panic!("the human sees the draft") };
    assert_eq!((draft.agent.as_str(), draft.text.as_str()), ("claude", "echo AGENT_PROPOSED"));
    assert!(!output(&mut gui, &id).contains("AGENT_PROPOSED\r\n"), "nothing is typed until the human accepts");
    assert!(matches!(gui.call(&Request::AcceptDraft { session: id.clone() }).unwrap(), Response::Ok));
    gui.call(&Request::Input { session: id.clone(), text: "\r".into() }).unwrap();
    wait_until("the accepted draft to run", 15, || output(&mut gui, &id).matches("AGENT_PROPOSED").count() >= 2);

    assert!(matches!(gui.call(&Request::GrantCreate { agent: "claude".into(), session: id.clone(), duration: GrantFor::FifteenMinutes, keep_after_detach: false, gate: String::new() }).unwrap(), Response::Ok));
    let Response::Grants { items } = gui.call(&Request::Grants { session: id.clone() }).unwrap() else { panic!("grants") };
    assert!(items.len() == 1 && items[0].remaining_ms > 14 * 60_000 && items[0].remaining_ms <= 15 * 60_000, "a 15 minute grant: {items:?}");
    assert!(matches!(agent.call(&Request::AgentInput { session: id.clone(), text: "echo AGENT_TYPED\r".into() }).unwrap(), Response::Ok));
    wait_until("the agent's typing to arrive", 15, || output(&mut gui, &id).matches("AGENT_TYPED").count() >= 2);

    println!("4. proposed, accepted, 15 minute grant, agent typed");
    // With the human gone the grant does not let the agent type; when a human is back, it does again.
    drop(gui);
    wait_until("the grant to stop working without a human", 10, || code_of(&agent.call(&Request::AgentInput { session: id.clone(), text: "echo UNATTENDED\r".into() }).unwrap()) == "no_grant");
    let mut gui = env.gui();
    assert!(matches!(agent.call(&Request::AgentInput { session: id.clone(), text: "echo BACK_ATTENDED\r".into() }).unwrap(), Response::Ok), "a human is attached again, the grant is still within its 15 minutes");
    wait_until("the agent's typing to arrive again", 15, || output(&mut gui, &id).matches("BACK_ATTENDED").count() >= 2);
    assert!(!output(&mut gui, &id).contains("UNATTENDED\r\n"), "what was refused was never typed");

    // ---- 5. the terminal is ended: a provable teardown of the whole tree ---------------------------------------------
    println!("   grant is dormant without a human and live again with one");
    assert!(matches!(gui.call(&Request::End { session: id.clone() }).unwrap(), Response::Ok), "End reports a proven teardown");
    wait_until("every process of the terminal to be gone", 15, || !pid_running(host) && !pid_running(agent_pid) && !pid_running(worker_pid));
    let settled = ticks(&env);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(ticks(&env), settled, "the agent's work really stopped");
    let Response::Sessions { items } = gui.call(&Request::List {}).unwrap() else { panic!("list") };
    assert!(items.iter().all(|s| s.id != id), "the session is no longer listed");
    let log = std::fs::read_to_string(env.dir.join("events.log")).unwrap_or_default();
    assert!(log.contains(&format!("ended {id} by=gui proof=true")), "the audit log records a proven teardown: {log}");
    println!("5. ended: host, shell, agent and worker are gone, the heartbeat stopped, the log records proof=true");
    cleanup.0.clear();
}
