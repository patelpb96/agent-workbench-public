//! Normalized events. No free-form provider text beyond bounded, sanitized labels.

pub const MAX_LABEL: usize = 80;
pub const MAX_ID: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Reported token counters. `None` means the source did not report the field
/// (never silently zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    SessionStarted { at_ms: Option<u64> },
    TurnStarted { at_ms: Option<u64> },
    TurnEnded { at_ms: Option<u64> },
    Message { role: Role, at_ms: Option<u64> },
    ToolStarted { call_id: String, tool: String, at_ms: Option<u64> },
    /// `ok` is `None` when the source records no outcome. `duration_ms` is the
    /// source's own measurement when it has one.
    ToolEnded { call_id: String, tool: Option<String>, at_ms: Option<u64>, ok: Option<bool>, duration_ms: Option<u64> },
    Usage { at_ms: Option<u64>, usage: Usage },
    Permission { call_id: Option<String>, decision: String, at_ms: Option<u64> },
}

fn clean(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(max)
        .collect()
}

/// Provider strings are bounded and stripped of control characters before use.
pub fn label(text: &str) -> String {
    clean(text, MAX_LABEL)
}
pub fn id(text: &str) -> String {
    clean(text, MAX_ID)
}
