//! A serializable, bounded view of an [`Index`] for the inspector UI. Names, counts,
//! timings and reported usage only; never transcript text.
use crate::index::{Index, SessionState};
use serde::Serialize;

pub const RECENT_CALLS: usize = 50;
pub const MAX_BUCKETS: usize = 48;
pub const MAX_TOOL_ROWS: usize = 100;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolRow {
    pub name: String,
    pub calls: u64,
    pub completed: u64,
    pub errors: u64,
    /// Completed calls whose source records no outcome (shown as unknown, not success).
    pub unknown_outcome: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    pub median_ms: Option<u64>,
    pub last_used_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CallView {
    pub tool: String,
    pub started_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// `Some(true)` ok, `Some(false)` error, `None` unknown.
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageView {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    pub records: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BreakdownView {
    pub tool_ms: u64,
    pub model_ms: u64,
    pub human_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct Bucket {
    pub calls: u64,
    pub errors: u64,
    pub tool_ms: u64,
}

/// Tool activity over time from the retained recent calls (bounded; older calls are not here).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Timeline {
    pub start_ms: u64,
    pub bucket_ms: u64,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    /// `empty`, `working`, `waiting_for_human` or `idle` (a heuristic).
    pub state: &'static str,
    pub first_event_ms: Option<u64>,
    pub last_event_ms: Option<u64>,
    pub turns: u64,
    pub open_calls: usize,
    pub usage: UsageView,
    pub breakdown: BreakdownView,
    pub tools: Vec<ToolRow>,
    /// Tools beyond `MAX_TOOL_ROWS` are summarized here rather than listed.
    pub tools_omitted: usize,
    pub recent: Vec<CallView>,
    pub timeline: Option<Timeline>,
    pub orphan_results: u64,
}

/// Buckets the retained calls by start time into at most `MAX_BUCKETS` equal slots of at least one
/// minute. `None` when no call has a start time.
pub fn timeline(index: &Index) -> Option<Timeline> {
    let calls: Vec<_> = index.recent_calls().filter_map(|c| c.started_ms.map(|t| (t, c))).collect();
    let first = calls.iter().map(|(t, _)| *t).min()?;
    let last = calls.iter().map(|(t, _)| *t).max()?;
    let span = last - first + 1;
    let minute = 60_000u64;
    let bucket_ms = span.div_ceil(MAX_BUCKETS as u64).div_ceil(minute).max(1) * minute;
    let count = (span.div_ceil(bucket_ms)) as usize;
    let mut buckets = vec![Bucket::default(); count.clamp(1, MAX_BUCKETS)];
    for (t, c) in calls {
        let i = (((t - first) / bucket_ms) as usize).min(buckets.len() - 1);
        buckets[i].calls += 1;
        buckets[i].tool_ms = buckets[i].tool_ms.saturating_add(c.duration_ms.unwrap_or(0));
        if c.ok == Some(false) {
            buckets[i].errors += 1;
        }
    }
    Some(Timeline { start_ms: first, bucket_ms, buckets })
}

pub fn snapshot(index: &Index, now_ms: u64, idle_after_ms: u64) -> Snapshot {
    let state = match index.state(now_ms, idle_after_ms) {
        SessionState::Empty => "empty",
        SessionState::Working => "working",
        SessionState::WaitingForHuman => "waiting_for_human",
        SessionState::Idle => "idle",
    };
    let all = index.tools();
    let tools_omitted = all.len().saturating_sub(MAX_TOOL_ROWS);
    let tools = all
        .into_iter()
        .take(MAX_TOOL_ROWS)
        .map(|(name, s)| ToolRow {
            name: name.to_owned(),
            calls: s.calls,
            completed: s.completed,
            errors: s.errors,
            unknown_outcome: s.unknown_outcome,
            total_ms: s.total_ms,
            max_ms: s.max_ms,
            median_ms: s.median_ms(),
            last_used_ms: s.last_used_ms,
        })
        .collect();
    let recent_all: Vec<_> = index.recent_calls().collect();
    let recent = recent_all
        .iter()
        .rev()
        .take(RECENT_CALLS)
        .map(|c| CallView { tool: c.tool.clone(), started_ms: c.started_ms, duration_ms: c.duration_ms, ok: c.ok })
        .collect();
    let t = index.totals();
    let b = index.breakdown();
    Snapshot {
        state,
        first_event_ms: index.first_event_ms(),
        last_event_ms: index.last_event_ms(),
        turns: index.turns(),
        open_calls: index.open_calls(),
        usage: UsageView { input: t.input, output: t.output, cache_read: t.cache_read, cache_write: t.cache_write, reasoning: t.reasoning, records: t.records },
        breakdown: BreakdownView { tool_ms: b.tool_ms, model_ms: b.model_ms, human_ms: b.human_ms },
        tools,
        tools_omitted,
        recent,
        timeline: timeline(index),
        orphan_results: index.orphan_results,
    }
}
