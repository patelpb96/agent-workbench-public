//! Incrementally built, bounded, rebuildable statistics derived from normalized events.
//! Nothing here is authoritative: replay the transcript to rebuild it.
use crate::event::{Event, Role, Usage};
use std::collections::{BTreeMap, HashMap, VecDeque};

pub const MAX_RECENT_CALLS: usize = 5_000;
pub const MAX_OPEN_CALLS: usize = 4_096;
pub const MAX_SAMPLES_PER_TOOL: usize = 1_000;
pub const MAX_TOOLS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRow {
    pub call_id: String,
    pub tool: String,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    pub ok: Option<bool>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolStats {
    /// Calls seen starting.
    pub calls: u64,
    /// Calls with a recorded end.
    pub completed: u64,
    pub errors: u64,
    /// Completed calls whose source records no outcome.
    pub unknown_outcome: u64,
    /// Sum of known durations (tool-seconds; parallel calls can exceed wall time).
    pub total_ms: u64,
    pub max_ms: u64,
    pub last_used_ms: Option<u64>,
    samples: VecDeque<u64>,
}
impl ToolStats {
    /// Median of the most recent bounded samples; `None` with no known duration.
    pub fn median_ms(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<u64> = self.samples.iter().copied().collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }
    pub fn sampled(&self) -> usize {
        self.samples.len()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    /// Usage records applied.
    pub records: u64,
}
fn add(total: &mut Option<u64>, add: Option<u64>) {
    if let Some(a) = add {
        *total = Some(total.unwrap_or(0).saturating_add(a));
    }
}

/// Wall-clock split derived from event gaps. A heuristic: only as precise as the
/// source timestamps, and parallel tool calls are counted per call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub tool_ms: u64,
    pub model_ms: u64,
    pub human_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Empty,
    Working,
    /// The agent finished and nothing is running: it is the human's turn.
    WaitingForHuman,
    /// No activity for the caller's idle threshold.
    Idle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    ToolStart,
    ToolEnd,
}

#[derive(Default)]
pub struct Index {
    tools: BTreeMap<String, ToolStats>,
    open: HashMap<String, CallRow>,
    open_order: VecDeque<String>,
    recent: VecDeque<CallRow>,
    totals: Totals,
    breakdown: Breakdown,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    last: Option<(u64, Kind)>,
    turns: u64,
    turn_open: bool,
    pub orphan_results: u64,
    pub dropped_open: u64,
    pub events: u64,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, e: &Event) {
        self.events += 1;
        match e {
            Event::SessionStarted { at_ms } => self.touch(*at_ms),
            Event::TurnStarted { at_ms } => {
                self.touch(*at_ms);
                self.turns += 1;
                self.turn_open = true;
            }
            Event::TurnEnded { at_ms } => {
                self.touch(*at_ms);
                // The agent finished composing its turn: treat like a final assistant message.
                self.gap(*at_ms, Kind::Assistant);
                self.turn_open = false;
            }
            Event::Message { role, at_ms } => {
                self.touch(*at_ms);
                self.gap(*at_ms, match role {
                    Role::User => Kind::User,
                    Role::Assistant => Kind::Assistant,
                });
            }
            Event::ToolStarted { call_id, tool, at_ms } => {
                self.touch(*at_ms);
                self.gap(*at_ms, Kind::ToolStart);
                self.tool_entry(tool).calls += 1;
                if let Some(t) = at_ms {
                    self.tool_entry(tool).last_used_ms = Some(self.tool_entry(tool).last_used_ms.map_or(*t, |p| p.max(*t)));
                }
                if self.open.len() >= MAX_OPEN_CALLS {
                    if let Some(old) = self.open_order.pop_front() {
                        self.open.remove(&old);
                        self.dropped_open += 1;
                    }
                }
                self.open_order.push_back(call_id.clone());
                self.open.insert(
                    call_id.clone(),
                    CallRow { call_id: call_id.clone(), tool: tool.clone(), started_ms: *at_ms, ended_ms: None, ok: None, duration_ms: None },
                );
            }
            Event::ToolEnded { call_id, tool, at_ms, ok, duration_ms } => {
                self.touch(*at_ms);
                self.gap(*at_ms, Kind::ToolEnd);
                let mut row = match self.open.remove(call_id) {
                    Some(r) => {
                        self.open_order.retain(|id| id != call_id);
                        r
                    }
                    None => {
                        self.orphan_results += 1;
                        let Some(name) = tool else { return };
                        CallRow { call_id: call_id.clone(), tool: name.clone(), started_ms: None, ended_ms: None, ok: None, duration_ms: None }
                    }
                };
                row.ended_ms = *at_ms;
                row.ok = *ok;
                row.duration_ms = duration_ms.or(match (row.started_ms, *at_ms) {
                    (Some(s), Some(t)) if t >= s => Some(t - s),
                    _ => None,
                });
                let stats = self.tool_entry(&row.tool);
                stats.completed += 1;
                match ok {
                    Some(false) => stats.errors += 1,
                    None => stats.unknown_outcome += 1,
                    Some(true) => {}
                }
                if let Some(d) = row.duration_ms {
                    stats.total_ms = stats.total_ms.saturating_add(d);
                    stats.max_ms = stats.max_ms.max(d);
                    if stats.samples.len() == MAX_SAMPLES_PER_TOOL {
                        stats.samples.pop_front();
                    }
                    stats.samples.push_back(d);
                    self.breakdown.tool_ms = self.breakdown.tool_ms.saturating_add(d);
                }
                if self.recent.len() == MAX_RECENT_CALLS {
                    self.recent.pop_front();
                }
                self.recent.push_back(row);
            }
            Event::Usage { at_ms, usage } => {
                self.touch(*at_ms);
                let Usage { input, output, cache_read, cache_write, reasoning } = *usage;
                add(&mut self.totals.input, input);
                add(&mut self.totals.output, output);
                add(&mut self.totals.cache_read, cache_read);
                add(&mut self.totals.cache_write, cache_write);
                add(&mut self.totals.reasoning, reasoning);
                self.totals.records += 1;
            }
            Event::Permission { at_ms, .. } => self.touch(*at_ms),
        }
    }

    fn touch(&mut self, at: Option<u64>) {
        if let Some(t) = at {
            self.first_ms = Some(self.first_ms.map_or(t, |f| f.min(t)));
            self.last_ms = Some(self.last_ms.map_or(t, |l| l.max(t)));
        }
    }

    fn gap(&mut self, at: Option<u64>, kind: Kind) {
        let Some(t) = at else { return };
        if let Some((t0, k0)) = self.last {
            let gap = t.saturating_sub(t0);
            match (k0, kind) {
                (Kind::User | Kind::ToolEnd, Kind::Assistant | Kind::ToolStart) => {
                    self.breakdown.model_ms = self.breakdown.model_ms.saturating_add(gap)
                }
                (Kind::Assistant, Kind::User) => self.breakdown.human_ms = self.breakdown.human_ms.saturating_add(gap),
                _ => {}
            }
        }
        self.last = Some((t, kind));
    }

    fn tool_entry(&mut self, tool: &str) -> &mut ToolStats {
        if !self.tools.contains_key(tool) && self.tools.len() >= MAX_TOOLS {
            return self.tools.entry("(other)".to_owned()).or_default();
        }
        self.tools.entry(tool.to_owned()).or_default()
    }

    // ---- reads ----------------------------------------------------------

    /// Tools ordered by call count (descending), then name.
    pub fn tools(&self) -> Vec<(&str, &ToolStats)> {
        let mut v: Vec<_> = self.tools.iter().map(|(k, s)| (k.as_str(), s)).collect();
        v.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
        v
    }
    pub fn tool(&self, name: &str) -> Option<&ToolStats> {
        self.tools.get(name)
    }
    pub fn recent_calls(&self) -> impl Iterator<Item = &CallRow> {
        self.recent.iter()
    }
    pub fn open_calls(&self) -> usize {
        self.open.len()
    }
    /// The names of the tools with a call in flight, each once, sorted, at most `limit`. Names only.
    pub fn open_tool_names(&self, limit: usize) -> Vec<String> {
        let mut names: Vec<String> = self.open.values().map(|c| c.tool.clone()).collect();
        names.sort();
        names.dedup();
        names.truncate(limit);
        names
    }
    pub fn totals(&self) -> Totals {
        self.totals
    }
    pub fn breakdown(&self) -> Breakdown {
        self.breakdown
    }
    pub fn turns(&self) -> u64 {
        self.turns
    }
    pub fn first_event_ms(&self) -> Option<u64> {
        self.first_ms
    }
    pub fn last_event_ms(&self) -> Option<u64> {
        self.last_ms
    }

    /// State at `now_ms`. Heuristic: transcripts carry no explicit idle marker.
    pub fn state(&self, now_ms: u64, idle_after_ms: u64) -> SessionState {
        let Some(last) = self.last_ms else { return SessionState::Empty };
        if now_ms.saturating_sub(last) >= idle_after_ms {
            return SessionState::Idle;
        }
        if !self.open.is_empty() || self.turn_open && !matches!(self.last, Some((_, Kind::Assistant))) {
            return SessionState::Working;
        }
        match self.last {
            Some((_, Kind::Assistant)) => SessionState::WaitingForHuman,
            Some((_, Kind::User | Kind::ToolEnd | Kind::ToolStart)) => SessionState::Working,
            None => SessionState::WaitingForHuman,
        }
    }
}
