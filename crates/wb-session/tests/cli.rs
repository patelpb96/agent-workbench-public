//! The `wb` command line as a script or an agent uses it: JSON output, exit codes, wait, watch, grants and drafts.
//! Spec: docs/design/wb-automation.md. Every case runs the real `wb` program against a real broker and real shells.
#![cfg(windows)]
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new() -> Env {
        let id = format!("cli-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}") }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
/// The daemon under test; when dropped it first ends every terminal still open (hosts outlive their daemon on purpose),
/// then stops.
struct Daemon(Child, PathBuf, String);
impl Drop for Daemon {
    fn drop(&mut self) {
        use wb_session::{client::Client, proto::{ClientKind, Request, Response}};
        if let Ok(mut c) = Client::connect(&self.2, &self.1, ClientKind::Gui) {
            if let Ok(Response::Sessions { items }) = c.call(&Request::List {}) {
                for s in items {
                    let _ = c.call(&Request::End { session: s.id });
                }
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn daemon(env: &Env) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wb-daemon")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    Daemon(child, env.dir.clone(), env.pipe.clone())
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}
impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {:?}", self.stdout))
    }
    fn error_code(&self) -> String {
        let v: Value = serde_json::from_str(self.stderr.trim()).unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {:?}", self.stderr));
        v["error"]["code"].as_str().unwrap_or_default().to_owned()
    }
}

fn wb_cmd(env: &Env, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wb"));
    cmd.arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).args(args);
    cmd
}
fn wb(env: &Env, args: &[&str]) -> Out {
    let o = wb_cmd(env, args).stdin(Stdio::null()).output().unwrap();
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

fn new_terminal(env: &Env) -> String {
    let out = wb(env, &["--json", "new", "--shell", "cmd"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    out.json()["id"].as_str().unwrap().to_owned()
}

#[test]
fn json_output_and_exit_codes_for_the_basic_commands() {
    let env = Env::new();
    let _d = daemon(&env);
    // Nothing yet.
    let ls = wb(&env, &["--json", "ls"]);
    assert_eq!((ls.code, ls.json()), (0, serde_json::json!({ "sessions": [] })));

    let created = wb(&env, &["--json", "new", "build", "--shell", "cmd"]);
    assert_eq!(created.code, 0, "{}", created.stderr);
    let v = created.json();
    let id = v["id"].as_str().unwrap().to_owned();
    assert_eq!(v["session"]["label"], "build");
    assert_eq!(v["session"]["state"], "running");
    assert_eq!(wb(&env, &["new", "--shell", "cmd"]).stdout.trim().len(), 8, "text mode prints just the id");

    let ls = wb(&env, &["--json", "ls"]).json();
    assert_eq!(ls["sessions"].as_array().unwrap().len(), 2);
    assert!(ls["sessions"].as_array().unwrap().iter().any(|s| s["id"] == id.as_str() && s["cols"] == 120));

    // send reports where the output stood; wait for what the command prints; read it back.
    let sent = wb(&env, &["--json", "send", &id, "echo", "hello-%OS%-marker"]);
    assert_eq!(sent.code, 0, "{}", sent.stderr);
    let from = sent.json()["from"].as_u64().expect("send reports the position before the text went in");
    let waited = wb(&env, &["--json", "wait", &id, "--from", &from.to_string(), "--for", "Windows_NT-marker", "--timeout", "20"]);
    assert_eq!(waited.code, 0, "{}", waited.stderr);
    let w = waited.json();
    assert_eq!(w["reason"], "text");
    assert!(w["next"].as_u64().unwrap() > from);
    let read = wb(&env, &["--json", "read", &id, &from.to_string()]).json();
    assert!(read["text"].as_str().unwrap().contains("hello-Windows_NT-marker"));
    assert!(!read["data"].as_str().unwrap().is_empty());
    // Text mode: the output on stdout, the position on stderr.
    let plain = wb(&env, &["read", &id, &from.to_string()]);
    assert!(plain.stdout.contains("Windows_NT-marker") && plain.stderr.contains("next="));

    // Codes: no such terminal, usage, unreachable, timeouts.
    let missing = wb(&env, &["--json", "read", "deadbeef"]);
    assert_eq!((missing.code, missing.error_code().as_str()), (3, "no_such_session"));
    assert_eq!(wb(&env, &["--json", "end", "deadbeef"]).code, 3);
    assert_eq!(wb(&env, &["--json", "nonsense"]).code, 64);
    assert_eq!(wb(&env, &["--json", "wait", &id]).code, 64, "wait needs something to wait for");
    assert_eq!(wb(&env, &["--json", "grant", &id]).code, 64, "grant needs --to");
    assert_eq!(wb(&env, &["--json", "new", "--shell", "tcsh"]).code, 64);
    let started = Instant::now();
    let timed_out = wb(&env, &["--json", "wait", &id, "--for", "NEVER-PRINTED", "--timeout", "1"]);
    assert_eq!((timed_out.code, timed_out.error_code().as_str()), (4, "timed_out"));
    assert!(started.elapsed() < Duration::from_secs(6));
    let idle = wb(&env, &["--json", "wait", &id, "--idle", "1", "--timeout", "20"]);
    assert_eq!(idle.code, 0);
    assert_eq!(idle.json()["reason"], "idle");
    let nowhere = Command::new(env!("CARGO_BIN_EXE_wb")).args(["--data-dir"]).arg(&env.dir).args(["--pipe-name", r"\\.\pipe\wbtest-nothing-here", "--json", "ls"]).output().unwrap();
    assert_eq!(nowhere.status.code(), Some(2));

    // End it: the second end finds nothing.
    assert_eq!(wb(&env, &["end", &id]).code, 0);
    assert_eq!(wb(&env, &["end", &id]).code, 3);
}

#[test]
fn wait_for_the_terminal_to_end_and_watch_a_terminal_that_ends() {
    let env = Env::new();
    let _d = daemon(&env);
    let id = new_terminal(&env);
    // `exit` ends the shell; `wait --exit` returns success with reason "ended".
    let waiter = {
        let mut cmd = wb_cmd(&env, &["--json", "wait", &id, "--exit", "--timeout", "30"]);
        thread::spawn(move || cmd.stdin(Stdio::null()).output().unwrap())
    };
    thread::sleep(Duration::from_millis(400));
    assert_eq!(wb(&env, &["send", &id, "exit"]).code, 0);
    let out = waiter.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"reason\":\"ended\""));
    // Ending an already-exited terminal and then watching it: the terminal is gone, exit 6.
    assert_eq!(wb(&env, &["end", &id]).code, 0);
    let watched = wb(&env, &["--json", "watch", &id, "--from", "start"]);
    assert_eq!(watched.code, 3, "a terminal that never existed or is already gone cannot be watched: {}", watched.stderr);

    // Watching one that ends while it watches: exit 6.
    let id = new_terminal(&env);
    let mut watcher = wb_cmd(&env, &["--json", "watch", &id]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(wb(&env, &["end", &id]).code, 0);
    let status = watcher.wait().unwrap();
    assert_eq!(status.code(), Some(6));
}

#[test]
fn watch_streams_raw_output_for_one_terminal_and_json_lines_for_several() {
    let env = Env::new();
    let _d = daemon(&env);
    let (a, b) = (new_terminal(&env), new_terminal(&env));
    // Several terminals need --json.
    assert_eq!(wb(&env, &["watch", &a, &b]).code, 64);

    // One terminal, raw: start the watcher, then cause output.
    let mut raw = wb_cmd(&env, &["watch", &a]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    {
        let (seen, stdout) = (seen.clone(), raw.stdout.take().unwrap());
        thread::spawn(move || {
            use std::io::Read;
            let mut stdout = stdout;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
    }
    thread::sleep(Duration::from_millis(600));
    assert_eq!(wb(&env, &["send", &a, "echo", "RAW-STREAM-1"]).code, 0);
    let deadline = Instant::now() + Duration::from_secs(15);
    while seen.lock().unwrap().matches("RAW-STREAM-1").count() < 2 {
        assert!(Instant::now() < deadline, "no raw output: {:?}", seen.lock().unwrap());
        thread::sleep(Duration::from_millis(50));
    }
    let _ = raw.kill();
    let _ = raw.wait();

    // Two terminals, JSON lines: each line names its session.
    let mut lines = wb_cmd(&env, &["--json", "watch", &a, &b]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut reader = BufReader::new(lines.stdout.take().unwrap());
    thread::sleep(Duration::from_millis(600));
    assert_eq!(wb(&env, &["send", &b, "echo", "LINES-FOR-B"]).code, 0);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got = String::new();
    while !got.contains("LINES-FOR-B\\r\\n") && !got.matches("LINES-FOR-B").count().eq(&2) {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "watch closed early: {got:?}");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["session"], b.as_str(), "only the terminal that produced output appears");
        assert!(v["next"].as_u64().unwrap() >= v["from"].as_u64().unwrap());
        got.push_str(v["text"].as_str().unwrap());
        assert!(Instant::now() < deadline, "no JSON lines: {got:?}");
    }
    let _ = lines.kill();
    let _ = lines.wait();
    wb(&env, &["end", &a]);
    wb(&env, &["end", &b]);
}

#[test]
fn an_agent_proposes_a_person_accepts_grants_and_the_agent_types_waits_and_loses_access() {
    let env = Env::new();
    let _d = daemon(&env);
    let id = new_terminal(&env);
    let agent = |args: &[&str]| {
        let mut all = vec!["--agent", "claude"];
        all.extend_from_slice(args);
        wb(&env, &all)
    };

    // Without a grant the agent cannot type, read or watch; a grant is not something it can give itself.
    let denied = agent(&["--json", "type", &id, "echo", "NO"]);
    assert_eq!((denied.code, denied.error_code().as_str()), (5, "no_grant"));
    assert_eq!(agent(&["--json", "read", &id]).code, 5);
    assert_eq!(agent(&["--json", "grant", &id, "--to", "claude"]).code, 5);
    assert_eq!(agent(&["--json", "accept", &id]).code, 5);

    // It can propose; the person sees the draft and accepts it.
    assert_eq!(agent(&["propose", &id, "echo", "PROPOSED-%OS%-BY-AGENT"]).code, 0);
    let draft = wb(&env, &["--json", "draft", &id]).json();
    assert_eq!((draft["draft"]["agent"].as_str(), draft["draft"]["text"].as_str()), (Some("claude"), Some("echo PROPOSED-%OS%-BY-AGENT")));
    assert_eq!(wb(&env, &["accept", &id]).code, 0);
    assert_eq!(wb(&env, &["send", &id, ""]).code, 0); // Enter, which the draft deliberately did not press
    let seen = wb(&env, &["--json", "wait", &id, "--from", "start", "--for", "PROPOSED-Windows_NT-BY-AGENT", "--timeout", "30"]);
    assert_eq!(seen.code, 0, "{}", seen.stderr);
    assert_eq!(wb(&env, &["--json", "draft", &id]).json()["draft"], Value::Null);

    // The person grants 15 minutes; the agent types, and waits for the answer by polling (it may not watch).
    let granted = wb(&env, &["--json", "grant", &id, "--to", "claude", "--for", "15m", "--keep"]);
    assert_eq!(granted.code, 0, "{}", granted.stderr);
    let grants = wb(&env, &["--json", "grants", &id]).json();
    let g = &grants["grants"][0];
    assert_eq!(g["agent"], "claude");
    assert!(g["remaining_ms"].as_u64().unwrap() > 14 * 60_000);
    let typed = agent(&["--json", "type", &id, "echo", "AGENT-TYPED-%OS%"]);
    assert_eq!(typed.code, 0, "{}", typed.stderr);
    let answer = agent(&["--json", "wait", &id, "--from", "start", "--for", "AGENT-TYPED-Windows_NT", "--timeout", "20"]);
    assert_eq!(answer.code, 0, "an agent with a grant can wait: {}", answer.stderr);
    assert_eq!(answer.json()["reason"], "text");

    // Revoked: the agent is out again.
    assert_eq!(wb(&env, &["revoke", &id, "--to", "claude"]).code, 0);
    assert_eq!(agent(&["--json", "type", &id, "echo", "LATE"]).code, 5);
    assert_eq!(wb(&env, &["--json", "grants", &id]).json()["grants"].as_array().unwrap().len(), 0);
    assert_eq!(wb(&env, &["end", &id]).code, 0);
}
