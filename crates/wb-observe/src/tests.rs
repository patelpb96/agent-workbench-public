//! Golden tests over SYNTHETIC transcripts that mirror the observed structure.
//! Every text field below holds a canary ("SECRET...") that must never surface.
use super::*;
use std::{fs, io::Write, path::PathBuf};

const T0: &str = "2026-10-21T10:00:00.000Z";
fn ms(s: &str) -> u64 {
    time::parse_iso_ms(s).unwrap()
}
fn feed(adapter: &mut dyn Adapter, lines: &[&str]) -> Vec<Event> {
    lines.iter().flat_map(|l| adapter.parse_line(l)).collect()
}
fn replay(adapter: &mut dyn Adapter, lines: &[&str]) -> Index {
    let mut index = Index::new();
    for e in feed(adapter, lines) {
        index.apply(&e);
    }
    index
}

// ---- Claude --------------------------------------------------------------

fn claude_lines() -> Vec<String> {
    vec![
        r#"{"type":"user","timestamp":"2026-10-21T10:00:00.000Z","sessionId":"s","message":{"role":"user","content":"SECRET prompt"}}"#.into(),
        // One assistant message split over two lines shares its id and usage: counted once.
        r#"{"type":"assistant","timestamp":"2026-10-21T10:00:02.000Z","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"SECRET"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"SECRET"}}],"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":5,"output_tokens_details":{"thinking_tokens":7}}}}"#.into(),
        r#"{"type":"assistant","timestamp":"2026-10-21T10:00:02.100Z","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Read","input":{}}],"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":5}}}"#.into(),
        r#"{"type":"user","timestamp":"2026-10-21T10:00:05.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"SECRET out","is_error":false}]},"permissionDecision":{"decision":"accept","source":"config","reasonType":"mode"}}"#.into(),
        r#"{"type":"user","timestamp":"2026-10-21T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"SECRET out","is_error":true}]}}"#.into(),
        r#"{"type":"assistant","timestamp":"2026-10-21T10:00:09.000Z","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"SECRET done"}],"usage":{"input_tokens":10,"output_tokens":2}}}"#.into(),
        r#"{"type":"user","timestamp":"2026-10-21T10:01:09.000Z","message":{"role":"user","content":"SECRET next"}}"#.into(),
    ]
}

#[test]
fn claude_tools_usage_permissions_and_dedupe() {
    let lines = claude_lines();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut a = ClaudeAdapter::new();
    let events = feed(&mut a, &refs);
    assert!(events.iter().any(|e| matches!(e, Event::SessionStarted { at_ms: Some(t) } if *t == ms(T0))));
    let usages: Vec<_> = events.iter().filter(|e| matches!(e, Event::Usage { .. })).collect();
    assert_eq!(usages.len(), 2, "m1 counted once, m2 once");
    assert!(events.iter().any(|e| matches!(e, Event::Permission { decision, call_id: Some(c), .. } if decision == "accept" && c == "t1")));
    // Never leaks transcript text.
    assert!(!format!("{events:?}").contains("SECRET"));
    let mut index = Index::new();
    for e in &events {
        index.apply(e);
    }
    let bash = index.tool("Bash").unwrap();
    assert_eq!((bash.calls, bash.completed, bash.errors), (1, 1, 0));
    assert_eq!(bash.total_ms, 3_000);
    assert_eq!(bash.last_used_ms, Some(ms("2026-10-21T10:00:02.000Z")));
    let read = index.tool("Read").unwrap();
    assert_eq!((read.calls, read.errors), (1, 1));
    assert_eq!(index.totals().input, Some(110));
    assert_eq!(index.totals().reasoning, Some(7));
    assert_eq!(index.totals().cache_write, Some(5));
    assert_eq!(index.turns(), 2);
    assert_eq!(index.open_calls(), 0);
}

#[test]
fn claude_sidechain_user_lines_do_not_open_turns() {
    let mut a = ClaudeAdapter::new();
    let ev = a.parse_line(r#"{"type":"user","isSidechain":true,"timestamp":"2026-10-21T10:00:00Z","message":{"role":"user","content":"SECRET"}}"#);
    assert!(!ev.iter().any(|e| matches!(e, Event::TurnStarted { .. })));
}

// ---- Pi ------------------------------------------------------------------

#[test]
fn pi_uses_the_sources_own_duration_and_outcome() {
    let lines = [
        r#"{"type":"session","version":3,"id":"x","timestamp":"2026-10-21T10:00:00.000Z","cwd":"SECRET"}"#,
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-21T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"SECRET"}]}}"#,
        r#"{"type":"message","id":"b","timestamp":"2026-10-21T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"SECRET"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"SECRET"}}],"usage":{"input":10,"output":5,"cacheRead":3,"cacheWrite":0,"reasoning":2,"totalTokens":18}}}"#,
        r#"{"type":"message","id":"c","timestamp":"2026-10-21T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"SECRET"}],"isError":false,"durationMs":750}}"#,
    ];
    let mut a = PiAdapter::new();
    let events = feed(&mut a, &lines);
    assert!(!format!("{events:?}").contains("SECRET"));
    let mut index = Index::new();
    events.iter().for_each(|e| index.apply(e));
    let bash = index.tool("bash").unwrap();
    // Source duration (750) wins over the 1000 ms timestamp difference.
    assert_eq!(bash.total_ms, 750);
    assert_eq!(index.totals().cache_write, Some(0));
    assert_eq!(index.totals().input, Some(10));
}

#[test]
fn pi_orphan_result_still_counts_when_it_names_its_tool() {
    let mut a = PiAdapter::new();
    let ev = a.parse_line(r#"{"type":"message","timestamp":"2026-10-21T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"zz","toolName":"edit","isError":true,"durationMs":5}}"#);
    let mut index = Index::new();
    ev.iter().for_each(|e| index.apply(e));
    assert_eq!(index.orphan_results, 1);
    let edit = index.tool("edit").unwrap();
    assert_eq!((edit.calls, edit.completed, edit.errors), (0, 1, 1));
}

// ---- Codex ---------------------------------------------------------------

#[test]
fn codex_pairs_calls_and_reports_usage_without_inventing_outcomes() {
    let lines = [
        r#"{"timestamp":"2026-10-21T10:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"id":"s","cwd":"SECRET"}}"#,
        r#"{"timestamp":"2026-10-21T10:00:01.000Z","ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#,
        r#"{"timestamp":"2026-10-21T10:00:02.000Z","ordinal":2,"type":"response_item","payload":{"type":"function_call","call_id":"k1","name":"shell","input":"SECRET"}}"#,
        r#"{"timestamp":"2026-10-21T10:00:04.500Z","ordinal":3,"type":"response_item","payload":{"type":"function_call_output","call_id":"k1","output":[{"type":"text","text":"SECRET"}]}}"#,
        r#"{"timestamp":"2026-10-21T10:00:05.000Z","ordinal":4,"type":"token_usage_record","payload":{"usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":0,"output_tokens":9,"reasoning_output_tokens":3,"total_tokens":109},"thread_token_usage":{"input_tokens":999}}}"#,
        r#"{"timestamp":"2026-10-21T10:00:06.000Z","ordinal":5,"type":"event_msg","payload":{"type":"task_complete","turn_id":"t"}}"#,
    ];
    let mut a = CodexAdapter::new();
    let events = feed(&mut a, &lines);
    assert!(!format!("{events:?}").contains("SECRET"));
    let mut index = Index::new();
    events.iter().for_each(|e| index.apply(e));
    let shell = index.tool("shell").unwrap();
    assert_eq!(shell.total_ms, 2_500);
    assert_eq!(shell.unknown_outcome, 1, "Codex records no tool outcome; never guessed");
    assert_eq!(index.totals().input, Some(100), "per-response usage, not the cumulative thread total");
    assert_eq!(index.totals().cache_read, Some(40));
    assert_eq!(index.state(ms("2026-10-21T10:00:07.000Z"), 600_000), SessionState::WaitingForHuman);
}

// ---- Index ---------------------------------------------------------------

#[test]
fn breakdown_separates_model_human_and_tool_time() {
    let lines = claude_lines();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let index = replay(&mut ClaudeAdapter::new(), &refs);
    let b = index.breakdown();
    assert_eq!(b.tool_ms, 3_000 + 900, "t1 3 s (10:00:02 to 10:00:05), t2 0.9 s (10:00:02.1 to 10:00:03)");
    assert!(b.model_ms >= 2_000, "user prompt to first assistant/tool start");
    assert_eq!(b.human_ms, 60_000, "assistant final message to next user prompt");
}

#[test]
fn state_is_heuristic_and_idle_overrides() {
    let mut index = Index::new();
    assert_eq!(index.state(0, 1000), SessionState::Empty);
    index.apply(&Event::ToolStarted { call_id: "a".into(), tool: "x".into(), at_ms: Some(10_000) });
    assert_eq!(index.state(11_000, 60_000), SessionState::Working);
    assert_eq!(index.state(100_000, 60_000), SessionState::Idle);
    index.apply(&Event::ToolEnded { call_id: "a".into(), tool: None, at_ms: Some(12_000), ok: Some(true), duration_ms: None });
    index.apply(&Event::Message { role: Role::Assistant, at_ms: Some(13_000) });
    assert_eq!(index.state(14_000, 60_000), SessionState::WaitingForHuman);
}

#[test]
fn bounds_hold_for_tools_open_calls_and_recent_rows() {
    let mut index = Index::new();
    for i in 0..(index::MAX_OPEN_CALLS + 10) {
        index.apply(&Event::ToolStarted { call_id: format!("c{i}"), tool: "t".into(), at_ms: Some(i as u64) });
    }
    assert_eq!(index.open_calls(), index::MAX_OPEN_CALLS);
    assert_eq!(index.dropped_open, 10);
    for i in 0..(index::MAX_RECENT_CALLS + 5) {
        let id = format!("r{i}");
        index.apply(&Event::ToolStarted { call_id: id.clone(), tool: "t".into(), at_ms: Some(1) });
        index.apply(&Event::ToolEnded { call_id: id, tool: None, at_ms: Some(2), ok: Some(true), duration_ms: Some(1) });
    }
    assert_eq!(index.recent_calls().count(), index::MAX_RECENT_CALLS);
    assert!(index.tool("t").unwrap().sampled() <= index::MAX_SAMPLES_PER_TOOL);
    for i in 0..(index::MAX_TOOLS + 20) {
        index.apply(&Event::ToolStarted { call_id: format!("x{i}"), tool: format!("tool{i}"), at_ms: Some(1) });
    }
    assert!(index.tools().len() <= index::MAX_TOOLS + 1);
}

#[test]
fn labels_are_bounded_and_sanitized() {
    let mut a = PiAdapter::new();
    let name = format!("bad\\u0007name{}", "x".repeat(500));
    let line = format!(r#"{{"type":"message","timestamp":"2026-10-21T10:00:01Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"c","name":"{name}"}}]}}}}"#);
    let ev = a.parse_line(&line);
    let Event::ToolStarted { tool, .. } = &ev[0] else { panic!("no tool event") };
    assert!(tool.chars().count() <= event::MAX_LABEL && !tool.contains('\u{7}'));
}

#[test]
fn garbage_and_unknown_lines_yield_nothing() {
    for l in ["", "not json", "{}", r#"{"type":"mystery"}"#, r#"{"type":"message"}"#] {
        assert!(ClaudeAdapter::new().parse_line(l).is_empty());
        assert!(PiAdapter::new().parse_line(l).is_empty());
        assert!(CodexAdapter::new().parse_line(l).is_empty());
    }
}

// ---- Tailer --------------------------------------------------------------

fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wb-observe-{}-{}", std::process::id(), name));
    let _ = fs::remove_file(&dir);
    dir
}
fn append(p: &PathBuf, bytes: &[u8]) {
    fs::OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(bytes).unwrap();
}

#[test]
fn tailer_waits_for_complete_lines_and_resumes_by_offset() {
    let p = temp_file("partial");
    let lines = claude_lines();
    let first = &lines[0];
    let second = &lines[1];
    append(&p, first.as_bytes());
    append(&p, b"\n");
    append(&p, &second.as_bytes()[..40]); // partial line
    let mut o = Observer::new(AgentKind::Claude, &p);
    o.poll().unwrap();
    assert_eq!(o.index.events, 3, "session start, turn start and user message only");
    append(&p, &second.as_bytes()[40..]);
    let r = o.poll().unwrap();
    assert_eq!(r.lines, 0, "still no newline");
    append(&p, b"\r\n");
    let r = o.poll().unwrap();
    assert_eq!(r.lines, 1);
    assert!(o.index.tool("Bash").is_some());
    let before = o.index.events;
    assert_eq!(o.poll().unwrap().bytes, 0);
    assert_eq!(o.index.events, before);
    let _ = fs::remove_file(&p);
}

#[test]
fn tailer_rebuilds_after_truncation_without_double_counting() {
    let p = temp_file("truncate");
    let lines = claude_lines();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&p, &body).unwrap();
    let mut o = Observer::new(AgentKind::Claude, &p);
    o.poll().unwrap();
    let calls = o.index.tool("Bash").unwrap().calls;
    fs::write(&p, format!("{}\n", lines[0])).unwrap(); // shorter file replaces it
    let r = o.poll().unwrap();
    assert!(r.reset);
    assert!(o.index.tool("Bash").is_none(), "state rebuilt from the new, shorter file");
    assert_eq!(calls, 1);
    let _ = fs::remove_file(&p);
}

#[test]
fn tailer_skips_oversized_lines_and_keeps_going() {
    let p = temp_file("oversize");
    let mut huge = vec![b'x'; tail::MAX_LINE + 10];
    huge.push(b'\n');
    append(&p, &huge);
    let lines = claude_lines();
    append(&p, format!("{}\n", lines[0]).as_bytes());
    let mut o = Observer::new(AgentKind::Claude, &p);
    o.poll().unwrap();
    assert_eq!(o.tailer().oversized_lines, 1);
    assert!(o.index.events >= 3, "the line after the oversized one was still parsed");
    let _ = fs::remove_file(&p);
}

#[test]
fn replaying_the_file_reproduces_the_index() {
    let p = temp_file("replay");
    let lines = claude_lines();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&p, &body).unwrap();
    let mut a = Observer::new(AgentKind::Claude, &p);
    a.poll().unwrap();
    let mut b = Observer::new(AgentKind::Claude, &p);
    b.poll().unwrap();
    assert_eq!(a.index.totals(), b.index.totals());
    assert_eq!(a.index.breakdown(), b.index.breakdown());
    let _ = fs::remove_file(&p);
}

#[test]
fn snapshot_is_bounded_serializable_and_text_free() {
    let lines = claude_lines();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut index = replay(&mut ClaudeAdapter::new(), &refs);
    let snap = snapshot(&index, ms("2026-10-21T10:01:10.000Z"), 600_000);
    assert_eq!(snap.state, "working", "the last event is a user prompt");
    assert_eq!(snap.turns, 2);
    let bash = snap.tools.iter().find(|t| t.name == "Bash").unwrap();
    assert_eq!((bash.calls, bash.total_ms, bash.errors), (1, 3_000, 0));
    assert_eq!(snap.recent.len(), 2);
    assert_eq!(snap.recent[0].tool, "Read", "most recent call first");
    let json = serde_json::to_string(&snap).unwrap();
    assert!(!json.contains("SECRET"));
    assert_eq!(snapshot(&index, ms("2026-10-21T12:00:00.000Z"), 600_000).state, "idle");
    // Bounds: tool rows are capped and the remainder is counted.
    for i in 0..(snapshot::MAX_TOOL_ROWS + 7) {
        index.apply(&Event::ToolStarted { call_id: format!("z{i}"), tool: format!("tool{i}"), at_ms: Some(1) });
    }
    let big = snapshot(&index, 0, 1);
    assert_eq!(big.tools.len(), snapshot::MAX_TOOL_ROWS);
    assert!(big.tools_omitted >= 7);
}

#[test]
fn timeline_buckets_calls_by_start_time_within_bounds() {
    let mut index = Index::new();
    assert!(snapshot::timeline(&index).is_none());
    let base = ms("2026-10-21T10:00:00.000Z");
    // Three calls in minute 0, one error in minute 5.
    for (i, (offset, ok)) in [(0u64, true), (10_000, true), (50_000, true), (5 * 60_000 + 5_000, false)].into_iter().enumerate() {
        let id = format!("c{i}");
        index.apply(&Event::ToolStarted { call_id: id.clone(), tool: "t".into(), at_ms: Some(base + offset) });
        index.apply(&Event::ToolEnded { call_id: id, tool: None, at_ms: Some(base + offset + 1_000), ok: Some(ok), duration_ms: Some(1_000) });
    }
    let t = snapshot::timeline(&index).unwrap();
    assert_eq!(t.start_ms, base);
    assert_eq!(t.bucket_ms, 60_000);
    assert_eq!(t.buckets.len(), 6);
    assert_eq!((t.buckets[0].calls, t.buckets[0].tool_ms), (3, 3_000));
    assert_eq!((t.buckets[5].calls, t.buckets[5].errors), (1, 1));
    // A long span widens the buckets instead of exceeding the cap.
    for i in 0..10 {
        let id = format!("far{i}");
        let at = base + (i + 1) * 3 * 3_600_000;
        index.apply(&Event::ToolStarted { call_id: id.clone(), tool: "t".into(), at_ms: Some(at) });
        index.apply(&Event::ToolEnded { call_id: id, tool: None, at_ms: Some(at + 10), ok: Some(true), duration_ms: Some(10) });
    }
    let wide = snapshot::timeline(&index).unwrap();
    assert!(wide.buckets.len() <= snapshot::MAX_BUCKETS && wide.bucket_ms > 60_000 && wide.bucket_ms % 60_000 == 0);
    assert_eq!(wide.buckets.iter().map(|b| b.calls).sum::<u64>(), 14, "every retained call lands in a bucket");
}

// ---- Claude subagent metadata (line 1 only) --------------------------------

fn write_first_line(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wb-observe-sub-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent-x.jsonl");
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn subagent_meta_reads_the_parent_and_agent_from_line_one_and_never_the_message() {
    let ok = r#"{"type":"user","isSidechain":true,"agentId":"a1b2c3","sessionId":"0a1b2c3d-1111-2222-3333-444455556666","cwd":"C:/x","message":{"role":"user","content":"SECRET prompt"}}"#;
    let path = write_first_line("valid", &format!("{ok}\n{{\"type\":\"assistant\"}}\n"));
    let meta = link::subagent_meta(&path).unwrap();
    assert_eq!((meta.parent_id.as_str(), meta.agent_id.as_str()), ("0a1b2c3d-1111-2222-3333-444455556666", "a1b2c3"));
    assert!(!format!("{meta:?}").contains("SECRET"), "the message text never surfaces");
    // Without a trailing newline (a file still being written) the line is still read.
    assert!(link::subagent_meta(&write_first_line("nonl", ok)).is_some());
}

#[test]
fn subagent_meta_refuses_anything_that_is_not_a_clear_sidechain_header() {
    let cases = [
        ("absent", r#"{"type":"user","agentId":"a1","sessionId":"s1","message":{}}"#.to_string()),
        ("false", r#"{"isSidechain":false,"agentId":"a1","sessionId":"s1"}"#.to_string()),
        ("no-agent", r#"{"isSidechain":true,"sessionId":"s1"}"#.to_string()),
        ("no-session", r#"{"isSidechain":true,"agentId":"a1"}"#.to_string()),
        ("empty-id", r#"{"isSidechain":true,"agentId":"","sessionId":"s1"}"#.to_string()),
        ("unsafe-id", r#"{"isSidechain":true,"agentId":"a/../b","sessionId":"s1"}"#.to_string()),
        ("not-json", "this is not json".to_string()),
        ("plain-message", r#"{"type":"user","message":{"role":"user","content":"hello"}}"#.to_string()),
        ("oversized", format!("{{\"isSidechain\":true,\"agentId\":\"a1\",\"sessionId\":\"s1\",\"pad\":\"{}\"}}", "x".repeat(70 * 1024))),
    ];
    for (name, line) in cases {
        assert!(link::subagent_meta(&write_first_line(name, &format!("{line}\n"))).is_none(), "{name}");
    }
    assert!(link::subagent_meta(&std::env::temp_dir().join("wb-observe-sub-missing-file.jsonl")).is_none());
}

// ---- roster edges for the agent map ---------------------------------------------

fn roster_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wb-observe-roster-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn roster_file(dir: &std::path::Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}
fn agent(session: &str, reports_to: Option<&str>) -> String {
    match reports_to {
        Some(m) => format!("{{\"sessionId\":\"{session}\",\"name\":\"SECRET name\",\"role\":\"SECRET role\",\"reportsTo\":\"{m}\",\"sessions\":[\"x\"]}}"),
        None => format!("{{\"sessionId\":\"{session}\",\"name\":\"SECRET name\",\"reportsTo\":null}}"),
    }
}

#[test]
fn rosters_return_manager_report_pairs_and_nothing_else() {
    let dir = roster_dir("valid");
    roster_file(&dir, "a.json", &format!("{{\"run\":\"r\",\"title\":\"SECRET\",\"agents\":[{},{},{}],\"events\":[1,2]}}", agent("boss", None), agent("w1", Some("boss")), agent("w2", Some("boss"))));
    let edges = map::rosters(&dir);
    assert_eq!(edges, vec![("boss".to_string(), "w1".to_string()), ("boss".to_string(), "w2".to_string())]);
    assert!(!format!("{edges:?}").contains("SECRET"));
    assert!(map::rosters(&dir.join("missing")).is_empty());
}

#[test]
fn rosters_drop_self_edges_duplicates_unsafe_ids_and_odd_entries_but_keep_cycles_for_the_model() {
    let dir = roster_dir("odd");
    let agents = [
        agent("a", Some("b")),
        agent("b", Some("a")),            // a cycle: both edges are returned, the map model breaks it
        agent("a", Some("b")),            // duplicate
        agent("self", Some("self")),      // self edge
        agent("../x", Some("a")),         // unsafe id
        agent("ok", Some("")),            // empty manager
        "{\"sessionId\":7,\"reportsTo\":\"a\"}".to_string(), // not a string
        "{\"reportsTo\":\"a\"}".to_string(),                 // no session
        agent("c", Some("a")),
    ]
    .join(",");
    roster_file(&dir, "r.json", &format!("{{\"agents\":[{agents}]}}"));
    assert_eq!(map::rosters(&dir), vec![("b".to_string(), "a".to_string()), ("a".to_string(), "b".to_string()), ("a".to_string(), "c".to_string())]);
}

#[test]
fn rosters_skip_oversized_and_malformed_files_and_read_only_the_newest_thirty_two() {
    let dir = roster_dir("limits");
    roster_file(&dir, "big.json", &format!("{{\"agents\":[{}],\"pad\":\"{}\"}}", agent("w", Some("boss")), "x".repeat(70 * 1024)));
    roster_file(&dir, "bad.json", "{ this is not json");
    roster_file(&dir, "wrong-shape.json", "[1,2,3]");
    roster_file(&dir, "notes.txt", &format!("{{\"agents\":[{}]}}", agent("t", Some("boss"))));
    assert!(map::rosters(&dir).is_empty(), "nothing usable in oversized, malformed, wrong-shaped or non-json files");
    for i in 0..40 {
        roster_file(&dir, &format!("f{i:02}.json"), &format!("{{\"agents\":[{}]}}", agent(&format!("w{i:02}"), Some("boss"))));
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    let edges = map::rosters(&dir);
    assert_eq!(edges.len(), map::MAX_FILES, "at most 32 files");
    assert_eq!(edges[0].1, "w39", "newest first");
    assert!(edges.iter().all(|(_, r)| r.as_str() >= "w08"), "the eight oldest were not read");
}

#[test]
fn rosters_stop_at_the_edge_cap() {
    let dir = roster_dir("cap");
    let agents: Vec<String> = (0..300).map(|i| agent(&format!("w{i}"), Some("boss"))).collect();
    roster_file(&dir, "one.json", &format!("{{\"agents\":[{}]}}", agents.join(",")));
    assert_eq!(map::rosters(&dir).len(), map::MAX_EDGES);
}

// ---- tools running right now (for the map) ---------------------------------------

#[test]
fn open_tool_names_lists_each_running_tool_once_and_forgets_it_when_the_result_arrives() {
    let mut index = Index::new();
    for (id, tool) in [("c1", "Bash"), ("c2", "Read"), ("c3", "Bash")] {
        index.apply(&Event::ToolStarted { call_id: id.into(), tool: tool.into(), at_ms: Some(1_000) });
    }
    assert_eq!(index.open_tool_names(8), vec!["Bash".to_string(), "Read".to_string()]);
    assert_eq!(index.open_tool_names(1), vec!["Bash".to_string()], "limit applies");
    index.apply(&Event::ToolEnded { call_id: "c2".into(), tool: None, at_ms: Some(2_000), ok: Some(true), duration_ms: Some(1_000) });
    assert_eq!(index.open_tool_names(8), vec!["Bash".to_string()]);
    assert!(Index::new().open_tool_names(8).is_empty());
}
