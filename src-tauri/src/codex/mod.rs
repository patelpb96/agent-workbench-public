//! Isolated Codex transport and Windows Job-contained execution driver.
//!
//! Not connected to M1 fake execution, Tauri IPC, SQLite storage, or the GUI.
//! Phase 2A1 provides the bounded UTF-8 JSONL protocol state machine (`protocol`).
//! Phase 2A2 adds executable pin verification with `FILE_SHARE_READ` write-denial
//! locking (`config`), disposable nonce-verified workspace leases and atomic
//! one-shot smoke token consumption (`lease`), and suspended child creation
//! inside a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Windows Job with restricted
//! 3-pipe handle inheritance and tree-quiescence verification (`driver`).
//!
//! Local Job termination guarantees OS process-tree containment only; it does NOT
//! prove remote provider HTTP request cancellation.

pub mod config;
#[cfg(windows)]
pub mod driver;
pub mod lease;
mod protocol;

pub use config::{
    build_pinned_ack_args, CodexCommandSpec, ExecutablePin, VerifiedExecutable,
    LOCKDOWN_CONFIG_OVERRIDES, MAX_STDIN_PROMPT_BYTES, PINNED_CODEX_BYTES, PINNED_CODEX_EXE,
    PINNED_CODEX_SHA256, SMOKE_PROMPT,
};
#[cfg(windows)]
pub use driver::{
    run_pinned_smoke_once, run_turn_with_evidence, run_turn_with_evidence_notify,
    run_turn_with_lease, run_turn_with_progress, CancelToken, CodexDriverConfig, CodexError, CodexExecutionReport,
    CodexTurnOutcome, TeardownEvidence, DEFAULT_TIMEOUT, MAX_STDERR_BYTES,
};
pub use lease::{
    CodexLease, OneShotSmokeGate, DEFAULT_SMOKE_TOKEN_REL_PATH, LEASE_DIR_PREFIX,
    LEASE_MARKER_FILE, SMOKE_ENV_VAR, SMOKE_PURPOSE,
};
pub use protocol::{
    CodexJsonlDecoder, Milestone, ParsedTurn, ProtocolError, ReportedUsage, MAX_EVENTS, MAX_LINE_BYTES,
    MAX_MESSAGE_BYTES, MAX_STDOUT_BYTES,
};

static DIAGNOSTIC_SINK: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Native-only: where sanitized (text-free) diagnostics go. First call wins.
pub fn set_diagnostic_sink(sink: fn(&str)) {
    let _ = DIAGNOSTIC_SINK.set(sink);
}
#[cfg(windows)]
pub(crate) fn diagnostic(line: &str) {
    if let Some(sink) = DIAGNOSTIC_SINK.get() {
        sink(line);
    }
}

pub const PINNED_CLI_VERSION: &str = "0.163.0";
pub const SMOKE_MODEL: &str = "gpt-6.1-sol";
