use super::{stored::*, validation::*, *};
use crate::execution::{self as v1, validation as old};
use serde_json::{json, Value};

const NOW: &str = "2026-10-21T00:00:00.000Z";
fn id(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}
fn unknown() -> Usage {
    Usage {
        input_tokens: None,
        output_tokens: None,
        estimated: false,
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        reasoning_output_tokens: None,
    }
}
fn known() -> Usage {
    Usage {
        input_tokens: Some(100),
        output_tokens: Some(9),
        estimated: false,
        cached_input_tokens: Some(0),
        cache_write_input_tokens: None,
        reasoning_output_tokens: Some(0),
    }
}
fn session(provider: ProviderId) -> SessionSnapshot {
    SessionSnapshot {
        contract_version: 2,
        workspace_id: id(1),
        session_id: id(2),
        provider_id: provider,
        label: "Test".into(),
        created_at: NOW.into(),
    }
}
fn run(provider: ProviderId) -> RunSnapshot {
    RunSnapshot {
        contract_version: 2,
        workspace_id: id(1),
        session_id: id(2),
        run_id: id(3),
        provider_id: provider,
        state: RunState::Running,
        last_seq: 3,
        created_at: NOW.into(),
        updated_at: NOW.into(),
        usage: unknown(),
        terminal_error: None,
        enqueue_order: 1,
        profile_id: if provider == ProviderId::Codex {
            Some(CodexProfileId::CodexTextV1)
        } else {
            None
        },
        model_id: if provider == ProviderId::Codex {
            Some(CodexModelId::Gpt61Sol)
        } else {
            None
        },
    }
}
fn event(snapshot: &RunSnapshot, payload: EventPayload, source: EventSource) -> RunEvent {
    RunEvent::from_parts(
        EventHeader {
            contract_version: 2,
            workspace_id: snapshot.workspace_id.clone(),
            session_id: snapshot.session_id.clone(),
            run_id: snapshot.run_id.clone(),
            event_id: id(snapshot.last_seq + 10),
            provider_id: snapshot.provider_id,
            source,
            seq: snapshot.last_seq + 1,
            occurred_at: NOW.into(),
        },
        payload,
    )
}
fn start() -> StartCodexRunRequest {
    StartCodexRunRequest {
        contract_version: 2,
        workspace_id: id(1),
        session_id: id(2),
        request_id: id(4),
        prompt: "hi".into(),
        profile_id: CodexProfileId::CodexTextV1,
        confirmed: true,
        context_run_ids: None,
    }
}
fn text(snapshot: &RunSnapshot, body: &str, source: EventSource) -> RunEvent {
    event(
        snapshot,
        EventPayload::TextDelta(TextDeltaPayload {
            stream: TextStream::Assistant,
            text: body.into(),
        }),
        source,
    )
}
fn change(snapshot: &RunSnapshot, to: RunState) -> RunEvent {
    event(
        snapshot,
        EventPayload::RunStateChanged(RunStateChangedPayload {
            from: snapshot.state,
            to,
            error: if to == RunState::Failed {
                Some(Error {
                    code: "codex_test_failure".into(),
                    user_message: "Failed.".into(),
                    recoverable: true,
                })
            } else {
                None
            },
        }),
        EventSource::System,
    )
}

#[test]
fn explicit_codex_admission_checks_utf8_bytes_confirmation_profile_and_session() {
    let mut request = start();
    assert!(validate_codex_session(&request, &session(ProviderId::Codex)).is_ok());
    assert!(validate_codex_session(&request, &session(ProviderId::Fake)).is_err());
    request.prompt = "😀".repeat(256);
    assert!(validate_codex_start(&request).is_ok());
    request.prompt.push('x');
    assert!(validate_codex_start(&request).is_err());
    for body in ["", "\0", "\u{7f}"] {
        request.prompt = body.into();
        assert!(validate_codex_start(&request).is_err());
    }
    request.prompt = "\r\n\t".into();
    assert!(validate_codex_start(&request).is_ok());
    request.confirmed = false;
    assert!(validate_codex_start(&request).is_err());
    request.confirmed = true;
    request.contract_version = 1;
    assert_eq!(
        validate_codex_start(&request).unwrap_err().code,
        "unsupported_contract_version"
    );
}

#[test]
fn strict_codex_request_shape_has_no_path_env_cli_model_or_history_escape_hatches() {
    let value = serde_json::to_value(start()).unwrap();
    for key in [
        "cwd",
        "path",
        "model",
        "args",
        "env",
        "resume",
        "thread_id",
        "provider_id",
    ] {
        let mut bad = value.clone();
        bad[key] = json!("forbidden");
        assert!(serde_json::from_value::<StartCodexRunRequest>(bad).is_err());
    }
    for key in value.as_object().unwrap().keys() {
        let mut bad = value.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<StartCodexRunRequest>(bad).is_err(),
            "{key}"
        );
    }
    let mut bad = value;
    bad["profile_id"] = json!("unrestricted");
    assert!(serde_json::from_value::<StartCodexRunRequest>(bad).is_err());
}

#[test]
fn fake_commands_and_frozen_v1_still_reject_codex() {
    let mut request = StartFakeRunRequest {
        contract_version: 2,
        workspace_id: id(1),
        session_id: id(2),
        request_id: id(4),
        provider_id: ProviderId::Fake,
        prompt: "f".repeat(65536),
        scenario: FakeScenario::Success,
    };
    assert!(validate_fake_session(&request, &session(ProviderId::Fake)).is_ok());
    assert!(validate_fake_session(&request, &session(ProviderId::Codex)).is_err());
    request.provider_id = ProviderId::Codex;
    assert_eq!(
        validate_fake_start(&request).unwrap_err().code,
        "provider_execution_disabled"
    );
    assert!(old::validate_fake(v1::ProviderId::Codex).is_err());
    assert!(old::validate_version(2).is_err());
}

#[test]
fn codex_event_sources_tool_stderr_and_wait_states_fail_closed() {
    let r = run(ProviderId::Codex);
    assert!(validate_event(&text(&r, "answer", EventSource::Provider)).is_ok());
    assert!(validate_event(&text(&r, "answer", EventSource::Fixture)).is_err());
    assert!(validate_event(&text(&r, "answer", EventSource::System)).is_err());
    let stderr = event(
        &r,
        EventPayload::TextDelta(TextDeltaPayload {
            stream: TextStream::Stderr,
            text: "private".into(),
        }),
        EventSource::Provider,
    );
    assert!(validate_event(&stderr).is_err());
    let tool = event(
        &r,
        EventPayload::ToolActivity(ToolActivityPayload {
            tool_call_id: "a".into(),
            title: "a".into(),
            status: ToolStatus::Completed,
        }),
        EventSource::Provider,
    );
    assert_eq!(
        validate_event(&tool).unwrap_err().code,
        "provider_activity_disabled"
    );
    for state in [RunState::WaitingInput, RunState::WaitingApproval] {
        assert!(validate_event(&change(&r, state)).is_err());
    }
    let mut fake = run(ProviderId::Fake);
    assert!(validate_event(&text(&fake, "answer", EventSource::Provider)).is_err());
    fake.last_seq = 0; // event builder's next seq is1; not a persisted snapshot.
    let payload = EventPayload::RunCreated(RunCreatedPayload {
        state: "queued".into(),
    });
    assert!(validate_event(&event(&fake, payload.clone(), EventSource::Fixture)).is_ok());
    assert!(validate_event(&event(&fake, payload.clone(), EventSource::System)).is_err());
    fake.provider_id = ProviderId::Codex;
    assert!(validate_event(&event(&fake, payload, EventSource::System)).is_ok());
}

#[test]
fn usage_unknown_is_not_zero_and_success_requires_reported_counts() {
    let mut r = run(ProviderId::Codex);
    assert!(validate_snapshot(&r).is_ok());
    r.state = RunState::Succeeded;
    assert!(validate_snapshot(&r).is_err());
    r.usage = known();
    assert!(validate_snapshot(&r).is_ok());
    r.usage.cache_write_input_tokens = Some(0);
    assert!(validate_snapshot(&r).is_ok());
    r.usage.estimated = true;
    assert!(validate_snapshot(&r).is_err());
    let r = run(ProviderId::Codex);
    assert!(validate_event(&event(
        &r,
        EventPayload::UsageUpdated(unknown()),
        EventSource::Provider
    ))
    .is_err());
    assert!(validate_event(&event(
        &r,
        EventPayload::UsageUpdated(known()),
        EventSource::Provider
    ))
    .is_ok());
    let mut bad = known();
    bad.input_tokens = Some(old::JS_MAX_INTEGER + 1);
    assert!(validate_usage(&bad, ProviderId::Codex, true).is_err());
    let mut bad = unknown();
    bad.input_tokens = Some(0);
    assert!(validate_usage(&bad, ProviderId::Codex, false).is_err());
}

#[test]
fn shared_reducer_rejects_success_and_output_after_cancellation() {
    let r = run(ProviderId::Codex);
    let next = apply_event(
        &r,
        &event(
            &r,
            EventPayload::UsageUpdated(known()),
            EventSource::Provider,
        ),
    )
    .unwrap();
    let done = apply_event(&next, &change(&next, RunState::Succeeded)).unwrap();
    assert!(apply_event(&done, &text(&done, "late", EventSource::Provider)).is_err());
    let cancelled = apply_event(&r, &change(&r, RunState::CancelRequested)).unwrap();
    assert!(apply_event(&cancelled, &change(&cancelled, RunState::Succeeded)).is_err());
    assert!(apply_event(&cancelled, &text(&cancelled, "late", EventSource::Provider)).is_err());
    assert!(apply_event(&cancelled, &change(&cancelled, RunState::Cancelled)).is_ok());
    assert!(apply_event(&cancelled, &change(&cancelled, RunState::Failed)).is_ok());
}

#[test]
fn completion_chunks_preserve_unicode_and_worst_case_encoded_byte_budget() {
    for body in [
        "😀".repeat(4096),
        "\u{1}".repeat(CODEX_MESSAGE_BYTES),
        "a".repeat(2047) + "😀" + &"b".repeat(3000),
    ] {
        let parts = completion_chunks(&body).unwrap();
        assert_eq!(parts.concat(), body);
        let r = run(ProviderId::Codex);
        for part in parts {
            assert!(part.len() <= 2048);
            let e = text(&r, part, EventSource::Provider);
            assert!(validate_event(&e).is_ok());
            assert!(serde_json::to_vec(&e).unwrap().len() < EVENT_BYTES);
        }
    }
    assert!(completion_chunks("").is_err());
    assert!(completion_chunks(&"a".repeat(CODEX_MESSAGE_BYTES + 1)).is_err());
}

#[test]
fn stored_legacy_replay_retains_original_version_and_bytes() {
    let legacy: v1::RunSnapshot=serde_json::from_value(json!({"contract_version":1,"workspace_id":id(1),"session_id":id(2),"run_id":id(3),"provider_id":"fake","state":"running","last_seq":3,"created_at":NOW,"updated_at":NOW,"usage":{"input_tokens":null,"output_tokens":null,"estimated":false},"terminal_error":null,"enqueue_order":1})).unwrap();
    let raw = format!("  {}\r\n", serde_json::to_string_pretty(&legacy).unwrap());
    let stored = StoredRunSnapshot::decode(&raw).unwrap();
    assert_eq!(stored.original_json(), raw);
    let projected = stored.projected().unwrap();
    assert_eq!(projected.contract_version, 2);
    assert_eq!(projected.usage.cached_input_tokens, None);
    assert!(projected.profile_id.is_none());
    let e = v1::RunEvent::from_parts(
        v1::EventHeader {
            contract_version: 1,
            event_id: id(20),
            workspace_id: id(1),
            session_id: id(2),
            run_id: id(3),
            provider_id: v1::ProviderId::Fake,
            source: v1::EventSource::System,
            seq: 4,
            occurred_at: NOW.into(),
        },
        v1::EventPayload::RunRecovered(v1::RunRecoveredPayload {
            previous_state: v1::ActiveState::Running,
            state: "interrupted".into(),
        }),
    );
    let event = StoredRunEvent::decode(&serde_json::to_string(&e).unwrap()).unwrap();
    let recovered = apply_stored(&stored, &event).unwrap();
    assert!(
        matches!(recovered.original(),SnapshotVersion::V1(s) if s.state==v1::RunState::Interrupted)
    );
    assert_eq!(event.projected().unwrap().header().contract_version, 2);
    let receipt = StoredRunSnapshot::decode(&raw).unwrap();
    assert!(same_original_prefix(&stored, &receipt));
    let projected_record =
        StoredRunSnapshot::decode(&serde_json::to_string(&projected).unwrap()).unwrap();
    assert!(!same_original_prefix(&stored, &projected_record));
    assert!(apply_stored(&projected_record, &event).is_err());
    assert_eq!(stored.original_json(), raw);
}

#[test]
fn stored_codecs_reject_unknown_versions_shapes_wrong_profiles_and_oversize() {
    let r = run(ProviderId::Codex);
    let value = serde_json::to_value(&r).unwrap();
    assert!(StoredRunSnapshot::decode(&serde_json::to_string(&r).unwrap()).is_ok());
    for version in [0, 1, 3] {
        let mut bad = value.clone();
        bad["contract_version"] = json!(version);
        assert!(StoredRunSnapshot::decode(&bad.to_string()).is_err());
    }
    let mut bad = value.clone();
    bad["model_id"] = json!(null);
    assert!(StoredRunSnapshot::decode(&bad.to_string()).is_err());
    let mut bad = value.clone();
    bad["unexpected"] = json!(true);
    assert!(StoredRunSnapshot::decode(&bad.to_string()).is_err());
    for key in ["usage", "terminal_error", "profile_id", "model_id"] {
        let mut bad = value.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert!(StoredRunSnapshot::decode(&bad.to_string()).is_err());
    }
    assert!(StoredRunSnapshot::decode(&" ".repeat(65537)).is_err());
    assert!(StoredRunEvent::decode(&" ".repeat(EVENT_BYTES + 1)).is_err());
    let encoded = serde_json::to_string(&change(&r, RunState::CancelRequested)).unwrap();
    assert!(StoredRunEvent::decode(&encoded).is_ok());
    let padded = format!(
        "{}{}",
        encoded,
        " ".repeat(NONTEXT_EVENT_BYTES + 1 - encoded.len())
    );
    assert!(StoredRunEvent::decode(&padded).is_err());
}

#[test]
fn v2_layout_allows_codex_panels_without_weakening_v1_layout_rules() {
    let layout:Layout=serde_json::from_value(json!({"layout_version":1,"workspace_id":id(1),"revision":0,"theme":"dark","active_module":"agents","sidebar_width":300,"panels":[{"panel_id":id(5),"session_id":id(2),"container_id":null,"stage":"full","geometry":{"x":0,"y":0,"width":500,"height":400},"expanded_geometry":{"x":0,"y":0,"width":500,"height":400}}],"containers":[]})).unwrap();
    assert!(validate_layout(&layout, &id(1), &[session(ProviderId::Codex)]).is_ok());
    let mut foreign = session(ProviderId::Codex);
    foreign.workspace_id = id(99);
    assert!(validate_layout(&layout, &id(1), &[foreign]).is_err());
}

#[test]
fn upgrade_gate_shape_requires_confirmation_and_non_dispatching_bootstrap() {
    let mut b = BootstrapResponse {
        contract_version: 2,
        storage_state: StorageState::UpgradeRequired,
        upgrade_id: Some(id(6)),
        snapshot: None,
    };
    assert!(validate_bootstrap(&b).is_ok());
    b.upgrade_id = None;
    assert!(validate_bootstrap(&b).is_err());
    b.storage_state = StorageState::Ready;
    assert!(validate_bootstrap(&b).is_err());
    let mut c = ConfirmUpgradeRequest {
        contract_version: 2,
        upgrade_id: id(6),
        request_id: id(7),
        confirmed: false,
    };
    assert!(validate_confirm_upgrade(&c).is_err());
    c.confirmed = true;
    assert!(validate_confirm_upgrade(&c).is_ok());
}

#[test]
fn private_tagged_input_requires_matching_session_and_has_no_fake_codex_scenario() {
    let request = start();
    let input = ports::QueuedInput::from_codex(&request, &session(ProviderId::Codex)).unwrap();
    assert_eq!(input.provider(), ProviderId::Codex);
    assert!(matches!(
        input,
        ports::QueuedInput::CodexText {
            profile_id: CodexProfileId::CodexTextV1,
            ..
        }
    ));
    assert!(ports::QueuedInput::from_codex(&request, &session(ProviderId::Fake)).is_err());
}

#[test]
fn workspace_checks_provider_ownership_and_quarantine_separately_from_storage() {
    let layout: Layout = serde_json::from_value(json!({"layout_version":1,"workspace_id":id(1),"revision":0,"theme":"dark","active_module":"agents","sidebar_width":300,"panels":[],"containers":[]})).unwrap();
    let mut workspace = WorkspaceSnapshot {
        contract_version: 2,
        workspace: WorkspaceSnapshotRecord {
            workspace_id: id(1),
            label: "Test".into(),
            created_at: NOW.into(),
        },
        sessions: vec![session(ProviderId::Codex)],
        runs: vec![run(ProviderId::Codex)],
        layout: Some(layout),
        layout_warning: None,
        execution_available: true,
        codex_availability: CodexAvailability::NotConfigured,
    };
    assert!(validate_workspace(&workspace).is_ok());
    workspace.sessions[0].provider_id = ProviderId::Fake;
    assert!(validate_workspace(&workspace).is_err());
    workspace.sessions[0].provider_id = ProviderId::Codex;
    workspace.codex_availability = CodexAvailability::Quarantined;
    assert!(validate_workspace(&workspace).is_err());
    workspace.execution_available = false;
    assert!(validate_workspace(&workspace).is_ok());
}

#[test]
fn stored_sessions_do_not_accept_legacy_codex_or_launder_invalid_v1() {
    let s = session(ProviderId::Codex);
    assert!(StoredSession::decode(&serde_json::to_string(&s).unwrap()).is_ok());
    let mut value = serde_json::to_value(&s).unwrap();
    value["contract_version"] = json!(1);
    assert!(StoredSession::decode(&value.to_string()).is_err());
    value["provider_id"] = json!("fake");
    let raw = value.to_string();
    let stored = StoredSession::decode(&raw).unwrap();
    assert_eq!(stored.original_json(), raw);
    assert_eq!(stored.projected().unwrap().contract_version, 2);
    value["session_id"] = Value::String("not-a-uuid".into());
    assert!(StoredSession::decode(&value.to_string()).is_err());
}
