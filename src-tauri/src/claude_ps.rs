//! Which Claude Code sessions are running right now, from `claude agents --json` (a read-only listing that
//! takes about 0.2 s). Bounded in time and size; failure reads as "nothing known", and the answer is cached
//! for a few seconds so the inspector list and an "open in terminal" request do not each start a process.
//! Only the process started here is ever stopped (if it hangs).
use agent_workbench::agent_terminals::{parse_claude_agent_list, ClaudeLive};
use std::{
    collections::HashMap,
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_WAIT: Duration = Duration::from_secs(6);
const MAX_OUTPUT: u64 = 1 << 20;
/// How long an answer is reused.
const FRESH: Duration = Duration::from_secs(4);

static CACHE: Mutex<Option<(Instant, HashMap<String, ClaudeLive>)>> = Mutex::new(None);

fn run() -> HashMap<String, ClaudeLive> {
    let Ok(mut child) = Command::new("claude").args(["agents", "--json"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn() else {
        return HashMap::new();
    };
    let Some(out) = child.stdout.take() else { return HashMap::new() };
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.take(MAX_OUTPUT).read_to_string(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < MAX_WAIT => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    parse_claude_agent_list(&reader.join().unwrap_or_default())
}

/// The running sessions by full session id (cached for a few seconds; `fresh` skips the cache).
pub fn running(fresh: bool) -> HashMap<String, ClaudeLive> {
    if let Ok(cache) = CACHE.lock() {
        if let Some((at, map)) = cache.as_ref() {
            if !fresh && at.elapsed() < FRESH {
                return map.clone();
            }
        }
    }
    let map = run();
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), map.clone()));
    }
    map
}

/// One session's state (always asks anew: it decides what a click does).
pub fn live(session_id: &str) -> ClaudeLive {
    running(true).remove(session_id).unwrap_or(ClaudeLive::NotRunning)
}
