//! Provider policy is checked here BEFORE structural normalization. Existing v1
//! geometry, identity, transition, paging and budget validators remain the shared
//! core, not a second reducer with a subtly different transition matrix.
use super::*;
use crate::execution::{self as v1, validation as old};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

fn require(ok: bool) -> DomainResult {
    if ok {
        Ok(())
    } else {
        Err(old::error(
            "validation_failed",
            "Invalid v2 execution data.",
        ))
    }
}
fn version(value: u64) -> DomainResult {
    if value == 2 {
        Ok(())
    } else {
        Err(old::error(
            "unsupported_contract_version",
            "Unsupported execution contract version.",
        ))
    }
}

// Internal structural adapter, NOT a provider-admission route. Never return this
// normalized fake value to storage or persist it instead of the actual v2 data.
fn structural<T: DeserializeOwned>(value: &impl Serialize) -> DomainResult<T> {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(fields) => {
                if fields.contains_key("contract_version") {
                    fields.insert("contract_version".into(), Value::from(1));
                }
                if fields.contains_key("provider_id") {
                    fields.insert("provider_id".into(), Value::from("fake"));
                }
                if fields.get("kind").and_then(Value::as_str) == Some("run_created") || fields.get("source").and_then(Value::as_str) == Some("provider") {
                    fields.insert("source".into(), Value::from("fixture"));
                }
                for key in [
                    "profile_id",
                    "model_id",
                    "cached_input_tokens",
                    "cache_write_input_tokens",
                    "reasoning_output_tokens",
                    "codex_availability",
                ] {
                    fields.remove(key);
                }
                for item in fields.values_mut() {
                    walk(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let mut json = serde_json::to_value(value)
        .map_err(|_| old::error("validation_failed", "Invalid v2 execution data."))?;
    walk(&mut json);
    serde_json::from_value(json)
        .map_err(|_| old::error("validation_failed", "Invalid v2 execution data."))
}

pub fn validate_session(value: &SessionSnapshot) -> DomainResult {
    version(value.contract_version)?;
    old::validate_session(&structural::<v1::SessionSnapshot>(value)?)
}

pub fn validate_usage(value: &Usage, provider: ProviderId, reported: bool) -> DomainResult {
    old::validate_usage(&structural::<v1::Usage>(value)?)?;
    for n in [
        value.cached_input_tokens,
        value.cache_write_input_tokens,
        value.reasoning_output_tokens,
    ]
    .into_iter()
    .flatten()
    {
        old::validate_counter(n)?;
    }
    match provider {
        ProviderId::Fake => require(
            value.cached_input_tokens.is_none()
                && value.cache_write_input_tokens.is_none()
                && value.reasoning_output_tokens.is_none(),
        ),
        ProviderId::Codex => {
            require(!value.estimated)?;
            let complete = value.input_tokens.is_some()
                && value.output_tokens.is_some()
                && value.cached_input_tokens.is_some()
                && value.reasoning_output_tokens.is_some();
            let unknown = value.input_tokens.is_none()
                && value.output_tokens.is_none()
                && value.cached_input_tokens.is_none()
                && value.reasoning_output_tokens.is_none()
                && value.cache_write_input_tokens.is_none();
            require(complete || (!reported && unknown))
        }
    }
}

pub fn validate_snapshot(value: &RunSnapshot) -> DomainResult {
    version(value.contract_version)?;
    match value.provider_id {
        ProviderId::Fake => require(value.profile_id.is_none() && value.model_id.is_none())?,
        ProviderId::Codex => require(
            matches!(
                value.profile_id,
                Some(CodexProfileId::CodexTextV1 | CodexProfileId::CodexTextV2)
            ) && value.model_id == Some(CodexModelId::Gpt61Sol)
                && !matches!(
                    value.state,
                    RunState::WaitingInput | RunState::WaitingApproval
                ),
        )?,
    }
    validate_usage(
        &value.usage,
        value.provider_id,
        value.provider_id == ProviderId::Codex && value.state == RunState::Succeeded,
    )?;
    old::validate_snapshot(&structural::<v1::RunSnapshot>(value)?)
}

pub fn validate_event(value: &RunEvent) -> DomainResult {
    let h = value.header();
    version(h.contract_version)?;
    if h.provider_id == ProviderId::Codex {
        match value {
            RunEvent::TextDelta { payload, .. } => require(
                h.source == EventSource::Provider
                    && payload.stream == TextStream::Assistant
                    && payload.text.len() <= CODEX_COMPLETION_CHUNK_BYTES,
            )?,
            RunEvent::UsageUpdated { payload, .. } => {
                require(h.source == EventSource::Provider)?;
                validate_usage(payload, h.provider_id, true)?;
            }
            RunEvent::ToolActivity { .. } => {
                return Err(old::error(
                    "provider_activity_disabled",
                    "Tools are disabled in this Codex profile.",
                ))
            }
            RunEvent::RunStateChanged { payload, .. } => require(
                h.source == EventSource::System
                    && !matches!(
                        payload.from,
                        RunState::WaitingInput | RunState::WaitingApproval
                    )
                    && !matches!(
                        payload.to,
                        RunState::WaitingInput | RunState::WaitingApproval
                    ),
            )?,
            RunEvent::RunRecovered { payload, .. } => require(
                h.source == EventSource::System
                    && !matches!(
                        payload.previous_state,
                        ActiveState::WaitingInput | ActiveState::WaitingApproval
                    ),
            )?,
            _ => require(h.source == EventSource::System)?,
        }
    } else {
        require(h.source != EventSource::Provider)?;
        if matches!(value, RunEvent::RunCreated { .. }) {
            require(h.source == EventSource::Fixture)?;
        }
        if let RunEvent::UsageUpdated { payload, .. } = value {
            validate_usage(payload, h.provider_id, false)?;
        }
    }
    // Check actual encoded bytes BEFORE normalization removes profile metadata.
    let size = serde_json::to_vec(value)
        .map_err(|_| old::error("validation_failed", "Invalid v2 event."))?
        .len();
    require(
        size <= if matches!(value, RunEvent::TextDelta { .. }) {
            EVENT_BYTES
        } else {
            NONTEXT_EVENT_BYTES
        },
    )?;
    old::validate_event(&structural::<v1::RunEvent>(value)?)
}

pub fn apply_event(snapshot: &RunSnapshot, event: &RunEvent) -> DomainResult<RunSnapshot> {
    validate_snapshot(snapshot)?;
    validate_event(event)?;
    let h = event.header();
    require(h.provider_id == snapshot.provider_id)?;
    if snapshot.provider_id == ProviderId::Codex
        && matches!(
            event,
            RunEvent::TextDelta { .. } | RunEvent::UsageUpdated { .. }
        )
    {
        require(snapshot.state == RunState::Running)?;
    }
    let derived = old::apply_event(
        &structural::<v1::RunSnapshot>(snapshot)?,
        &structural::<v1::RunEvent>(event)?,
    )?;
    let mut next = snapshot.clone();
    next.state = serde_json::from_value(
        serde_json::to_value(derived.state)
            .map_err(|_| old::error("validation_failed", "Invalid v2 state."))?,
    )
    .map_err(|_| old::error("validation_failed", "Invalid v2 state."))?;
    next.last_seq = derived.last_seq;
    next.updated_at = derived.updated_at;
    next.terminal_error = derived.terminal_error.map(|e| Error {
        code: e.code,
        user_message: e.user_message,
        recoverable: e.recoverable,
    });
    if let RunEvent::UsageUpdated { payload, .. } = event {
        next.usage = payload.clone();
    }
    validate_snapshot(&next)?;
    Ok(next)
}

fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::Interrupted
    )
}

/// Reserved control events: constrained stop/recovery capacity, never spare
/// ordinary capacity. Same classification as frozen v1.
pub fn is_reserved_event(event: &RunEvent) -> bool {
    match event {
        RunEvent::OutputTruncated { .. } | RunEvent::RunRecovered { .. } => true,
        RunEvent::RunStateChanged { payload, .. } => {
            payload.to == RunState::CancelRequested || terminal(payload.to)
        }
        _ => false,
    }
}

/// Batch-local append policy: identity/sequence, profile rules via apply_event,
/// pending/control encoded-byte budgets, one-shot truncation that atomically
/// stops, and exact derived-snapshot parity. Cumulative per-run budgets are
/// enforced by the store against committed counters in the same transaction.
pub fn validate_append(current: &RunSnapshot, batch: &super::ports::AppendBatch) -> DomainResult {
    validate_snapshot(current)?;
    require(
        batch.workspace_id == current.workspace_id
            && batch.run_id == current.run_id
            && batch.expected_last_seq == current.last_seq
            && batch.expected_state == current.state,
    )?;
    require(!batch.events.is_empty() && batch.events.len() <= PAGE_EVENTS)?;
    let mut derived = current.clone();
    let mut ids = std::collections::HashSet::new();
    let mut scopes = std::collections::HashSet::new();
    let (mut ordinary_bytes, mut control_bytes, mut control_events) = (0usize, 0usize, 0usize);
    let mut truncated = false;
    for event in &batch.events {
        let bytes = serde_json::to_vec(event)
            .map_err(|_| old::error("validation_failed", "Invalid v2 event."))?
            .len();
        if is_reserved_event(event) {
            control_bytes += bytes;
            control_events += 1;
        } else {
            // Ordinary production cannot follow a truncation marker.
            require(!truncated)?;
            ordinary_bytes += bytes;
        }
        if let RunEvent::OutputTruncated { payload, .. } = event {
            require(scopes.insert(payload.scope.as_str()))?;
            truncated = true;
        }
        require(ids.insert(event.header().event_id))?;
        derived = apply_event(&derived, event)?;
    }
    require(
        ordinary_bytes <= PENDING_BYTES
            && control_bytes <= CONTROL_PENDING_BYTES
            && control_events <= RESERVED_EVENTS,
    )?;
    if truncated {
        require(matches!(
            derived.state,
            RunState::CancelRequested
                | RunState::Cancelled
                | RunState::Failed
                | RunState::Interrupted
        ))?;
    }
    require(derived == batch.next_snapshot)
}

pub fn validate_layout(
    value: &Layout,
    workspace_id: &str,
    sessions: &[SessionSnapshot],
) -> DomainResult {
    for s in sessions {
        validate_session(s)?;
    }
    old::validate_layout(
        &structural::<v1::Layout>(value)?,
        workspace_id,
        &sessions
            .iter()
            .map(structural::<v1::SessionSnapshot>)
            .collect::<DomainResult<Vec<_>>>()?,
    )
}

pub fn validate_workspace(value: &WorkspaceSnapshot) -> DomainResult {
    version(value.contract_version)?;
    for s in &value.sessions {
        validate_session(s)?;
    }
    for r in &value.runs {
        validate_snapshot(r)?;
        require(
            value
                .sessions
                .iter()
                .any(|s| s.session_id == r.session_id && s.provider_id == r.provider_id),
        )?;
    }
    if value.codex_availability == CodexAvailability::Quarantined {
        require(!value.execution_available)?;
    }
    old::validate_workspace_snapshot(&structural::<v1::WorkspaceSnapshot>(value)?)
}

fn prompt(value: &str, limit: usize) -> DomainResult {
    require(
        !value.is_empty()
            && value.len() <= limit
            && !value
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t')),
    )
}

pub fn validate_create_session(value: &CreateSessionRequest) -> DomainResult {
    version(value.contract_version)?;
    old::validate_create_session(&v1::CreateSessionRequest {
        contract_version: 1,
        workspace_id: value.workspace_id.clone(),
        request_id: value.request_id.clone(),
        label: value.label.clone(),
    })
}

pub fn validate_fake_start(value: &StartFakeRunRequest) -> DomainResult {
    version(value.contract_version)?;
    if value.provider_id != ProviderId::Fake {
        return Err(old::error(
            "provider_execution_disabled",
            "This command starts simulated runs only.",
        ));
    }
    old::validate_start_fake_run(&structural::<v1::StartFakeRunRequest>(value)?)
}

pub fn validate_fake_session(
    value: &StartFakeRunRequest,
    session: &SessionSnapshot,
) -> DomainResult {
    validate_fake_start(value)?;
    validate_session(session)?;
    require(
        session.provider_id == ProviderId::Fake
            && session.workspace_id == value.workspace_id
            && session.session_id == value.session_id,
    )
}

pub fn validate_codex_start(value: &StartCodexRunRequest) -> DomainResult {
    version(value.contract_version)?;
    for id in [&value.workspace_id, &value.session_id, &value.request_id] {
        old::validate_id(id)?;
    }
    require(value.confirmed)?;
    // v1 never carries context; v2 always carries 1..=6 distinct earlier-run ids
    // (a first turn with nothing to carry uses v1). Same-session/succeeded rules
    // need the journal and are enforced in the admission transaction and audit.
    match (value.profile_id, value.context_run_ids.as_deref()) {
        (CodexProfileId::CodexTextV1, None) => {}
        (CodexProfileId::CodexTextV2, Some(ids)) => {
            require(!ids.is_empty() && ids.len() <= CODEX_CONTEXT_RUNS)?;
            for (index, id) in ids.iter().enumerate() {
                old::validate_id(id)?;
                require(!ids[..index].contains(id) && *id != value.request_id)?;
            }
        }
        _ => return Err(old::error("validation_failed", "Invalid v2 execution data.")),
    }
    prompt(&value.prompt, CODEX_PROMPT_BYTES)
}

/// A preview is a read: no consent flag, no request identity, nothing is admitted.
pub fn validate_preview_context(value: &PreviewCodexContextRequest) -> DomainResult {
    version(value.contract_version)?;
    for id in [&value.workspace_id, &value.session_id] {
        old::validate_id(id)?;
    }
    let ids = &value.context_run_ids;
    require(!ids.is_empty() && ids.len() <= CODEX_CONTEXT_RUNS)?;
    for (index, id) in ids.iter().enumerate() {
        old::validate_id(id)?;
        require(!ids[..index].contains(id))?;
    }
    prompt(&value.prompt, CODEX_PROMPT_BYTES)
}

pub fn validate_codex_session(
    value: &StartCodexRunRequest,
    session: &SessionSnapshot,
) -> DomainResult {
    validate_codex_start(value)?;
    validate_session(session)?;
    require(
        session.provider_id == ProviderId::Codex
            && session.workspace_id == value.workspace_id
            && session.session_id == value.session_id,
    )
}

pub fn validate_cancel(value: &CancelRunRequest) -> DomainResult {
    version(value.contract_version)?;
    old::validate_cancel(&structural::<v1::CancelRunRequest>(value)?)
}
pub fn validate_workspace_request(value: &WorkspaceRequest) -> DomainResult {
    version(value.contract_version)?;
    old::validate_workspace_request(&structural::<v1::WorkspaceRequest>(value)?)
}
pub fn validate_event_page(value: &EventPageRequest) -> DomainResult {
    version(value.contract_version)?;
    old::validate_event_page(&structural::<v1::EventPageRequest>(value)?)
}
pub fn validate_save_layout(
    value: &SaveLayoutRequest,
    sessions: &[SessionSnapshot],
) -> DomainResult {
    version(value.contract_version)?;
    for s in sessions {
        validate_session(s)?;
    }
    old::validate_save_layout(
        &structural::<v1::SaveLayoutRequest>(value)?,
        &sessions
            .iter()
            .map(structural::<v1::SessionSnapshot>)
            .collect::<DomainResult<Vec<_>>>()?,
    )
}

pub fn validate_bootstrap(value: &BootstrapResponse) -> DomainResult {
    version(value.contract_version)?;
    match value.storage_state {
        StorageState::UpgradeRequired => {
            require(value.snapshot.is_none())?;
            old::validate_id(
                value
                    .upgrade_id
                    .as_deref()
                    .ok_or_else(|| old::error("validation_failed", "Missing upgrade identity."))?,
            )
        }
        StorageState::Ready => {
            require(value.upgrade_id.is_none())?;
            validate_workspace(
                value
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| old::error("validation_failed", "Missing workspace."))?,
            )
        }
    }
}

pub fn validate_confirm_upgrade(value: &ConfirmUpgradeRequest) -> DomainResult {
    version(value.contract_version)?;
    old::validate_id(&value.upgrade_id)?;
    old::validate_id(&value.request_id)?;
    require(value.confirmed)
}

/// The producer still must prove successful process/cleanup before calling this.
/// UTF-8-safe chunks keep worst-case JSON escaping under the existing event cap.
pub fn completion_chunks(text: &str) -> DomainResult<Vec<&str>> {
    require(!text.is_empty() && text.len() <= CODEX_MESSAGE_BYTES)?;
    let mut rest = text;
    let mut result = Vec::new();
    while !rest.is_empty() {
        let mut end = rest.len().min(CODEX_COMPLETION_CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        result.push(&rest[..end]);
        rest = &rest[end..];
    }
    Ok(result)
}
