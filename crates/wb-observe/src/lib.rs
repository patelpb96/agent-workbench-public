//! Provider-neutral observation of agent sessions from their transcripts.
//!
//! Read-only by construction: adapters parse one JSONL line at a time into a small
//! normalized event (names, ids, timestamps, outcome flags, reported usage). Tool
//! arguments, results and message text are never read into events. An [`index::Index`]
//! derives per-tool statistics from those events incrementally, and a
//! [`tail::Tailer`] feeds it from a growing file by byte offset. The transcript stays
//! the source of truth; the index can always be rebuilt by replaying the file.
pub mod adapters;
pub mod discover;
pub mod event;
pub mod index;
pub mod link;
pub mod map;
pub mod peek;
pub mod snapshot;
pub mod tail;
pub mod time;

pub use adapters::{Adapter, ClaudeAdapter, CodexAdapter, PiAdapter};
pub use event::{Event, Role, Usage};
pub use index::{Breakdown, CallRow, Index, SessionState, ToolStats, Totals};
pub use discover::{discover, Found};
pub use peek::{peek, Peek};
pub use snapshot::{snapshot, Snapshot};
pub use tail::{AgentKind, Observer, PollResult, Tailer};

#[cfg(test)]
mod tests;
