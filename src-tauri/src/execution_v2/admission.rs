//! Sealed domain preparation for shared transactional admission. Adding a new
//! reviewed adapter/profile extends this boundary, not SQL queue/replay logic.
//! This is NOT an adapter loader, capability registry, or execution permission.
use super::{ports::QueuedInput, validation as policy, *};
use crate::execution::{ports::RunSeed, validation as common};

// No Debug/Serialize: native-only requests/arguments contain private prompts.
#[derive(Clone, Copy)]
pub enum StartRequest<'a> {
    Fake(&'a StartFakeRunRequest),
    CodexText(&'a StartCodexRunRequest),
}
impl StartRequest<'_> {
    pub fn workspace_id(&self) -> &str {
        match self {
            Self::Fake(r) => &r.workspace_id,
            Self::CodexText(r) => &r.workspace_id,
        }
    }
    pub fn session_id(&self) -> &str {
        match self {
            Self::Fake(r) => &r.session_id,
            Self::CodexText(r) => &r.session_id,
        }
    }
}

/// Created only after typed policy/session validation INSIDE the store writer
/// transaction. Private fields prevent callers substituting a provider or input
/// after preparation. This is durable-admission data, never a dispatch arm.
pub struct PreparedAdmission {
    workspace_id: Id,
    session_id: Id,
    request_id: Id,
    command: CommandName,
    arguments: String,
    input: QueuedInput,
}
impl PreparedAdmission {
    pub fn validate(request: StartRequest<'_>, session: &SessionSnapshot) -> DomainResult<Self> {
        let (request_id, command, arguments, input) = match request {
            StartRequest::Fake(r) => {
                let input = QueuedInput::from_fake(r, session)?;
                (
                    r.request_id.clone(),
                    CommandName::StartFakeRunV2,
                    common::canonical_arguments(r)?,
                    input,
                )
            }
            StartRequest::CodexText(r) => {
                let input = QueuedInput::from_codex(r, session)?;
                (
                    r.request_id.clone(),
                    CommandName::StartCodexRunV2,
                    common::canonical_arguments(r)?,
                    input,
                )
            }
        };
        Ok(Self {
            workspace_id: request.workspace_id().into(),
            session_id: request.session_id().into(),
            request_id,
            command,
            arguments,
            input,
        })
    }
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn command(&self) -> CommandName {
        self.command
    }
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
    pub fn input(&self) -> &QueuedInput {
        &self.input
    }

    /// Seeds are native-owned, never canonical request arguments. The store
    /// MUST look up exact receipt replay before asking to materialize new IDs.
    pub fn materialize(&self, seed: RunSeed, enqueue_order: Counter) -> DomainResult<CreatedRun> {
        common::validate_id(&seed.run_id)?;
        common::validate_id(&seed.event_id)?;
        common::validate_timestamp(&seed.created_at)?;
        common::validate_counter(enqueue_order)?;
        let provider = self.input.provider();
        let (model_id, source) = match provider {
            ProviderId::Fake => (None, EventSource::Fixture),
            ProviderId::Codex => (Some(CodexModelId::Gpt61Sol), EventSource::System),
        };
        let snapshot = RunSnapshot {
            contract_version: 2,
            workspace_id: self.workspace_id.clone(),
            session_id: self.session_id.clone(),
            run_id: seed.run_id,
            provider_id: provider,
            profile_id: self.input.profile(),
            model_id,
            state: RunState::Queued,
            last_seq: 1,
            created_at: seed.created_at.clone(),
            updated_at: seed.created_at.clone(),
            enqueue_order,
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_output_tokens: None,
            },
            terminal_error: None,
        };
        let event = RunEvent::from_parts(
            EventHeader {
                contract_version: 2,
                event_id: seed.event_id,
                workspace_id: snapshot.workspace_id.clone(),
                session_id: snapshot.session_id.clone(),
                run_id: snapshot.run_id.clone(),
                provider_id: provider,
                source,
                seq: 1,
                occurred_at: seed.created_at,
            },
            EventPayload::RunCreated(RunCreatedPayload {
                state: "queued".into(),
            }),
        );
        policy::validate_snapshot(&snapshot)?;
        policy::validate_event(&event)?;
        Ok(CreatedRun { snapshot, event })
    }
}

// No Debug/Serialize to avoid accidental logging of future event payloads.
pub struct CreatedRun {
    pub snapshot: RunSnapshot,
    pub event: RunEvent,
}
