//! Gated manual entry point for the single Phase 2B live Codex smoke task.
//!
//! Default `cargo test` runs only `default_suite_refuses_unarmed_live_smoke`
//! and NEVER invokes `codex.exe`. The `#[ignore]` test `live_codex_ack_smoke`
//! requires both `WORKBENCH_CODEX_SMOKE_ONCE=1` and a valid one-shot token file
//! at `artifacts/codex-smoke/approval-token.json`, which is atomically consumed
//! and deleted before executable pin verification or process creation.

use agent_workbench::codex::{
    run_pinned_smoke_once, CodexDriverConfig, CodexError, DEFAULT_SMOKE_TOKEN_REL_PATH,
    PINNED_CLI_VERSION, SMOKE_ENV_VAR, SMOKE_MODEL,
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn resolve_smoke_token_path() -> PathBuf {
    let direct = Path::new(DEFAULT_SMOKE_TOKEN_REL_PATH);
    if direct.exists() || direct.parent().is_some_and(Path::exists) {
        return direct.to_path_buf();
    }
    Path::new("..").join(DEFAULT_SMOKE_TOKEN_REL_PATH)
}

#[test]
fn default_suite_refuses_unarmed_live_smoke() {
    assert_ne!(
        std::env::var(SMOKE_ENV_VAR).ok().as_deref(),
        Some("1"),
        "ordinary cargo test must never run with {SMOKE_ENV_VAR}=1"
    );
    let missing_token = std::env::temp_dir().join("nonexistent-codex-approval-token.json");
    assert_eq!(
        run_pinned_smoke_once(&missing_token, CodexDriverConfig::default()).err(),
        Some(CodexError::SmokeGateNotArmed)
    );
}

#[test]
#[ignore = "manual Phase 2B one-shot live Codex smoke only; requires WORKBENCH_CODEX_SMOKE_ONCE=1 and approval token"]
fn live_codex_ack_smoke() {
    let token_path = resolve_smoke_token_path();
    let outcome = match run_pinned_smoke_once(&token_path, CodexDriverConfig::default()) {
        Ok(outcome) => outcome,
        Err(err) => {
            panic!(
                "live_codex_ack_smoke failed closed with code={} (token_remaining={})",
                err.code(),
                token_path.exists()
            );
        }
    };
    assert!(!token_path.exists(), "one-shot token must be deleted");
    assert!(outcome.owned_workspace_removed);
    assert!(!outcome.turn.thread_id.is_empty());
    assert_eq!(outcome.turn.message.trim(), "ACK");
    assert!(outcome.turn.usage.input_tokens > 0);
    assert!(outcome.turn.usage.output_tokens > 0);

    let summary = json!({
        "cli_version": PINNED_CLI_VERSION,
        "model": SMOKE_MODEL,
        "message_exact_ack": outcome.turn.message.trim() == "ACK",
        "message_bytes": outcome.turn.message.len(),
        "thread_id_len": outcome.turn.thread_id.len(),
        "events_seen": outcome.turn.events_seen,
        "reasoning_items_discarded": outcome.turn.reasoning_items_discarded,
        "usage": {
            "input_tokens": outcome.turn.usage.input_tokens,
            "cached_input_tokens": outcome.turn.usage.cached_input_tokens,
            "cache_write_input_tokens": outcome.turn.usage.cache_write_input_tokens,
            "output_tokens": outcome.turn.usage.output_tokens,
            "reasoning_output_tokens": outcome.turn.usage.reasoning_output_tokens,
        },
        "stderr_bytes_drained": outcome.stderr_bytes_drained,
        "total_job_processes": outcome.total_job_processes,
        "owned_workspace_removed": outcome.owned_workspace_removed,
        "approval_token_deleted": !token_path.exists(),
    });
    println!("CODEX_SMOKE_SUMMARY={summary}");
}
