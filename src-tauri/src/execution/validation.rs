//! Native semantic rules that JSON shape/Serde alone cannot enforce.
use super::*;
use serde::Serialize;
use std::collections::HashSet;

pub const JS_MAX_INTEGER: u64 = 9_007_199_254_740_991;
pub type ValidationResult<T = ()> = Result<T, Error>;

pub fn error(code: &str, message: &str) -> Error {
    Error {
        code: code.into(),
        user_message: message.into(),
        recoverable: true,
    }
}
fn require(ok: bool, message: &str) -> ValidationResult {
    if ok {
        Ok(())
    } else {
        Err(error("validation_failed", message))
    }
}
/// Tauri wrappers decode the request Value here so malformed inner DTOs return
/// sanitized structured errors rather than exposing Serde input/exception text.
pub fn decode_request<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> ValidationResult<T> {
    serde_json::from_value(value)
        .map_err(|_| error("validation_failed", "Invalid execution request shape."))
}
pub fn validate_id(value: &str) -> ValidationResult {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| error("validation_failed", "Expected a UUID v4 identity."))?;
    require(
        parsed.get_version_num() == 4
            && parsed.get_variant() == uuid::Variant::RFC4122
            && parsed.to_string() == value,
        "Expected a canonical lowercase UUID v4 identity.",
    )
}
pub fn validate_timestamp(value: &str) -> ValidationResult {
    require(
        value.len() <= 40
            && value.ends_with('Z')
            && chrono::DateTime::parse_from_rfc3339(value).is_ok(),
        "Expected a bounded UTC RFC3339 timestamp.",
    )
}
pub fn validate_version(version: u64) -> ValidationResult {
    if version == 1 {
        Ok(())
    } else {
        Err(error(
            "unsupported_contract_version",
            "Unsupported execution contract version.",
        ))
    }
}
pub fn validate_fake(provider: ProviderId) -> ValidationResult {
    if provider == ProviderId::Fake {
        Ok(())
    } else {
        Err(error(
            "provider_execution_disabled",
            "Only the explicitly simulated fake provider is enabled.",
        ))
    }
}
pub fn validate_counter(value: u64) -> ValidationResult {
    require(
        value <= JS_MAX_INTEGER,
        "Counter exceeds the JSON-safe integer range.",
    )
}
pub fn canonical_arguments<T: Serialize>(value: &T) -> ValidationResult<String> {
    // Explicitly sort keys even if a downstream dependency enables preserve_order.
    // Call only after DTO validation. Array and Unicode order is preserved.
    let mut value = serde_json::to_value(value)
        .map_err(|_| error("validation_failed", "Cannot encode request arguments."))?;
    normalize_numbers(&mut value);
    serde_json::to_string(&value)
        .map_err(|_| error("validation_failed", "Cannot encode request arguments."))
}
fn normalize_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        serde_json::Value::Object(fields) => {
            fields.values_mut().for_each(normalize_numbers);
            fields.sort_keys();
        }
        serde_json::Value::Number(number) => {
            if let Some(n) = number.as_f64() {
                if n >= 0.0 && n <= JS_MAX_INTEGER as f64 && n.fract() == 0.0 {
                    *value = serde_json::json!(n as u64);
                }
            }
        }
        _ => {}
    }
}
pub fn validate_workspace_request(request: &WorkspaceRequest) -> ValidationResult {
    validate_version(request.contract_version)?;
    validate_id(&request.workspace_id)
}
pub fn validate_create_session(request: &CreateSessionRequest) -> ValidationResult {
    validate_version(request.contract_version)?;
    validate_id(&request.workspace_id)?;
    validate_id(&request.request_id)?;
    require(
        (1..=128).contains(&request.label.chars().count())
            && !request.label.chars().any(char::is_control),
        "Session label must contain 1–128 non-control characters.",
    )
}
pub fn validate_start_fake_run(request: &StartFakeRunRequest) -> ValidationResult {
    validate_version(request.contract_version)?;
    validate_fake(request.provider_id)?;
    for id in [
        &request.workspace_id,
        &request.session_id,
        &request.request_id,
    ] {
        validate_id(id)?;
    }
    require(
        !request.prompt.is_empty()
            && request.prompt.len() <= PROMPT_BYTES
            && !request
                .prompt
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "Prompt must contain 1–65536 UTF-8 bytes, without unsupported control characters.",
    )
}
pub fn validate_cancel(request: &CancelRunRequest) -> ValidationResult {
    validate_version(request.contract_version)?;
    for id in [&request.workspace_id, &request.run_id, &request.request_id] {
        validate_id(id)?;
    }
    Ok(())
}
pub fn validate_event_page(request: &EventPageRequest) -> ValidationResult {
    validate_version(request.contract_version)?;
    validate_id(&request.workspace_id)?;
    validate_id(&request.run_id)?;
    validate_counter(request.after_seq)?;
    if let Some(through) = request.through_seq {
        validate_counter(through)?;
        require(
            request.after_seq <= through,
            "Event cursor exceeds the pinned high-water mark.",
        )?;
    }
    require(
        (1..=PAGE_EVENTS as u64).contains(&request.limit),
        "Event page size must be 1–200.",
    )
}
pub fn validate_error(value: &Error) -> ValidationResult {
    require(
        (1..=64).contains(&value.code.chars().count())
            && (1..=2048).contains(&value.user_message.chars().count()),
        "Structured error fields exceed their bounds.",
    )
}
pub fn validate_usage(usage: &Usage) -> ValidationResult {
    for count in [usage.input_tokens, usage.output_tokens]
        .into_iter()
        .flatten()
    {
        validate_counter(count)?;
    }
    Ok(())
}
pub fn is_terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::Interrupted
    )
}
pub fn validate_session(snapshot: &SessionSnapshot) -> ValidationResult {
    validate_version(snapshot.contract_version)?;
    validate_fake(snapshot.provider_id)?;
    validate_id(&snapshot.workspace_id)?;
    validate_id(&snapshot.session_id)?;
    validate_timestamp(&snapshot.created_at)?;
    require(
        (1..=128).contains(&snapshot.label.chars().count())
            && !snapshot.label.chars().any(char::is_control),
        "Invalid session label.",
    )
}
pub fn validate_workspace_snapshot(snapshot: &WorkspaceSnapshot) -> ValidationResult {
    validate_version(snapshot.contract_version)?;
    let workspace = &snapshot.workspace;
    validate_id(&workspace.workspace_id)?;
    validate_timestamp(&workspace.created_at)?;
    require(
        (1..=128).contains(&workspace.label.chars().count())
            && !workspace.label.chars().any(char::is_control),
        "Invalid workspace label.",
    )?;
    require(
        snapshot.sessions.len() <= WORKSPACE_SESSIONS && snapshot.runs.len() <= WORKSPACE_RUNS,
        "Workspace history capacity exceeded.",
    )?;
    let mut sessions = HashSet::new();
    for session in &snapshot.sessions {
        validate_session(session)?;
        require(
            session.workspace_id == workspace.workspace_id && sessions.insert(&session.session_id),
            "Duplicate or foreign session.",
        )?;
    }
    let mut runs = HashSet::new();
    let mut orders = HashSet::new();
    for run in &snapshot.runs {
        validate_snapshot(run)?;
        require(
            run.workspace_id == workspace.workspace_id
                && sessions.contains(&run.session_id)
                && runs.insert(&run.run_id)
                && orders.insert(run.enqueue_order),
            "Duplicate or foreign run/order.",
        )?;
    }
    let queued = snapshot
        .runs
        .iter()
        .filter(|run| run.state == RunState::Queued)
        .count();
    let active = snapshot
        .runs
        .iter()
        .filter(|run| run.state != RunState::Queued && !is_terminal(run.state))
        .count();
    require(
        queued <= QUEUED_RUNS && active <= 1,
        "Workspace queue/active slot invariant failed.",
    )?;
    if let Some(layout) = &snapshot.layout {
        validate_layout(layout, &workspace.workspace_id, &snapshot.sessions)?;
    }
    if let Some(warning) = &snapshot.layout_warning {
        validate_error(warning)?;
    }
    require(
        snapshot.layout.is_some() != snapshot.layout_warning.is_some(),
        "Return either a valid layout or a preserved-layout warning.",
    )
}
pub fn validate_snapshot(snapshot: &RunSnapshot) -> ValidationResult {
    validate_version(snapshot.contract_version)?;
    validate_fake(snapshot.provider_id)?;
    for id in [
        &snapshot.workspace_id,
        &snapshot.session_id,
        &snapshot.run_id,
    ] {
        validate_id(id)?;
    }
    validate_timestamp(&snapshot.created_at)?;
    validate_timestamp(&snapshot.updated_at)?;
    validate_counter(snapshot.last_seq)?;
    validate_counter(snapshot.enqueue_order)?;
    validate_usage(&snapshot.usage)?;
    if let Some(value) = &snapshot.terminal_error {
        validate_error(value)?;
    }
    require(
        snapshot.last_seq > 0
            && snapshot.enqueue_order > 0
            && snapshot.state != RunState::WaitingApproval,
        "Invalid fake run snapshot.",
    )?;
    require(
        (snapshot.state == RunState::Failed) == snapshot.terminal_error.is_some(),
        "Only a failed run carries its required terminal error.",
    )
}
pub fn validate_event(event: &RunEvent) -> ValidationResult {
    let header = event.header();
    validate_version(header.contract_version)?;
    validate_fake(header.provider_id)?;
    for id in [
        &header.workspace_id,
        &header.session_id,
        &header.run_id,
        &header.event_id,
    ] {
        validate_id(id)?;
    }
    validate_timestamp(&header.occurred_at)?;
    validate_counter(header.seq)?;
    require(
        header.seq > 0 && header.source != EventSource::Provider,
        "Invalid fake event source or sequence.",
    )?;
    let size = serde_json::to_vec(event)
        .map_err(|_| error("validation_failed", "Cannot encode event."))?
        .len();
    let limit = if matches!(event, RunEvent::TextDelta { .. }) {
        EVENT_BYTES
    } else {
        NONTEXT_EVENT_BYTES
    };
    require(size <= limit, "Encoded event exceeds its byte budget.")?;
    match event {
        RunEvent::RunCreated { payload, .. } => require(
            header.seq == 1 && payload.state == "queued" && header.source == EventSource::Fixture,
            "Invalid queued creation event.",
        ),
        RunEvent::RunStateChanged { payload, .. } => {
            if let Some(value) = &payload.error {
                validate_error(value)?;
            }
            require(
                payload.from != RunState::WaitingApproval
                    && payload.to != RunState::WaitingApproval
                    && payload.from.can_transition_to(payload.to),
                "Illegal fake run transition.",
            )?;
            require(
                (payload.to == RunState::Failed) == payload.error.is_some(),
                "Only failure transitions carry a required error.",
            )
        }
        RunEvent::TextDelta { payload, .. } => require(
            !payload.text.is_empty()
                && payload.text.len() <= TEXT_DELTA_BYTES
                && header.source == EventSource::Fixture,
            "Invalid simulated text delta.",
        ),
        RunEvent::ToolActivity { payload, .. } => require(
            (1..=128).contains(&payload.tool_call_id.chars().count())
                && (1..=256).contains(&payload.title.chars().count())
                && header.source == EventSource::Fixture,
            "Invalid simulated tool activity.",
        ),
        RunEvent::UsageUpdated { payload, .. } => validate_usage(payload),
        RunEvent::OutputTruncated { payload, .. } => {
            validate_counter(payload.discarded_events)?;
            if let Some(bytes) = payload.discarded_bytes {
                validate_counter(bytes)?;
            }
            require(
                header.source == EventSource::System
                    && match payload.scope {
                        TruncationScope::EventCount => {
                            payload.discarded_bytes.is_none() && payload.discarded_events > 0
                        }
                        _ => {
                            payload.discarded_bytes.is_some_and(|bytes| bytes > 0)
                                && payload.discarded_events > 0
                        }
                    },
                "Invalid one-shot truncation counters or source.",
            )
        }
        RunEvent::RunRecovered { payload, .. } => require(
            header.source == EventSource::System
                && payload.state == "interrupted"
                && payload.previous_state != ActiveState::WaitingApproval,
            "Recovery must be a system interruption.",
        ),
    }
}
pub fn is_reserved_event(event: &RunEvent) -> bool {
    match event {
        RunEvent::OutputTruncated { .. } | RunEvent::RunRecovered { .. } => true,
        RunEvent::RunStateChanged { payload, .. } => {
            payload.to == RunState::CancelRequested || is_terminal(payload.to)
        }
        _ => false,
    }
}
pub fn apply_event(snapshot: &RunSnapshot, event: &RunEvent) -> ValidationResult<RunSnapshot> {
    validate_snapshot(snapshot)?;
    validate_event(event)?;
    let h = event.header();
    require(
        !is_terminal(snapshot.state)
            && h.workspace_id == snapshot.workspace_id
            && h.session_id == snapshot.session_id
            && h.run_id == snapshot.run_id
            && h.provider_id == snapshot.provider_id
            && h.seq == snapshot.last_seq + 1,
        "Event identity, sequence, or terminal-state conflict.",
    )?;
    let mut next = snapshot.clone();
    match event {
        RunEvent::RunCreated { .. } => {
            return Err(error(
                "state_conflict",
                "Run creation cannot be appended twice.",
            ))
        }
        RunEvent::RunStateChanged { payload, .. } => {
            require(
                payload.from == snapshot.state,
                "Transition precondition does not match.",
            )?;
            next.state = payload.to;
            next.terminal_error = payload.error.clone();
        }
        RunEvent::RunRecovered { payload, .. } => {
            require(
                payload.previous_state.as_str() == snapshot.state.as_str()
                    && snapshot.state.can_transition_to(RunState::Interrupted),
                "Recovery precondition does not match.",
            )?;
            next.state = RunState::Interrupted;
            next.terminal_error = None;
        }
        RunEvent::UsageUpdated { payload, .. } => next.usage = payload.clone(),
        RunEvent::TextDelta { .. } | RunEvent::ToolActivity { .. } => {
            require(
                matches!(
                    snapshot.state,
                    RunState::Running | RunState::WaitingInput | RunState::CancelRequested
                ),
                "Output requires an executing run.",
            )?;
        }
        RunEvent::OutputTruncated { .. } => {}
    }
    next.last_seq = h.seq;
    next.updated_at = h.occurred_at;
    validate_snapshot(&next)?;
    Ok(next)
}
pub fn validate_append(current: &RunSnapshot, batch: &ports::AppendBatch) -> ValidationResult {
    require(
        batch.workspace_id == current.workspace_id
            && batch.run_id == current.run_id
            && batch.expected_last_seq == current.last_seq
            && batch.expected_state == current.state,
        "Append precondition conflict.",
    )?;
    require(
        !batch.events.is_empty() && batch.events.len() <= PAGE_EVENTS,
        "Append batch must contain 1–200 events.",
    )?;
    let mut derived = current.clone();
    let mut ids = HashSet::new();
    let mut scopes = HashSet::new();
    let mut ordinary_bytes = 0;
    let mut control_bytes = 0;
    let mut control_events = 0;
    let mut truncated = false;
    for event in &batch.events {
        let bytes = serde_json::to_vec(event)
            .map_err(|_| error("validation_failed", "Cannot encode event."))?
            .len();
        if is_reserved_event(event) {
            control_bytes += bytes;
            control_events += 1;
        } else {
            require(
                !truncated,
                "Ordinary production cannot follow a truncation marker.",
            )?;
            ordinary_bytes += bytes;
        }
        if let RunEvent::OutputTruncated { payload, .. } = event {
            require(
                scopes.insert(payload.scope.as_str()),
                "Duplicate truncation scope within batch.",
            )?;
            truncated = true;
        }
        require(
            ids.insert(event.header().event_id),
            "Duplicate event identity within batch.",
        )?;
        derived = apply_event(&derived, event)?;
    }
    require(
        ordinary_bytes <= PENDING_BYTES
            && control_bytes <= CONTROL_PENDING_BYTES
            && control_events <= RESERVED_EVENTS,
        "Append exceeds pending/control delivery budget.",
    )?;
    if truncated {
        require(
            matches!(
                derived.state,
                RunState::CancelRequested
                    | RunState::Cancelled
                    | RunState::Failed
                    | RunState::Interrupted
            ),
            "Truncation must atomically request/finish stopping.",
        )?;
    }
    require(
        derived == batch.next_snapshot,
        "Committed snapshot must equal the event-derived snapshot.",
    )
}
pub fn validate_layout(
    layout: &Layout,
    workspace_id: &str,
    sessions: &[SessionSnapshot],
) -> ValidationResult {
    if layout.layout_version != 1 {
        return Err(error(
            "unsupported_layout_version",
            "Unsupported layout version; original data is preserved.",
        ));
    }
    validate_id(workspace_id)?;
    require(
        layout.workspace_id == workspace_id,
        "Layout belongs to another workspace.",
    )?;
    validate_counter(layout.revision)?;
    require(
        layout.sidebar_width.is_finite() && (240.0..=440.0).contains(&layout.sidebar_width),
        "Sidebar width must be finite and bounded.",
    )?;
    require(
        layout.panels.len() <= WORKSPACE_SESSIONS && layout.containers.len() <= 8,
        "Layout capacity exceeded.",
    )?;
    let mut panel_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    let mut container_ids = HashSet::new();
    for container in &layout.containers {
        validate_id(&container.container_id)?;
        require(
            container_ids.insert(&container.container_id),
            "Duplicate layout container.",
        )?;
        validate_geometry(&container.geometry)?;
    }
    for panel in &layout.panels {
        validate_id(&panel.panel_id)?;
        validate_id(&panel.session_id)?;
        require(
            panel_ids.insert(&panel.panel_id)
                && session_ids.insert(&panel.session_id)
                && !container_ids.contains(&panel.panel_id),
            "Duplicate layout identity or session reference.",
        )?;
        require(
            sessions.iter().any(|s| {
                s.workspace_id == workspace_id
                    && s.session_id == panel.session_id
                    && s.provider_id == ProviderId::Fake
            }),
            "Panel refers to an unknown session.",
        )?;
        if let Some(container) = &panel.container_id {
            require(
                container_ids.contains(container),
                "Panel refers to an unknown container.",
            )?;
        }
        validate_geometry(&panel.geometry)?;
        validate_geometry(&panel.expanded_geometry)?;
    }
    Ok(())
}
pub fn validate_save_layout(
    request: &SaveLayoutRequest,
    sessions: &[SessionSnapshot],
) -> ValidationResult {
    validate_version(request.contract_version)?;
    validate_id(&request.request_id)?;
    validate_counter(request.expected_revision)?;
    require(
        request.layout.revision == request.expected_revision,
        "Submitted layout revision must equal expected_revision.",
    )?;
    validate_layout(&request.layout, &request.workspace_id, sessions)
}
fn validate_geometry(g: &Geometry) -> ValidationResult {
    require(
        [g.x, g.y, g.width, g.height].iter().all(|n| n.is_finite())
            && (0.0..=100_000.0).contains(&g.x)
            && (0.0..=100_000.0).contains(&g.y)
            && (1.0..=100_000.0).contains(&g.width)
            && (1.0..=100_000.0).contains(&g.height),
        "Layout geometry must be finite and bounded.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures_deserialize_and_replay_through_native_rules() {
        for source in [
            include_str!("../../../contracts/fixtures/fake-success.json"),
            include_str!("../../../contracts/fixtures/fake-failed.json"),
            include_str!("../../../contracts/fixtures/fake-cancelled.json"),
            include_str!("../../../contracts/fixtures/fake-recovered.json"),
        ] {
            let value: serde_json::Value = serde_json::from_str(source).unwrap();
            let events: Vec<RunEvent> = serde_json::from_value(value["events"].clone()).unwrap();
            let expected: RunSnapshot = serde_json::from_value(value["snapshot"].clone()).unwrap();
            let mut current = expected.clone();
            current.state = RunState::Queued;
            current.last_seq = 1;
            current.updated_at = current.created_at.clone();
            current.usage = Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
            };
            current.terminal_error = None;
            validate_event(&events[0]).unwrap();
            for event in &events[1..] {
                current = apply_event(&current, event).unwrap();
            }
            assert_eq!(current, expected);
            assert!(apply_event(&current, events.last().unwrap()).is_err());
        }
    }
    #[test]
    fn strict_wire_shapes_and_disabled_providers() {
        let raw = serde_json::json!({"contract_version":1,"unexpected":true});
        assert_eq!(
            decode_request::<BootstrapRequest>(raw).unwrap_err().code,
            "validation_failed"
        );
        for provider in [ProviderId::Codex, ProviderId::Claude] {
            assert_eq!(
                validate_fake(provider).unwrap_err().code,
                "provider_execution_disabled"
            );
        }
        assert!(validate_id("01900000-0000-7000-8000-000000000001").is_err());
        assert!(validate_counter(JS_MAX_INTEGER + 1).is_err());
    }
    #[test]
    fn all_command_examples_deserialize_with_numeric_parity() {
        use serde::de::DeserializeOwned;
        fn check<T: DeserializeOwned + Serialize>(v: &serde_json::Value) {
            let typed: T = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(
                canonical_arguments(v).unwrap(),
                canonical_arguments(&typed).unwrap()
            );
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/fixtures/commands.json"))
                .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let v = &case["value"];
            match case["definition"].as_str().unwrap() {
                "bootstrap_request" => check::<BootstrapRequest>(v),
                "workspace_request" => check::<WorkspaceRequest>(v),
                "workspace_snapshot" => check::<WorkspaceSnapshot>(v),
                "create_session_request" => check::<CreateSessionRequest>(v),
                "create_session_response" => check::<CreateSessionResponse>(v),
                "start_fake_run_request" => check::<StartFakeRunRequest>(v),
                "start_fake_run_response" => check::<StartFakeRunResponse>(v),
                "cancel_run_request" => check::<CancelRunRequest>(v),
                "cancel_run_response" => check::<CancelRunResponse>(v),
                "event_page_request" => check::<EventPageRequest>(v),
                "event_page" => check::<EventPage>(v),
                "save_layout_request" => check::<SaveLayoutRequest>(v),
                "save_layout_response" => check::<SaveLayoutResponse>(v),
                "execution_changed" => check::<ExecutionChanged>(v),
                other => panic!("Untested command fixture: {other}"),
            }
        }
        for case in fixtures["native_rejections"].as_array().unwrap() {
            let request: StartFakeRunRequest =
                serde_json::from_value(case["value"].clone()).unwrap();
            assert_eq!(
                validate_start_fake_run(&request).unwrap_err().code,
                case["code"].as_str().unwrap()
            );
        }
    }
    #[test]
    fn explicit_nulls_utf8_and_fake_approval_boundaries() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/fixtures/commands.json"))
                .unwrap();
        let find = |name: &str| {
            fixtures["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["definition"] == name)
                .unwrap()["value"]
                .clone()
        };
        let mut page = find("event_page_request");
        page.as_object_mut().unwrap().remove("through_seq");
        assert!(serde_json::from_value::<EventPageRequest>(page).is_err());
        let mut request: StartFakeRunRequest =
            serde_json::from_value(find("start_fake_run_request")).unwrap();
        request.prompt = "🧪".repeat(PROMPT_BYTES / 4 + 1);
        assert!(validate_start_fake_run(&request).is_err());
        let value: StartFakeRunResponse =
            serde_json::from_value(find("start_fake_run_response")).unwrap();
        let mut snapshot = value.run;
        snapshot.state = RunState::WaitingApproval;
        assert!(validate_snapshot(&snapshot).is_err());
    }
    #[test]
    fn workspace_and_layout_relationships_are_native_validated() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/fixtures/commands.json"))
                .unwrap();
        let value = &fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["definition"] == "workspace_snapshot")
            .unwrap()["value"];
        let mut snapshot: WorkspaceSnapshot = serde_json::from_value(value.clone()).unwrap();
        validate_workspace_snapshot(&snapshot).unwrap();
        snapshot.layout.as_mut().unwrap().panels[0]
            .expanded_geometry
            .width = f64::NAN;
        assert!(validate_workspace_snapshot(&snapshot).is_err());
        snapshot = serde_json::from_value(value.clone()).unwrap();
        snapshot.runs.push(snapshot.runs[0].clone());
        assert!(validate_workspace_snapshot(&snapshot).is_err());
    }
    #[test]
    fn append_preconditions_and_snapshot_parity_are_enforced() {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../contracts/fixtures/fake-success.json"
        ))
        .unwrap();
        let mut initial: RunSnapshot = serde_json::from_value(source["snapshot"].clone()).unwrap();
        initial.state = RunState::Queued;
        initial.last_seq = 1;
        initial.updated_at = initial.created_at.clone();
        let event: RunEvent = serde_json::from_value(source["events"][1].clone()).unwrap();
        let next = apply_event(&initial, &event).unwrap();
        let mut batch = ports::AppendBatch {
            workspace_id: initial.workspace_id.clone(),
            run_id: initial.run_id.clone(),
            expected_last_seq: 1,
            expected_state: RunState::Queued,
            events: vec![event],
            next_snapshot: next,
        };
        validate_append(&initial, &batch).unwrap();
        batch.next_snapshot.state = RunState::Running;
        assert!(validate_append(&initial, &batch).is_err());
    }
    #[test]
    fn canonical_arguments_normalize_only_numeric_representation() {
        assert_eq!(
            canonical_arguments(&serde_json::json!({"x": 1, "y": 0})).unwrap(),
            canonical_arguments(&serde_json::json!({"y": -0.0, "x": 1.0})).unwrap()
        );
        assert_ne!(
            canonical_arguments(&serde_json::json!({"label": "é"})).unwrap(),
            canonical_arguments(&serde_json::json!({"label": "e\u{301}"})).unwrap()
        );
    }
    #[test]
    fn truncation_requires_atomic_stop_and_reserved_capacity() {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../contracts/fixtures/fake-success.json"
        ))
        .unwrap();
        let events: Vec<RunEvent> = serde_json::from_value(source["events"].clone()).unwrap();
        let mut running: RunSnapshot = serde_json::from_value(source["snapshot"].clone()).unwrap();
        running.state = RunState::Running;
        running.last_seq = 3;
        running.updated_at = events[2].header().occurred_at;
        let mut marker_header = events[3].header();
        marker_header.source = EventSource::System;
        let marker = RunEvent::from_parts(
            marker_header,
            EventPayload::OutputTruncated(OutputTruncatedPayload {
                scope: TruncationScope::EventCount,
                discarded_bytes: None,
                discarded_events: 1,
            }),
        );
        let marked = apply_event(&running, &marker).unwrap();
        let mut batch = ports::AppendBatch {
            workspace_id: running.workspace_id.clone(),
            run_id: running.run_id.clone(),
            expected_last_seq: 3,
            expected_state: RunState::Running,
            events: vec![marker.clone()],
            next_snapshot: marked.clone(),
        };
        assert!(is_reserved_event(&marker));
        assert!(validate_append(&running, &batch).is_err());
        let mut cancel_header = events[4].header();
        cancel_header.source = EventSource::System;
        let cancel = RunEvent::from_parts(
            cancel_header,
            EventPayload::RunStateChanged(RunStateChangedPayload {
                from: RunState::Running,
                to: RunState::CancelRequested,
                error: None,
            }),
        );
        batch.next_snapshot = apply_event(&marked, &cancel).unwrap();
        batch.events.push(cancel);
        validate_append(&running, &batch).unwrap();
    }
    #[test]
    fn uuid_variant_matrix_matches_wire_contract() {
        for nibble in 0..16 {
            let id = format!("00000000-0000-4000-{nibble:x}000-000000000001");
            assert_eq!(
                validate_id(&id).is_ok(),
                (8..=11).contains(&nibble),
                "variant nibble {nibble:x}"
            );
        }
    }
    #[test]
    fn layout_panel_container_collision_and_self_reference_are_rejected() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/fixtures/commands.json"))
                .unwrap();
        let value = &fixtures["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["definition"] == "workspace_snapshot")
            .unwrap()["value"];
        for parent_reference in [false, true] {
            let mut workspace: WorkspaceSnapshot = serde_json::from_value(value.clone()).unwrap();
            let layout = workspace.layout.as_mut().unwrap();
            let panel_id = layout.panels[0].panel_id.clone();
            layout.containers.push(ContainerLayout {
                container_id: panel_id.clone(),
                geometry: layout.panels[0].geometry.clone(),
            });
            if parent_reference {
                layout.panels[0].container_id = Some(panel_id);
            }
            assert!(
                validate_layout(
                    layout,
                    &workspace.workspace.workspace_id,
                    &workspace.sessions
                )
                .is_err(),
                "cross-kind collision, parent_reference={parent_reference}"
            );
        }
        let mut workspace: WorkspaceSnapshot = serde_json::from_value(value.clone()).unwrap();
        let layout = workspace.layout.as_mut().unwrap();
        let container_id = "00000000-0000-4000-8000-000000000008".to_owned();
        layout.containers.push(ContainerLayout {
            container_id: container_id.clone(),
            geometry: layout.panels[0].geometry.clone(),
        });
        layout.panels[0].container_id = Some(container_id);
        validate_layout(
            layout,
            &workspace.workspace.workspace_id,
            &workspace.sessions,
        )
        .unwrap();
    }
    #[test]
    fn generated_state_table_is_terminal_and_cancel_safe() {
        assert!(RunState::Queued.can_transition_to(RunState::Cancelled));
        assert!(!RunState::CancelRequested.can_transition_to(RunState::Succeeded));
        for state in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Interrupted,
        ] {
            assert!(!state.can_transition_to(RunState::Starting));
        }
    }
}
