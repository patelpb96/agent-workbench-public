//! A look at the END of a transcript: the last line the agent wrote and the model it used.
//!
//! This is the one place the observer reads message TEXT, so it is kept apart from the index and the
//! snapshot (which stay free of transcript text) and bounded hard: only the end of the file (the last 256 KiB, widened to 1 and 4 MiB only when it holds no assistant text)
//! are read, only the last non-empty line of the last assistant message is kept, control characters are
//! removed and the line is cut at `MAX_LINE_CHARS`. Nothing is stored beyond what the caller keeps; the
//! caller decides where the line is shown. Read-only.
use crate::{event, tail::AgentKind};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// How much of the end of a transcript is searched: the smallest window first, a larger one only when it held no
/// assistant text (a long run of tool calls can push the last text far back). Beyond the last, "no line".
pub const WINDOWS: [u64; 3] = [256 * 1024, 1024 * 1024, 4 * 1024 * 1024];
pub const MAX_LINE_CHARS: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peek {
    /// The last non-empty line of the last assistant message, cleaned and bounded.
    pub last_line: Option<String>,
    /// The model named by the most recent assistant message (or turn context), as a short label.
    pub model: Option<String>,
    /// The first line of the most recent message the person sent (cleaned and bounded), or the assistant's last line
    /// when the transcript has no user text. Used as the one-line task name.
    pub task: Option<String>,
}

/// Control characters become spaces, runs of whitespace collapse, the result is trimmed and cut.
pub fn clean_line(line: &str) -> Option<String> {
    let flat: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let joined = words.join(" ");
    if joined.chars().count() <= MAX_LINE_CHARS {
        return Some(joined);
    }
    let mut cut: String = joined.chars().take(MAX_LINE_CHARS - 1).collect();
    cut.push('…');
    Some(cut)
}

/// The text of the blocks of `content` whose `type` is one of `kinds` (or the string itself).
fn block_text(content: &Value, kinds: &[&str]) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str).is_some_and(|t| kinds.contains(&t)))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn model_label(v: Option<&Value>) -> Option<String> {
    let name = v?.as_str()?.trim();
    // Placeholder names some agents use for locally generated messages are not a model.
    (!name.is_empty() && !name.starts_with('<')).then(|| event::label(name))
}

/// What one transcript line contributes: (assistant text, model, user text).
fn read_line(kind: AgentKind, v: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let kind_of = |k: &str| v.get("type").and_then(Value::as_str) == Some(k);
    match kind {
        AgentKind::Claude if kind_of("assistant") => {
            let message = v.get("message");
            (message.and_then(|m| m.get("content")).and_then(|c| block_text(c, &["text"])), model_label(message.and_then(|m| m.get("model"))), None)
        }
        AgentKind::Claude if kind_of("user") => {
            let message = v.get("message");
            (None, None, message.and_then(|m| m.get("content")).and_then(|c| block_text(c, &["text"])))
        }
        AgentKind::Pi if kind_of("message") => {
            let message = v.get("message");
            let role = message.and_then(|m| m.get("role")).and_then(Value::as_str);
            let text = message.and_then(|m| m.get("content")).and_then(|c| block_text(c, &["text"]));
            if role == Some("assistant") {
                (text, model_label(message.and_then(|m| m.get("model"))), None)
            } else if role == Some("user") {
                (None, None, text)
            } else {
                (None, None, None)
            }
        }
        AgentKind::Codex => {
            let payload = v.get("payload");
            let payload_type = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
            if kind_of("response_item") && payload_type == Some("message") {
                let role = payload.and_then(|p| p.get("role")).and_then(Value::as_str);
                if role == Some("assistant") {
                    return (payload.and_then(|p| p.get("content")).and_then(|c| block_text(c, &["output_text"])), None, None);
                }
                if role == Some("user") {
                    return (None, None, payload.and_then(|p| p.get("content")).and_then(|c| block_text(c, &["input_text", "text"])));
                }
            }
            if kind_of("event_msg") && payload_type == Some("agent_message") {
                return (payload.and_then(|p| p.get("message")).and_then(Value::as_str).map(str::to_owned), None, None);
            }
            if kind_of("event_msg") && payload_type == Some("user_message") {
                return (None, None, payload.and_then(|p| p.get("message")).and_then(Value::as_str).map(str::to_owned));
            }
            if kind_of("turn_context") {
                return (None, model_label(payload.and_then(|p| p.get("model"))), None);
            }
            (None, None, None)
        }
        _ => (None, None, None),
    }
}

/// Read one window at the end of the file: (last assistant text, model, last user text).
fn scan(kind: AgentKind, file: &mut fs::File, len: u64, window: u64) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let start = len.saturating_sub(window);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.take(window).read_to_end(&mut bytes).ok()?;
    let mut lines = bytes.split(|b| *b == 10);
    if start > 0 {
        lines.next(); // begins mid-line
    }
    let (mut last_text, mut model, mut last_user) = (None, None, None);
    for line in lines {
        let Ok(text) = std::str::from_utf8(line) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text) else { continue };
        let (t, m, u) = read_line(kind, &value);
        if t.as_deref().is_some_and(|t| !t.trim().is_empty()) {
            last_text = t;
        }
        if m.is_some() {
            model = m;
        }
        if u.as_deref().is_some_and(|u| !u.trim().is_empty()) {
            last_user = u;
        }
    }
    Some((last_text, model, last_user))
}

/// Read the end of the transcript at `path`. Anything unreadable gives an empty `Peek`.
pub fn peek(kind: AgentKind, path: &Path) -> Peek {
    let read = || -> Option<Peek> {
        let mut file = fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut model = None;
        let mut task = None;
        for (i, window) in WINDOWS.iter().enumerate() {
            let (text, found_model, found_task) = scan(kind, &mut file, len, *window)?;
            model = found_model.or(model);
            // A larger window reaches further back; a task found in the smaller (newer) window wins.
            task = found_task.and_then(|t| t.lines().find_map(clean_line)).or(task);
            let last_line = text.and_then(|t| t.lines().rev().find_map(clean_line));
            // Done when a line was found, or the window already covered the whole file.
            if last_line.is_some() || len <= *window || i == WINDOWS.len() - 1 {
                return Some(Peek { task: task.or_else(|| last_line.clone()), last_line, model });
            }
        }
        None
    };
    read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-peek-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(lines: &[Value]) -> std::path::PathBuf {
        let p = dir().join("t.jsonl");
        fs::write(&p, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        p
    }

    #[test]
    fn claude_gives_the_last_line_of_the_last_assistant_text_and_its_model() {
        let p = write(&[
            json!({"type":"user","message":{"role":"user","content":"hello"}}),
            json!({"type":"assistant","message":{"model":"claude-old","content":[{"type":"text","text":"first answer"}]}}),
            json!({"type":"assistant","message":{"model":"claude-opus-5","content":[{"type":"text","text":"Line one\n\n  Done: tests pass \t (31)  \n"},{"type":"tool_use","name":"Bash"}]}}),
            json!({"type":"assistant","message":{"model":"claude-opus-5","content":[{"type":"tool_use","name":"Read"}]}}),
            json!({"type":"user","message":{"role":"user","content":"thanks"}}),
        ]);
        let got = peek(AgentKind::Claude, &p);
        assert_eq!(got.last_line.as_deref(), Some("Done: tests pass (31)"), "last non-empty line, whitespace collapsed, tool-only and user messages skipped");
        assert_eq!(got.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(got.task.as_deref(), Some("thanks"), "the most recent user message wins for the task name");
    }

    #[test]
    fn pi_and_codex_are_read_the_same_way() {
        let pi = write(&[
            json!({"type":"session","id":"x","cwd":"C:\\p"}),
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"question"}]}}),
            json!({"type":"message","message":{"role":"assistant","model":"gemini-x","content":[{"type":"thinking","text":"hidden"},{"type":"text","text":"Answer\nlast words"}]}}),
        ]);
        let got = peek(AgentKind::Pi, &pi);
        assert_eq!((got.last_line.as_deref(), got.model.as_deref(), got.task.as_deref()), (Some("last words"), Some("gemini-x"), Some("question")));
        let codex = write(&[
            json!({"type":"session_meta","payload":{"id":"x","cwd":"C:\\p"}}),
            json!({"type":"turn_context","payload":{"model":"gpt-6"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do it"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working on it\nAll done"}]}}),
            json!({"type":"event_msg","payload":{"type":"token_count"}}),
        ]);
        let got = peek(AgentKind::Codex, &codex);
        assert_eq!((got.last_line.as_deref(), got.model.as_deref(), got.task.as_deref()), (Some("All done"), Some("gpt-6"), Some("do it")));
        let agent_message = write(&[json!({"type":"event_msg","payload":{"type":"agent_message","message":"Plain message"}})]);
        let got = peek(AgentKind::Codex, &agent_message);
        assert_eq!((got.last_line.as_deref(), got.task.as_deref()), (Some("Plain message"), Some("Plain message")), "no user text: the assistant line is the task");
    }

    #[test]
    fn lines_are_cleaned_and_bounded_and_odd_files_give_nothing() {
        assert_eq!(clean_line("a\u{7}b\u{1b}[31mc   d"), Some("a b [31mc d".to_string()));
        assert_eq!(clean_line("   \t "), None);
        let long = clean_line(&"x".repeat(500)).unwrap();
        assert_eq!(long.chars().count(), MAX_LINE_CHARS);
        assert!(long.ends_with('…'));
        let d = dir();
        assert_eq!(peek(AgentKind::Claude, &d.join("missing.jsonl")), Peek::default());
        fs::write(d.join("junk.jsonl"), b"not json\n\xff\xfe\n{}\n").unwrap();
        assert_eq!(peek(AgentKind::Claude, &d.join("junk.jsonl")), Peek::default());
        // Placeholder model names are not models.
        let p = write(&[json!({"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"hi"}]}})]);
        assert_eq!(peek(AgentKind::Claude, &p).model, None);
    }

    #[test]
    fn a_larger_window_is_read_only_when_the_small_one_has_no_text() {
        let filler = |n: usize| (0..n).map(|_| json!({"type":"user","message":{"role":"user","content":"x".repeat(1000)}})).collect::<Vec<_>>();
        let early = json!({"type":"assistant","message":{"model":"early","content":[{"type":"text","text":"EARLY"}]}});
        // The answer is beyond the first window (256 KiB) but inside the second (1 MiB): found, cut first line and all.
        let mut lines = vec![early.clone()];
        lines.extend(filler(400));
        let p = write(&lines);
        assert!(fs::metadata(&p).unwrap().len() > WINDOWS[0]);
        assert_eq!(peek(AgentKind::Claude, &p).last_line.as_deref(), Some("EARLY"));
        // A later answer inside the first window wins.
        lines.push(json!({"type":"assistant","message":{"model":"late","content":[{"type":"text","text":"LATE"}]}}));
        assert_eq!(peek(AgentKind::Claude, &write(&lines)).last_line.as_deref(), Some("LATE"));
        // Beyond the last window (4 MiB) nothing is searched for.
        let mut far = vec![early];
        far.extend(filler(4500));
        let q = write(&far);
        assert!(fs::metadata(&q).unwrap().len() > WINDOWS[2]);
        let got = peek(AgentKind::Claude, &q);
        assert_eq!(got.last_line, None, "the assistant answer is beyond the last window");
        assert_eq!(got.model, None);
        assert!(got.task.as_deref().is_some_and(|t| t.starts_with("xxx")), "the latest user message is still the task");
    }
}
