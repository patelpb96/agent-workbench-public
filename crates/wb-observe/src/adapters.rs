//! Per-agent transcript adapters. One JSONL line in, zero or more normalized events out.
//! Unknown or unparseable lines yield nothing; message text, tool arguments and tool
//! output are never read. Formats observed 2026-10-21 (structure only); re-verify on upgrades.
use crate::{
    event::{id, label, Event, Role, Usage},
    time::parse_iso_ms,
};
use serde_json::Value;
use std::collections::VecDeque;

pub trait Adapter {
    fn parse_line(&mut self, line: &str) -> Vec<Event>;
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}
fn u(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}
fn blocks(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn iso(v: &Value) -> Option<u64> {
    parse_iso_ms(s(v, "timestamp")?)
}

/// Remember the most recent ids so a message split across lines counts once.
struct Seen {
    ids: VecDeque<String>,
}
impl Seen {
    fn new() -> Self {
        Self { ids: VecDeque::new() }
    }
    /// True if `key` was not seen before.
    fn first(&mut self, key: &str) -> bool {
        if self.ids.iter().any(|k| k == key) {
            return false;
        }
        if self.ids.len() == 256 {
            self.ids.pop_front();
        }
        self.ids.push_back(key.to_owned());
        true
    }
}

// ---- Claude Code ---------------------------------------------------------

pub struct ClaudeAdapter {
    started: bool,
    usage_seen: Seen,
}
impl ClaudeAdapter {
    pub fn new() -> Self {
        Self { started: false, usage_seen: Seen::new() }
    }
}
impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for ClaudeAdapter {
    fn parse_line(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let at = iso(&v);
        let mut out = Vec::new();
        let kind = s(&v, "type").unwrap_or("");
        if !self.started && at.is_some() && matches!(kind, "user" | "assistant" | "system") {
            self.started = true;
            out.push(Event::SessionStarted { at_ms: at });
        }
        let Some(msg) = v.get("message") else { return out };
        let content = msg.get("content").unwrap_or(&Value::Null);
        match kind {
            "assistant" => {
                for b in blocks(content) {
                    if s(b, "type") == Some("tool_use") {
                        if let (Some(cid), Some(name)) = (s(b, "id"), s(b, "name")) {
                            out.push(Event::ToolStarted { call_id: id(cid), tool: label(name), at_ms: at });
                        }
                    }
                }
                out.push(Event::Message { role: Role::Assistant, at_ms: at });
                if let (Some(mid), Some(usage)) = (s(msg, "id"), msg.get("usage")) {
                    if self.usage_seen.first(mid) {
                        out.push(Event::Usage {
                            at_ms: at,
                            usage: Usage {
                                input: u(usage, "input_tokens"),
                                output: u(usage, "output_tokens"),
                                cache_read: u(usage, "cache_read_input_tokens"),
                                cache_write: u(usage, "cache_creation_input_tokens"),
                                reasoning: usage.get("output_tokens_details").and_then(|d| u(d, "thinking_tokens")),
                            },
                        });
                    }
                }
            }
            "user" => {
                let mut had_result = false;
                for b in blocks(content) {
                    if s(b, "type") == Some("tool_result") {
                        had_result = true;
                        let cid = s(b, "tool_use_id").map(id);
                        // The API's `is_error` is optional and defaults to false.
                        let ok = Some(!b.get("is_error").and_then(Value::as_bool).unwrap_or(false));
                        if let Some(c) = &cid {
                            if let Some(d) = v.get("permissionDecision").and_then(|p| s(p, "decision")) {
                                out.push(Event::Permission { call_id: Some(c.clone()), decision: label(d), at_ms: at });
                            }
                            out.push(Event::ToolEnded { call_id: c.clone(), tool: None, at_ms: at, ok, duration_ms: None });
                        }
                    }
                }
                if !had_result && v.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                    out.push(Event::TurnStarted { at_ms: at });
                    out.push(Event::Message { role: Role::User, at_ms: at });
                }
            }
            _ => {}
        }
        out
    }
}

// ---- Pi ------------------------------------------------------------------

pub struct PiAdapter;
impl PiAdapter {
    pub fn new() -> Self {
        Self
    }
}
impl Default for PiAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for PiAdapter {
    fn parse_line(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let at = iso(&v);
        match s(&v, "type") {
            Some("session") => vec![Event::SessionStarted { at_ms: at }],
            Some("message") => {
                let Some(m) = v.get("message") else { return vec![] };
                let mut out = Vec::new();
                match s(m, "role") {
                    Some("assistant") => {
                        for b in blocks(m.get("content").unwrap_or(&Value::Null)) {
                            if s(b, "type") == Some("toolCall") {
                                if let (Some(cid), Some(name)) = (s(b, "id"), s(b, "name")) {
                                    out.push(Event::ToolStarted { call_id: id(cid), tool: label(name), at_ms: at });
                                }
                            }
                        }
                        out.push(Event::Message { role: Role::Assistant, at_ms: at });
                        if let Some(usage) = m.get("usage") {
                            out.push(Event::Usage {
                                at_ms: at,
                                usage: Usage {
                                    input: u(usage, "input"),
                                    output: u(usage, "output"),
                                    cache_read: u(usage, "cacheRead"),
                                    cache_write: u(usage, "cacheWrite"),
                                    reasoning: u(usage, "reasoning"),
                                },
                            });
                        }
                    }
                    Some("toolResult") => {
                        if let Some(cid) = s(m, "toolCallId") {
                            out.push(Event::ToolEnded {
                                call_id: id(cid),
                                tool: s(m, "toolName").map(label),
                                at_ms: at,
                                ok: m.get("isError").and_then(Value::as_bool).map(|e| !e),
                                duration_ms: u(m, "durationMs"),
                            });
                        }
                    }
                    Some("user") => {
                        out.push(Event::TurnStarted { at_ms: at });
                        out.push(Event::Message { role: Role::User, at_ms: at });
                    }
                    _ => {}
                }
                out
            }
            _ => vec![],
        }
    }
}

// ---- Codex ---------------------------------------------------------------

pub struct CodexAdapter;
impl CodexAdapter {
    pub fn new() -> Self {
        Self
    }
}
impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for CodexAdapter {
    fn parse_line(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let at = iso(&v);
        let Some(p) = v.get("payload") else { return vec![] };
        match s(&v, "type") {
            Some("session_meta") => vec![Event::SessionStarted { at_ms: at }],
            Some("event_msg") => match s(p, "type") {
                Some("task_started") => vec![Event::TurnStarted { at_ms: at }],
                Some("task_complete") => vec![Event::TurnEnded { at_ms: at }],
                _ => vec![],
            },
            Some("response_item") => match s(p, "type") {
                Some("function_call") | Some("custom_tool_call") => match (s(p, "call_id"), s(p, "name")) {
                    (Some(cid), Some(name)) => vec![Event::ToolStarted { call_id: id(cid), tool: label(name), at_ms: at }],
                    _ => vec![],
                },
                Some("function_call_output") | Some("custom_tool_call_output") => match s(p, "call_id") {
                    Some(cid) => vec![Event::ToolEnded { call_id: id(cid), tool: None, at_ms: at, ok: None, duration_ms: None }],
                    None => vec![],
                },
                Some("message") => match s(p, "role") {
                    Some("user") => vec![Event::Message { role: Role::User, at_ms: at }],
                    Some("assistant") => vec![Event::Message { role: Role::Assistant, at_ms: at }],
                    _ => vec![],
                },
                _ => vec![],
            },
            Some("token_usage_record") => match p.get("usage") {
                Some(usage) => vec![Event::Usage {
                    at_ms: at,
                    usage: Usage {
                        input: u(usage, "input_tokens"),
                        output: u(usage, "output_tokens"),
                        cache_read: u(usage, "cached_input_tokens"),
                        cache_write: u(usage, "cache_write_input_tokens"),
                        reasoning: u(usage, "reasoning_output_tokens"),
                    },
                }],
                None => vec![],
            },
            _ => vec![],
        }
    }
}
