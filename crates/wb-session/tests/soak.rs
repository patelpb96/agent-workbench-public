//! Soak: the broker is hammered the way real use would, for a while, and must not leak.
//!
//! Each round: a "GUI" connects, creates a terminal, types a burst of output, reads some of it and is then dropped
//! WITHOUT ending anything (the app being killed mid-read); a second connection ends the terminal; connections that
//! fail the handshake are made too. Meanwhile one long-lived terminal keeps being read. After a warm-up the broker's
//! handle count and private memory are sampled; at the end they may not have grown beyond a small allowance, the
//! broker must still answer, and no host process may be left behind.
//!
//! Default length is short (about 25 s) so that it runs in `check-all`. For an overnight run:
//! `set WB_SOAK_SECS=28800` and start the compiled `soak-*.exe` by hand (see crates/wb-session/README.md).
#![cfg(windows)]
// Test code written before clippy gained this lint: `round % N == 0` is what it means and what it does.
#![allow(clippy::manual_is_multiple_of)]
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::{GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};
use wb_session::{
    client::Client,
    proto::{b64_decode, ClientKind, Request, Response, Shell},
    win::{alive, creation_time, pid_running},
};

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new() -> Env {
        let id = format!("soak-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}") }
    }
    fn gui(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Gui).expect("the GUI connects")
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// (handle count, private bytes) of a process.
fn usage(pid: u32) -> (u32, u64) {
    // SAFETY: plain Win32 queries on a handle opened and closed here; the out-structures are local and correctly sized.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        assert!(!h.is_null(), "cannot open the broker for measurement");
        let mut handles = 0u32;
        assert!(GetProcessHandleCount(h, &mut handles) != 0);
        let mut mem: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        mem.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        assert!(GetProcessMemoryInfo(h, std::ptr::addr_of_mut!(mem).cast(), mem.cb) != 0);
        CloseHandle(h);
        // The working set is a sanity figure for the private bytes (a private figure far above it would mean a misread).
        assert!(mem.PrivateUsage as u64 >= (mem.WorkingSetSize as u64) / 8, "implausible memory reading");
        (handles, mem.PrivateUsage as u64)
    }
}

fn create(c: &mut Client) -> (String, u32) {
    match c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: Some(Shell::Cmd), extended_history: None, cwd: None }).unwrap() {
        Response::Created { session } => (session.id, session.host_pid),
        other => panic!("create: {other:?}"),
    }
}

fn read_all(c: &mut Client, id: &str) -> String {
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

#[test]
fn the_broker_does_not_leak_handles_or_memory_under_repeated_connect_create_flood_drop_and_end() {
    let secs: u64 = std::env::var("WB_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    let env = Env::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wb-daemon")).arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    let daemon = Daemon(child);
    let pid = daemon.0.id();

    let mut keeper = env.gui();
    let (keeper_id, keeper_host) = create(&mut keeper);
    let secret = wb_session::win::load_secret(&env.dir, false).unwrap();
    // Only recent hosts are kept for the final "none left behind" check, each with its start time: over hours a pid is
    // reused by other programs (the first overnight run flagged a browser that had been given an old host's pid).
    let mut hosts: std::collections::VecDeque<(u32, u64)> = std::collections::VecDeque::new();
    let mut round = 0u32;
    let mut last_report = Instant::now();
    // For hunting a slow leak: `WB_SOAK_MODE=handshake` only connects and fails handshakes; `sessions` also creates and
    // ends terminals without any output; anything else is the full mix. `WB_SOAK_REPORT_SECS` sets the progress interval.
    let mode = std::env::var("WB_SOAK_MODE").unwrap_or_default();
    let report_every: u64 = std::env::var("WB_SOAK_REPORT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut baseline = None;
    let started = Instant::now();
    let warmup = Duration::from_secs((secs / 4).max(3));
    while started.elapsed() < Duration::from_secs(secs) {
        round += 1;
        if mode == "handshake" {
            drop(env.gui());
            assert!(Client::connect_as(&env.pipe, &[7u8; 32], ClientKind::Gui, None).is_err());
            assert!(Client::connect_as(&env.pipe, &secret, ClientKind::Agent, None).is_err());
            if baseline.is_none() && started.elapsed() >= warmup {
                baseline = Some(usage(pid));
            }
            if last_report.elapsed() >= Duration::from_secs(report_every) {
                last_report = Instant::now();
                let (h, b) = usage(pid);
                println!("soak progress: {} s, {round} rounds, broker handles {h}, private bytes {} KiB", started.elapsed().as_secs(), b >> 10);
            }
            continue;
        }
        // The app starts, creates a terminal and floods it.
        let mut gui = env.gui();
        let (id, host) = create(&mut gui);
        hosts.push_back((host, creation_time(host).unwrap_or(0)));
        if hosts.len() > 300 {
            hosts.pop_front();
        }
        if mode != "sessions" {
            gui.call(&Request::Input { session: id.clone(), text: "for /L %i in (1,1,300) do @echo soak-line-%i\r".into() }).unwrap();
            thread::sleep(Duration::from_millis(150));
            let _ = read_all(&mut gui, &id);
        }
        // ...and is killed mid-use: no End, no goodbye.
        drop(gui);
        // Another connection tidies up some rounds and leaves others for the end of the test.
        if round % 2 == 0 {
            let mut other = env.gui();
            assert!(matches!(other.call(&Request::End { session: id.clone() }).unwrap(), Response::Ok), "round {round}: End");
        }
        // Terminals dropped without End pile up in a long run: sweep all but the long-lived one now and then (a person
        // would end their terminals eventually; this keeps the live count bounded to a few dozen).
        if round % 25 == 0 {
            let mut sweeper = env.gui();
            if let Ok(Response::Sessions { items }) = sweeper.call(&Request::List {}) {
                for s in items.iter().filter(|s| s.id != keeper_id) {
                    let _ = sweeper.call(&Request::End { session: s.id.clone() });
                }
            }
        }
        // Handshakes that go wrong: a wrong secret, and an agent with no name.
        assert!(Client::connect_as(&env.pipe, &[7u8; 32], ClientKind::Gui, None).is_err());
        assert!(Client::connect_as(&env.pipe, &secret, ClientKind::Agent, None).is_err());
        // The long-lived terminal keeps working.
        keeper.call(&Request::Input { session: keeper_id.clone(), text: format!("echo keeper-{round}\r") }).unwrap();
        if baseline.is_none() && started.elapsed() >= warmup {
            baseline = Some(usage(pid));
        }
        // A long run prints a line a minute, so the trend can be read afterwards (handles and memory of the broker).
        // Sampled right after a sweep (every 25th round) so the live terminal count is the same each time: a trend in these
        // numbers is the broker's own growth, not terminals that happen to be alive.
        if round % 25 == 0 && last_report.elapsed() >= Duration::from_secs(report_every) {
            last_report = Instant::now();
            let (h, b) = usage(pid);
            println!("soak progress: {} s, {round} rounds, broker handles {h}, private bytes {} KiB", started.elapsed().as_secs(), b >> 10);
        }
    }
    let baseline = baseline.expect("the soak ran past its warm-up");
    thread::sleep(Duration::from_millis(500));
    assert!(read_all(&mut keeper, &keeper_id).contains(&format!("keeper-{round}")), "the long-lived terminal still works");

    // End everything that is left, then measure.
    let Response::Sessions { items } = env.gui().call(&Request::List {}).unwrap() else { panic!("list") };
    for s in items.iter().filter(|s| s.id != keeper_id) {
        env.gui().call(&Request::End { session: s.id.clone() }).unwrap();
    }
    thread::sleep(Duration::from_secs(1));
    let (handles, bytes) = usage(pid);
    let (h0, b0) = baseline;
    println!("soak: {round} rounds in {secs}s; broker handles {h0} -> {handles}; private bytes {} KiB -> {} KiB", b0 >> 10, bytes >> 10);
    // Ending everything must leave no more than the baseline plus a small allowance (the warm-up already held live sessions).
    assert!(handles <= h0 + 40, "handle leak: {h0} -> {handles}");
    assert!(bytes <= b0 + (24 << 20), "memory growth: {} MiB -> {} MiB", b0 >> 20, bytes >> 20);
    assert!(matches!(env.gui().call(&Request::List {}).unwrap(), Response::Sessions { .. }), "the broker still answers");
    let strays: Vec<u32> = hosts.iter().copied().filter(|(pid, started)| alive(*pid, *started)).map(|(pid, _)| pid).collect();
    // End the long-lived terminal first, so a failed assertion below does not leave its host running.
    assert!(pid_running(keeper_host));
    env.gui().call(&Request::End { session: keeper_id }).unwrap();
    assert!(strays.is_empty(), "host processes left behind: {strays:?}");
}
