//! `Watch`: output pushed to a watcher as it appears, for many sessions over one connection.
//!
//! Runs every case twice: with hosts that signal new output (the normal case) and with hosts that do not (standing in
//! for terminals started before the signal existed, which the broker then polls).
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
    proto::{b64_decode, ClientKind, Request, Response, Shell, WatchCursor, WatchItem},
};

struct Env {
    dir: PathBuf,
    pipe: String,
}
impl Env {
    fn new() -> Env {
        let id = format!("watch-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        Env { dir: std::env::temp_dir().join(format!("wbtest-{id}")), pipe: format!(r"\\.\pipe\wbtest-{id}") }
    }
    fn gui(&self) -> Client {
        Client::connect(&self.pipe, &self.dir, ClientKind::Gui).expect("a client connects")
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
/// The daemon under test; when dropped it first ends every terminal still open (hosts outlive their daemon on purpose, so a
/// test that fails halfway would otherwise leave them running), then stops.
struct Daemon(Child, PathBuf, String);
impl Drop for Daemon {
    fn drop(&mut self) {
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

fn daemon(env: &Env, signals: bool) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wb-daemon"));
    cmd.arg("--data-dir").arg(&env.dir).arg("--pipe-name").arg(&env.pipe).stdout(Stdio::piped()).stderr(Stdio::null());
    if !signals {
        cmd.env("WB_NO_OUTPUT_SIGNAL", "1");
    }
    let mut child = cmd.spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    Daemon(child, env.dir.clone(), env.pipe.clone())
}

fn create(c: &mut Client) -> String {
    let Response::Created { session } = c.call(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: Some(Shell::Cmd), extended_history: None, cwd: None }).unwrap() else { panic!("create") };
    session.id
}

fn cursor(session: &str, from: u64) -> WatchCursor {
    WatchCursor { session: session.into(), from }
}

fn watch(c: &mut Client, cursors: Vec<WatchCursor>, wait_ms: u64) -> (Vec<WatchItem>, Vec<String>) {
    match c.call(&Request::Watch { cursors, wait_ms }).unwrap() {
        Response::Watch { items, gone } => (items, gone),
        other => panic!("watch: {other:?}"),
    }
}

fn text(item: &WatchItem) -> String {
    String::from_utf8_lossy(&b64_decode(&item.data).unwrap_or_default()).into_owned()
}

/// Keeps watching `cursors` (advancing them) until `marker` has appeared for `session`; returns how long it took
/// from `started`.
fn watch_for(c: &mut Client, cursors: &mut [WatchCursor], session: &str, marker: &str, started: Instant) -> Duration {
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (items, _) = watch(c, cursors.to_vec(), 1000);
        for item in items {
            if let Some(cur) = cursors.iter_mut().find(|cur| cur.session == item.session) {
                cur.from = item.next;
            }
            if item.session == session {
                seen.push_str(&text(&item));
                if seen.matches(marker).count() >= 2 {
                    return started.elapsed();
                }
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for {marker:?} in {session}; saw {seen:?}");
    }
}

fn scenario(signals: bool) {
    let env = Env::new();
    let _d = daemon(&env, signals);
    let mut human = env.gui();
    let ids: Vec<String> = (0..5).map(|_| create(&mut human)).collect();
    let mut watcher = env.gui(); // a watcher uses a connection of its own: a Watch keeps it busy while it waits

    // Output that already exists is delivered at once to a cursor that is behind (from 0), for every session.
    let mut cursors: Vec<WatchCursor> = ids.iter().map(|id| cursor(id, 0)).collect();
    let first = Instant::now();
    let (items, gone) = watch(&mut watcher, cursors.clone(), 2000);
    assert!(gone.is_empty());
    assert!(!items.is_empty(), "the shells' banners are waiting");
    assert!(first.elapsed() < Duration::from_millis(1500), "existing output is not waited for");
    // Drain until quiet.
    let drain = Instant::now() + Duration::from_secs(10);
    loop {
        let (items, _) = watch(&mut watcher, cursors.clone(), 400);
        if items.is_empty() {
            break;
        }
        for item in items {
            cursors.iter_mut().find(|c| c.session == item.session).unwrap().from = item.next;
        }
        assert!(Instant::now() < drain, "the banners never stopped");
    }

    // Nothing new: the call waits (about the time asked), then returns empty.
    let idle = Instant::now();
    let (items, gone) = watch(&mut watcher, cursors.clone(), 600);
    assert!(items.is_empty() && gone.is_empty());
    let waited = idle.elapsed();
    assert!(waited >= Duration::from_millis(500) && waited < Duration::from_millis(1500), "an idle watch waits its time: {waited:?}");

    // Typing into one session wakes the watcher with that session only, quickly.
    let target = ids[2].clone();
    let typed = {
        let mut human = env.gui();
        let start = Instant::now();
        human.call(&Request::Input { session: target.clone(), text: "echo PUSHED_OUTPUT\r".into() }).unwrap();
        start
    };
    let mut quiet = cursors.clone();
    let took = watch_for(&mut watcher, &mut quiet, &target, "PUSHED_OUTPUT", typed);
    println!("watch ({}): echo reached the watcher in {took:?}", if signals { "signalled" } else { "polled" });
    assert!(took < Duration::from_millis(1500), "{took:?}");
    for c in &quiet {
        if c.session != target {
            assert!(cursors.iter().find(|o| o.session == c.session).unwrap().from == c.from, "other sessions had nothing new");
        }
    }

    // A flood in one session arrives in order and complete, across several calls.
    human.call(&Request::Input { session: ids[0].clone(), text: "for /L %i in (1,1,400) do @echo flood-line-%i\r".into() }).unwrap();
    let mut got = String::new();
    let mut flood = vec![cursor(&ids[0], cursors.iter().find(|c| c.session == ids[0]).unwrap().from)];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !got.contains("flood-line-400") {
        let (items, _) = watch(&mut watcher, flood.clone(), 1000);
        for item in items {
            assert_eq!(item.from, flood[0].from, "an item starts where the cursor was");
            flood[0].from = item.next;
            got.push_str(&text(&item));
        }
        assert!(Instant::now() < deadline, "flood incomplete");
    }
    // The terminal repeats each line in its title updates; the visible lines (those ending the row) are in order.
    let numbers: Vec<u32> = got
        .split("flood-line-")
        .skip(1)
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect();
    assert!(numbers.windows(2).all(|w| w[0] <= w[1]) && numbers.contains(&1) && numbers.contains(&400), "ordered and complete");

    // An ended session is reported as gone, and the rest keep working.
    human.call(&Request::End { session: ids[4].clone() }).unwrap();
    let (items, gone) = watch(&mut watcher, cursors.clone(), 500);
    assert_eq!(gone, vec![ids[4].clone()], "{items:?}");

    // Bad requests.
    let bad = |c: &mut Client, cursors: Vec<WatchCursor>| match c.call(&Request::Watch { cursors, wait_ms: 10 }).unwrap() {
        Response::Error { code, .. } => code,
        other => format!("not an error: {other:?}"),
    };
    assert_eq!(bad(&mut watcher, vec![]), "invalid_watch");
    assert_eq!(bad(&mut watcher, (0..65).map(|_| cursor(&ids[0], 0)).collect()), "invalid_watch");
    let (items, gone) = watch(&mut watcher, vec![cursor("deadbeef", 0)], 1000);
    assert!(items.is_empty() && gone == vec!["deadbeef".to_owned()], "unknown sessions are reported, not waited on");

    // Agents cannot watch.
    let mut agent = Client::connect_agent(&env.pipe, &env.dir, "claude").unwrap();
    assert_eq!(bad(&mut agent, vec![cursor(&ids[0], 0)]), "forbidden");

    // Clean up.
    for id in &ids[..4] {
        human.call(&Request::End { session: id.clone() }).unwrap();
    }
}

#[test]
fn watch_delivers_output_as_it_appears_for_hosts_that_signal() {
    scenario(true);
}

#[test]
fn watch_still_works_for_hosts_that_do_not_signal_by_polling_them() {
    scenario(false);
}

#[test]
fn a_blocked_watch_does_not_hold_up_other_clients_or_stopping_the_service() {
    let env = Env::new();
    let _d = daemon(&env, true);
    let mut human = env.gui();
    let id = create(&mut human);
    let pipe = env.pipe.clone();
    let dir = env.dir.clone();
    let id2 = id.clone();
    // A watcher waiting on a quiet session for the longest allowed time.
    let waiting = thread::spawn(move || {
        let mut c = Client::connect(&pipe, &dir, ClientKind::Gui).unwrap();
        let (items, _) = watch(&mut c, vec![cursor(&id2, u64::MAX / 2)], 2000);
        items.len()
    });
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    assert!(matches!(human.call(&Request::List {}).unwrap(), Response::Sessions { .. }));
    assert!(started.elapsed() < Duration::from_millis(500), "other clients are not blocked: {:?}", started.elapsed());
    assert_eq!(waiting.join().unwrap(), 0);
    human.call(&Request::End { session: id }).unwrap();
}

/// Everything a viewer sees of `id` from the start, read to the end of what exists now.
fn whole_stream(c: &mut Client, id: &str) -> Vec<u8> {
    let (mut bytes, mut from) = (Vec::new(), 0u64);
    loop {
        let Response::Output { data, next, .. } = c.call(&Request::Read { session: id.into(), from }).unwrap() else { panic!("read") };
        bytes.extend_from_slice(&b64_decode(&data).unwrap_or_default());
        if next == from {
            return bytes;
        }
        from = next;
    }
}

#[test]
fn two_viewers_of_one_terminal_see_the_same_bytes_and_neither_can_hold_the_other_up() {
    let env = Env::new();
    let _d = daemon(&env, true);
    let mut human = env.gui();
    let id = create(&mut human);
    // Viewer A watches and then stops reading altogether; viewer B keeps watching.
    let mut a = env.gui();
    let mut b = env.gui();
    let mut a_cursor = vec![cursor(&id, 0)];
    let mut b_cursor = vec![cursor(&id, 0)];
    watch(&mut a, a_cursor.clone(), 300);
    // A flood while A is stalled: B must still receive all of it promptly.
    human.call(&Request::Input { session: id.clone(), text: "for /L %i in (1,1,600) do @echo two-viewers-%i\r".into() }).unwrap();
    let started = Instant::now();
    let mut seen_b = String::new();
    while !seen_b.contains("two-viewers-600") {
        let (items, gone) = watch(&mut b, b_cursor.clone(), 1000);
        assert!(gone.is_empty());
        for item in items {
            b_cursor[0].from = item.next;
            seen_b.push_str(&text(&item));
        }
        assert!(started.elapsed() < Duration::from_secs(20), "B was held up by a stalled A: {:?}", started.elapsed());
    }
    // A comes back and catches up from where it stopped: nothing was lost for it, and the result is byte-identical.
    let mut seen_a = String::new();
    while !seen_a.contains("two-viewers-600") {
        let (items, _) = watch(&mut a, a_cursor.clone(), 1000);
        for item in items {
            a_cursor[0].from = item.next;
            seen_a.push_str(&text(&item));
        }
        assert!(started.elapsed() < Duration::from_secs(30), "A could not catch up");
    }
    // They stopped at different points (each as soon as it saw the last line), so one stream is a prefix of the other.
    let common = seen_a.len().min(seen_b.len());
    assert!(common > 5_000, "both saw the flood");
    assert!(seen_a.as_bytes()[..common] == seen_b.as_bytes()[..common], "two viewers read the same stream (differs within the first {common} bytes)");
    // A viewer that vanishes mid-stream does not disturb the other, and a new viewer replays exactly the same bytes.
    drop(a);
    human.call(&Request::Input { session: id.clone(), text: "echo after-a-left\r".into() }).unwrap();
    let mut tail = String::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while tail.matches("after-a-left").count() < 2 {
        let (items, _) = watch(&mut b, b_cursor.clone(), 1000);
        for item in items {
            b_cursor[0].from = item.next;
            tail.push_str(&text(&item));
        }
        assert!(Instant::now() < deadline, "B stopped receiving after A left: {tail:?}");
    }
    let mut late = env.gui();
    thread::sleep(Duration::from_millis(300));
    let replay = whole_stream(&mut late, &id);
    let mut other = env.gui();
    assert!(whole_stream(&mut other, &id) == replay, "any viewer sees the same history");

    // Two viewers that both resize: the last one wins, and everyone is told the same size.
    assert!(matches!(a_resize(&mut human, &id, 90, 25), Response::Ok));
    assert!(matches!(a_resize(&mut late, &id, 130, 40), Response::Ok));
    for c in [&mut human, &mut other] {
        let Response::Sessions { items } = c.call(&Request::List {}).unwrap() else { panic!("list") };
        let s = items.iter().find(|s| s.id == id).unwrap();
        assert_eq!((s.cols, s.rows), (130, 40), "the most recent resize is the size");
    }
    human.call(&Request::End { session: id }).unwrap();
}

fn a_resize(c: &mut Client, id: &str, cols: u16, rows: u16) -> Response {
    c.call(&Request::Resize { session: id.into(), cols, rows }).unwrap()
}
