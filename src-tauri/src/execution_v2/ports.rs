//! Native-only candidate admission seam. No complete v2 JournalStore/runtime API
//! is frozen or wired; none of these values confers dispatch or live permission.
use super::{validation, *};
pub use crate::execution::ports::{RunSeed, SessionSeed};

/// Native-only result. Fresh durable creation reports its queued event; exact
/// replay returns the immutable result and NO events. Neither case is a launch
/// instruction. Future owner-managed volatile arms are deliberately absent.
pub struct MutationCommit<T> {
    pub result: T,
    pub committed_events: Vec<RunEvent>,
    pub replayed: bool,
}

/// Candidate narrow store port, not the complete JournalStore/runtime freeze.
/// Shared writer transactions consume typed, validated adapter admission data;
/// no executable/network/model invocation belongs to this persistence boundary.
pub trait AdmissionJournal {
    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
        seed: SessionSeed,
    ) -> DomainResult<MutationCommit<SessionSnapshot>>;
    fn enqueue_run(
        &mut self,
        request: super::admission::StartRequest<'_>,
        seed: RunSeed,
    ) -> DomainResult<MutationCommit<RunSnapshot>>;
}

/// Candidate v2 compare-and-append request. The store re-derives every snapshot
/// through shared policy inside its writer transaction; callers cannot assert a
/// state, counter or budget. No Debug/Serialize: events carry assistant output.
pub struct AppendBatch {
    pub workspace_id: Id,
    pub run_id: Id,
    pub expected_last_seq: Sequence,
    pub expected_state: RunState,
    pub events: Vec<RunEvent>,
    pub next_snapshot: RunSnapshot,
}

/// Candidate writer port (append/cancel/layout), not the complete runtime freeze.
/// Committed writes are durable journal data only: no dispatch arm, availability,
/// readiness claim or permission to signal any worker. Cancellation persists its
/// intent; any later worker signal is the owner's job, after this commit.
pub trait JournalWriter {
    fn append_batch(&mut self, batch: AppendBatch) -> DomainResult<MutationCommit<RunSnapshot>>;
    /// `mutation` is required exactly when the run changes state (queued ->
    /// cancelled, active -> cancel_requested) and must be absent for no-ops.
    fn cancel_run(
        &mut self,
        request: &CancelRunRequest,
        mutation: Option<AppendBatch>,
    ) -> DomainResult<MutationCommit<RunSnapshot>>;
    fn save_layout(&mut self, request: &SaveLayoutRequest) -> DomainResult<MutationCommit<Layout>>;
}

/// Startup recovery result: runs interrupted and the events that did it.
#[derive(Default)]
pub struct RecoveryReport {
    pub interrupted_runs: Vec<RunSnapshot>,
    pub committed_events: Vec<RunEvent>,
}

/// Candidate bounded read port. Returned projections are not dispatch authority;
/// the future production owner must complete startup audit/recovery first.
pub trait JournalReader {
    fn workspace_snapshot(&mut self, request: &WorkspaceRequest)
        -> DomainResult<WorkspaceSnapshot>;
    fn get_run(&mut self, workspace_id: &str, run_id: &str) -> DomainResult<QueuedRun>;
    fn next_queued(&mut self, workspace_id: &str) -> DomainResult<Option<QueuedRun>>;
    fn list_run_events(&mut self, request: &EventPageRequest) -> DomainResult<EventPage>;
}

// No Debug/Serialize: native private input must not enter workspace/IPC/log DTOs.
pub struct QueuedRun {
    pub snapshot: RunSnapshot,
    pub input: QueuedInput,
}

// No Debug/Serialize: prompts remain private native input, not workspace data.
pub enum QueuedInput {
    Fake {
        prompt: String,
        scenario: FakeScenario,
    },
    CodexText {
        prompt: String,
        profile_id: CodexProfileId,
        /// Earlier runs of the same session carried into a `codex_text_v2` turn (ids only).
        context_run_ids: Vec<Id>,
    },
}
impl QueuedInput {
    /// Strict native stored-input codec. This reconstructs data, NOT consent or
    /// a launch arm; original request consent is audited separately in receipts.
    pub fn from_stored(
        snapshot: &RunSnapshot,
        session: &SessionSnapshot,
        prompt: &str,
        scenario: Option<&str>,
        profile: Option<&str>,
        context: Option<&str>,
    ) -> DomainResult<Self> {
        validation::validate_snapshot(snapshot)?;
        let limit = match snapshot.provider_id {
            ProviderId::Fake => PROMPT_BYTES,
            ProviderId::Codex => CODEX_PROMPT_BYTES,
        };
        if prompt.is_empty() || prompt.len() > limit {
            return Err(crate::execution::validation::error(
                "validation_failed",
                "Stored input exceeds its prompt budget.",
            ));
        }
        if snapshot.profile_id.map(|p| p.as_str()) != profile {
            return Err(crate::execution::validation::error(
                "validation_failed",
                "Stored input profile is inconsistent.",
            ));
        }
        let context_run_ids = match context {
            None => None,
            Some(raw) => {
                let ids: Vec<Id> = serde_json::from_str(raw).map_err(|_| {
                    crate::execution::validation::error(
                        "validation_failed",
                        "Stored run context is invalid.",
                    )
                })?;
                // Only the canonical encoding is accepted back.
                if serde_json::to_string(&ids).ok().as_deref() != Some(raw) {
                    return Err(crate::execution::validation::error(
                        "validation_failed",
                        "Stored run context is not canonical.",
                    ));
                }
                Some(ids)
            }
        };
        if snapshot.provider_id == ProviderId::Fake && context_run_ids.is_some() {
            return Err(crate::execution::validation::error(
                "validation_failed",
                "Stored fake input cannot carry run context.",
            ));
        }
        match snapshot.provider_id {
            ProviderId::Fake => {
                let scenario = scenario.ok_or_else(|| {
                    crate::execution::validation::error(
                        "validation_failed",
                        "Stored fake input is incomplete.",
                    )
                })?;
                let scenario = serde_json::from_value(serde_json::Value::String(scenario.into()))
                    .map_err(|_| {
                    crate::execution::validation::error(
                        "validation_failed",
                        "Stored fake input is invalid.",
                    )
                })?;
                Self::from_fake(
                    &StartFakeRunRequest {
                        contract_version: 2,
                        workspace_id: snapshot.workspace_id.clone(),
                        session_id: snapshot.session_id.clone(),
                        request_id: snapshot.run_id.clone(),
                        provider_id: ProviderId::Fake,
                        prompt: prompt.into(),
                        scenario,
                    },
                    session,
                )
            }
            ProviderId::Codex => {
                if scenario.is_some() {
                    return Err(crate::execution::validation::error(
                        "validation_failed",
                        "Stored Codex input cannot contain a fake scenario.",
                    ));
                }
                Self::from_codex(
                    &StartCodexRunRequest {
                        contract_version: 2,
                        workspace_id: snapshot.workspace_id.clone(),
                        session_id: snapshot.session_id.clone(),
                        request_id: snapshot.run_id.clone(),
                        prompt: prompt.into(),
                        profile_id: snapshot.profile_id.ok_or_else(|| {
                            crate::execution::validation::error(
                                "validation_failed",
                                "Stored Codex input has no profile.",
                            )
                        })?,
                        confirmed: true,
                        context_run_ids,
                    },
                    session,
                )
            }
        }
    }
    pub fn from_fake(
        request: &StartFakeRunRequest,
        session: &SessionSnapshot,
    ) -> DomainResult<Self> {
        validation::validate_fake_session(request, session)?;
        Ok(Self::Fake {
            prompt: request.prompt.clone(),
            scenario: request.scenario,
        })
    }
    pub fn from_codex(
        request: &StartCodexRunRequest,
        session: &SessionSnapshot,
    ) -> DomainResult<Self> {
        validation::validate_codex_session(request, session)?;
        Ok(Self::CodexText {
            prompt: request.prompt.clone(),
            profile_id: request.profile_id,
            context_run_ids: request.context_run_ids.clone().unwrap_or_default(),
        })
    }
    pub fn prompt(&self) -> &str {
        match self {
            Self::Fake { prompt, .. } | Self::CodexText { prompt, .. } => prompt,
        }
    }
    /// Earlier runs carried into this turn (empty for fake and `codex_text_v1`).
    pub fn context_run_ids(&self) -> &[Id] {
        match self {
            Self::CodexText {
                context_run_ids, ..
            } => context_run_ids,
            Self::Fake { .. } => &[],
        }
    }
    /// Canonical stored form of the context (`None` when there is none).
    pub fn context_json(&self) -> Option<String> {
        match self {
            Self::CodexText {
                profile_id: CodexProfileId::CodexTextV2,
                context_run_ids,
                ..
            } => serde_json::to_string(context_run_ids).ok(),
            _ => None,
        }
    }
    pub fn scenario(&self) -> Option<FakeScenario> {
        match self {
            Self::Fake { scenario, .. } => Some(*scenario),
            _ => None,
        }
    }
    pub fn profile(&self) -> Option<CodexProfileId> {
        match self {
            Self::CodexText { profile_id, .. } => Some(*profile_id),
            _ => None,
        }
    }
    pub fn provider(&self) -> ProviderId {
        match self {
            Self::Fake { .. } => ProviderId::Fake,
            Self::CodexText { .. } => ProviderId::Codex,
        }
    }
}
