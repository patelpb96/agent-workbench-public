use serde::{
    de::{Error as _, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

pub const MAX_LINE_BYTES: usize = 65_536;
pub const MAX_STDOUT_BYTES: usize = 262_144;
pub const MAX_EVENTS: usize = 64;
pub const MAX_MESSAGE_BYTES: usize = 16_384;
const MAX_SAFE_COUNTER: u64 = 9_007_199_254_740_991;

/// Code-only errors never include raw stdout, reasoning, paths or provider text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    LimitExceeded,
    InvalidJson,
    InvalidShape,
    InvalidLifecycle,
    ForbiddenActivity,
    ProviderFailure,
    Incomplete,
}
impl ProtocolError {
    pub fn code(self) -> &'static str {
        match self {
            Self::LimitExceeded => "codex_output_limit",
            Self::InvalidJson => "codex_invalid_jsonl",
            Self::InvalidShape => "codex_invalid_event",
            Self::InvalidLifecycle => "codex_invalid_lifecycle",
            Self::ForbiddenActivity => "codex_forbidden_activity",
            Self::ProviderFailure => "codex_turn_failed",
            Self::Incomplete => "codex_incomplete_stream",
        }
    }
}
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for ProtocolError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportedUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    /// Missing optional fields are not silently invented as zero.
    pub cache_write_input_tokens: Option<u64>,
}

/// Provisional decoded data ONLY. The future process driver must separately
/// verify process exit=0, both pipe EOFs and an empty owned Job before success.
/// No Debug/Serialize implementation: callers must choose evidence explicitly.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedTurn {
    pub thread_id: String,
    pub message: String,
    pub usage: ReportedUsage,
    pub events_seen: usize,
    pub reasoning_items_discarded: usize,
}
/// Coarse lifecycle milestones for volatile progress display. Carry no text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    ThreadStarted,
    TurnStarted,
    ReasoningObserved,
    MessageComplete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initial,
    Thread,
    Turn,
    Message,
    Complete,
}

/// Incremental bounded UTF-8 JSONL parser for the pinned, tool-free text-turn
/// profile. Unknown event/item kinds fail closed; optional extra JSON fields are
/// bounded but ignored. Never derives fake I0 events or stores reasoning text.
/// No Debug implementation because a partial line can contain private data.
pub struct CodexJsonlDecoder {
    line: Vec<u8>,
    bytes: usize,
    events: usize,
    phase: Phase,
    failed: Option<ProtocolError>,
    thread_id: Option<String>,
    message: Option<String>,
    usage: Option<ReportedUsage>,
    item_ids: BTreeSet<String>,
    reasoning_items: usize,
    milestones: Vec<Milestone>,
    trace: Vec<&'static str>,
}
impl Default for CodexJsonlDecoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            bytes: 0,
            events: 0,
            phase: Phase::Initial,
            failed: None,
            thread_id: None,
            message: None,
            usage: None,
            item_ids: BTreeSet::new(),
            reasoning_items: 0,
            milestones: Vec::new(),
            trace: Vec::new(),
        }
    }
}
impl CodexJsonlDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Errors poison the decoder permanently and discard all buffered content.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if bytes.len() > MAX_STDOUT_BYTES.saturating_sub(self.bytes) {
            return self.poison(ProtocolError::LimitExceeded);
        }
        self.bytes += bytes.len();
        for &byte in bytes {
            if byte == b'\n' {
                if let Err(error) = self.consume_line() {
                    return self.poison(error);
                }
            } else {
                if self.line.len() == MAX_LINE_BYTES {
                    return self.poison(ProtocolError::LimitExceeded);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }

    /// Milestones decoded since the last call (at most one per accepted event).
    pub fn take_milestones(&mut self) -> Vec<Milestone> {
        std::mem::take(&mut self.milestones)
    }

    /// Fixed-vocabulary labels of the events seen so far (the failing one
    /// last), for diagnosing rejected sequences. Never contains provider text:
    /// anything outside the known vocabulary is labelled `unknown`.
    pub fn trace(&self) -> String {
        self.trace.join(",")
    }

    /// Call only at stdout EOF. A complete last JSON value need not have LF.
    /// This consumes the decoder; it does NOT assert process/Job completion.
    pub fn finish_stdout(mut self) -> Result<ParsedTurn, ProtocolError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.line.is_empty() {
            self.consume_line()?;
        }
        if self.phase != Phase::Complete {
            return Err(ProtocolError::Incomplete);
        }
        Ok(ParsedTurn {
            thread_id: self.thread_id.take().ok_or(ProtocolError::Incomplete)?,
            message: self.message.take().ok_or(ProtocolError::Incomplete)?,
            usage: self.usage.take().ok_or(ProtocolError::Incomplete)?,
            events_seen: self.events,
            reasoning_items_discarded: self.reasoning_items,
        })
    }

    fn poison(&mut self, error: ProtocolError) -> Result<(), ProtocolError> {
        self.failed = Some(error);
        self.line.clear();
        self.thread_id = None;
        self.message = None;
        self.usage = None;
        self.item_ids.clear();
        self.milestones.clear();
        Err(error)
    }
    fn consume_line(&mut self) -> Result<(), ProtocolError> {
        if self.phase == Phase::Complete {
            return Err(ProtocolError::InvalidLifecycle);
        }
        if self.events == MAX_EVENTS {
            return Err(ProtocolError::LimitExceeded);
        }
        // serde_json validates UTF-8 and JSON syntax within the already checked
        // borrowed-byte ceiling; no raw error is propagated to callers.
        let parsed = serde_json::from_slice::<UniqueJson>(&self.line);
        let Ok(UniqueJson(value)) = parsed else {
            self.trace.push("invalid_json");
            return Err(ProtocolError::InvalidJson);
        };
        self.line.clear();
        self.events += 1;
        self.trace.push(trace_label(&value));
        match field(&value, "type")? {
            "thread.started" if self.phase == Phase::Initial => {
                self.thread_id = Some(identifier(&value, "thread_id")?.to_owned());
                self.phase = Phase::Thread;
                self.milestones.push(Milestone::ThreadStarted);
            }
            "turn.started" if self.phase == Phase::Thread => {
                self.phase = Phase::Turn;
                self.milestones.push(Milestone::TurnStarted);
            }
            "item.completed" => {
                let item = value.get("item").ok_or(ProtocolError::InvalidShape)?;
                let kind = field(item, "type")?;
                if kind != "agent_message" && kind != "reasoning" {
                    return Err(ProtocolError::ForbiddenActivity);
                }
                // Codex may send a preamble message before its answer: after the
                // first message, further messages/reasoning are accepted until
                // turn.completed and the LAST message is the reply.
                if self.phase != Phase::Turn && self.phase != Phase::Message {
                    return Err(ProtocolError::InvalidLifecycle);
                }
                let id = identifier(item, "id")?;
                if !self.item_ids.insert(id.to_owned()) {
                    return Err(ProtocolError::InvalidLifecycle);
                }
                let text = field(item, "text")?;
                if kind == "reasoning" {
                    // Drop the parsed Value at return. Retain only this count.
                    self.reasoning_items += 1;
                    self.milestones.push(Milestone::ReasoningObserved);
                } else {
                    if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
                        return Err(ProtocolError::LimitExceeded);
                    }
                    self.message = Some(text.to_owned());
                    if self.phase != Phase::Message {
                        self.milestones.push(Milestone::MessageComplete);
                    }
                    self.phase = Phase::Message;
                }
            }
            "turn.completed" if self.phase == Phase::Message => {
                let usage = value.get("usage").ok_or(ProtocolError::InvalidShape)?;
                self.usage = Some(ReportedUsage {
                    input_tokens: counter(usage, "input_tokens")?,
                    cached_input_tokens: counter(usage, "cached_input_tokens")?,
                    output_tokens: counter(usage, "output_tokens")?,
                    reasoning_output_tokens: counter(usage, "reasoning_output_tokens")?,
                    cache_write_input_tokens: usage
                        .get("cache_write_input_tokens")
                        .map(|_| counter(usage, "cache_write_input_tokens"))
                        .transpose()?,
                });
                self.phase = Phase::Complete;
            }
            "error" | "turn.failed" => return Err(ProtocolError::ProviderFailure),
            "thread.started" | "turn.started" | "turn.completed" => {
                return Err(ProtocolError::InvalidLifecycle)
            }
            // Pinned 0.163.0 emits reasoning/assistant ONLY as item.completed.
            // started/updated items, aliases, tools, plans and unknown events are
            // deliberately unsupported in this minimal text-turn profile.
            _ => return Err(ProtocolError::ForbiddenActivity),
        }
        Ok(())
    }
}
fn trace_label(value: &Value) -> &'static str {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let item = value
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match (kind, item) {
        ("thread.started", _) => "thread.started",
        ("turn.started", _) => "turn.started",
        ("turn.completed", _) => "turn.completed",
        ("turn.failed", _) => "turn.failed",
        ("error", _) => "error",
        ("item.completed", "agent_message") => "item.completed:agent_message",
        ("item.completed", "reasoning") => "item.completed:reasoning",
        ("item.completed", "command_execution") => "item.completed:command_execution",
        ("item.completed", "web_search") => "item.completed:web_search",
        ("item.completed", "mcp_tool_call") => "item.completed:mcp_tool_call",
        ("item.completed", "file_change") => "item.completed:file_change",
        ("item.completed", "todo_list") => "item.completed:todo_list",
        ("item.started", "agent_message") => "item.started:agent_message",
        ("item.started", "reasoning") => "item.started:reasoning",
        ("item.started", "command_execution") => "item.started:command_execution",
        ("item.started", "web_search") => "item.started:web_search",
        ("item.started", "mcp_tool_call") => "item.started:mcp_tool_call",
        ("item.updated", _) => "item.updated",
        ("item.started", _) => "item.started:other",
        ("item.completed", _) => "item.completed:other",
        _ => "unknown",
    }
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProtocolError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidShape)
}
fn identifier<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProtocolError> {
    let text = field(value, key)?;
    if text.is_empty() || text.len() > 256 || !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ProtocolError::InvalidShape);
    }
    Ok(text)
}
fn counter(value: &Value, key: &str) -> Result<u64, ProtocolError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n <= MAX_SAFE_COUNTER)
        .ok_or(ProtocolError::InvalidShape)
}

// Value's normal deserializer silently accepts duplicate object keys. Reject
// them recursively, including ignored optional fields, rather than choosing an
// ambiguous lifecycle/identity/value. Serde's normal recursion limit remains.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("unambiguous JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueJson(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(v)) = seq.next_element()? {
                    values.push(v);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(A::Error::custom("duplicate JSON key"));
                    }
                    let UniqueJson(v) = map.next_value()?;
                    values.insert(key, v);
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

#[cfg(test)]
mod tests;
