//! SQL fixtures exercise admission-independent content auditing, not a v2 runtime.
use super::*;
use crate::execution_v2::{self as v2, stored::StoredRunSnapshot, validation as policy};
use crate::storage::{audit_v2::audit_copy, upgrade_preview::migrate_copy};

struct Fixture {
    store: SqliteStore,
    _directory: TemporaryDirectory,
    workspace: String,
    legacy: RunSnapshot,
    current: v2::RunSnapshot,
}
fn add_receipt(
    conn: &Connection,
    workspace: &str,
    command: &str,
    request_id: &str,
    args: &impl Serialize,
    result: &impl Serialize,
    emergency: bool,
) {
    conn.execute(
        "INSERT INTO receipts VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            workspace,
            command,
            request_id,
            canonical_arguments(args).unwrap(),
            encode(result).unwrap(),
            i64::from(emergency)
        ],
    )
    .unwrap();
}
fn event(run: &v2::RunSnapshot, payload: v2::EventPayload) -> v2::RunEvent {
    let source = match &payload {
        v2::EventPayload::TextDelta(_) | v2::EventPayload::UsageUpdated(_) => {
            if run.provider_id == v2::ProviderId::Codex {
                v2::EventSource::Provider
            } else {
                v2::EventSource::Fixture
            }
        }
        v2::EventPayload::RunCreated(_) if run.provider_id == v2::ProviderId::Fake => {
            v2::EventSource::Fixture
        }
        _ => v2::EventSource::System,
    };
    v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(),
            workspace_id: run.workspace_id.clone(),
            session_id: run.session_id.clone(),
            run_id: run.run_id.clone(),
            provider_id: run.provider_id,
            source,
            seq: run.last_seq + 1,
            occurred_at: LATER.into(),
        },
        payload,
    )
}
fn add_event(conn: &Connection, e: &v2::RunEvent) {
    let h = e.header();
    conn.execute(
        "INSERT INTO events VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            h.event_id,
            h.workspace_id,
            h.session_id,
            h.run_id,
            h.seq,
            encode(e).unwrap()
        ],
    )
    .unwrap();
}
impl Fixture {
    fn new(provider: v2::ProviderId) -> Self {
        let directory = TemporaryDirectory::new();
        let (mut store, workspace, legacy_session) = fresh(&directory.0);
        let legacy = queued(&mut store, &workspace, &legacy_session);
        let legacy = store
            .cancel_run(
                &cancel_request(&legacy),
                Some(transition(&legacy, RunState::Cancelled)),
            )
            .unwrap()
            .result;
        migrate_copy(&mut store.conn, |_| Ok(())).unwrap();
        let request = v2::CreateSessionRequest {
            contract_version: 2,
            workspace_id: workspace.clone(),
            request_id: id(),
            label: "Audited native fixture (not execution)".into(),
            provider_id: provider,
        };
        let session = v2::SessionSnapshot {
            contract_version: 2,
            workspace_id: workspace.clone(),
            session_id: id(),
            label: request.label.clone(),
            provider_id: provider,
            created_at: NOW.into(),
        };
        store
            .conn
            .execute(
                "INSERT INTO sessions VALUES (?1,?2,?3)",
                params![session.session_id, workspace, encode(&session).unwrap()],
            )
            .unwrap();
        add_receipt(
            &store.conn,
            &workspace,
            "create_session_v2",
            &request.request_id,
            &request,
            &session,
            false,
        );
        let codex = provider == v2::ProviderId::Codex;
        let queued = v2::RunSnapshot {
            contract_version: 2,
            workspace_id: workspace.clone(),
            session_id: session.session_id.clone(),
            run_id: id(),
            provider_id: provider,
            state: v2::RunState::Queued,
            last_seq: 1,
            created_at: LATER.into(),
            updated_at: LATER.into(),
            profile_id: codex.then_some(v2::CodexProfileId::CodexTextV1),
            model_id: codex.then_some(v2::CodexModelId::Gpt61Sol),
            usage: v2::Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_output_tokens: None,
            },
            terminal_error: None,
            enqueue_order: 2,
        };
        policy::validate_snapshot(&queued).unwrap();
        let prompt = "Private UTF-8 fixture 😀";
        store
            .conn
            .execute(
                "INSERT INTO runs VALUES (?1,?2,?3,'queued',1,2,?4,?5,?6,1,0,0,0,0,0,?7,?8)",
                params![
                    queued.run_id,
                    workspace,
                    session.session_id,
                    encode(&queued).unwrap(),
                    prompt,
                    if codex { None } else { Some("success") },
                    provider.as_str(),
                    queued.profile_id.map(|p| p.as_str())
                ],
            )
            .unwrap();
        store
            .conn
            .execute("UPDATE workspaces SET last_enqueue=2", [])
            .unwrap();
        let mut seed = queued.clone();
        seed.last_seq = 0;
        add_event(
            &store.conn,
            &event(
                &seed,
                v2::EventPayload::RunCreated(v2::RunCreatedPayload {
                    state: "queued".into(),
                }),
            ),
        );
        if codex {
            let request = v2::StartCodexRunRequest {
                contract_version: 2,
                workspace_id: workspace.clone(),
                session_id: session.session_id.clone(),
                request_id: id(),
                prompt: prompt.into(),
                profile_id: v2::CodexProfileId::CodexTextV1,
                confirmed: true,
                context_run_ids: None,
            };
            add_receipt(
                &store.conn,
                &workspace,
                "start_codex_run_v2",
                &request.request_id,
                &request,
                &queued,
                false,
            );
        } else {
            let request = v2::StartFakeRunRequest {
                contract_version: 2,
                workspace_id: workspace.clone(),
                session_id: session.session_id.clone(),
                request_id: id(),
                provider_id: provider,
                prompt: prompt.into(),
                scenario: v2::FakeScenario::Success,
            };
            add_receipt(
                &store.conn,
                &workspace,
                "start_fake_run_v2",
                &request.request_id,
                &request,
                &queued,
                false,
            );
        }
        Self {
            store,
            _directory: directory,
            workspace,
            legacy,
            current: queued,
        }
    }
    fn append(&mut self, payload: v2::EventPayload) {
        let e = event(&self.current, payload);
        let next = policy::apply_event(&self.current, &e).unwrap();
        let (ordinary,reserved,text,mask,cancels,terminals): (u64,u64,u64,u64,u64,u64) = self.store.conn.query_row(
            "SELECT ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count FROM runs WHERE run_id=?1",[&next.run_id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        let mut counts = [ordinary, reserved, text, mask, cancels, terminals];
        match &e {
            v2::RunEvent::RunStateChanged { payload, .. }
                if payload.to == v2::RunState::CancelRequested =>
            {
                counts[1] += 1;
                counts[4] += 1;
            }
            v2::RunEvent::RunRecovered { .. } => {
                counts[1] += 1;
                counts[5] += 1;
            }
            v2::RunEvent::RunStateChanged { payload, .. }
                if matches!(
                    payload.to,
                    v2::RunState::Succeeded
                        | v2::RunState::Failed
                        | v2::RunState::Cancelled
                        | v2::RunState::Interrupted
                ) =>
            {
                counts[1] += 1;
                counts[5] += 1;
            }
            v2::RunEvent::OutputTruncated { payload, .. } => {
                counts[1] += 1;
                counts[3] |= match payload.scope {
                    v2::TruncationScope::PendingBytes => 1,
                    v2::TruncationScope::PersistedTextBytes => 2,
                    v2::TruncationScope::EventCount => 4,
                };
            }
            _ => {
                counts[0] += 1;
                if let v2::RunEvent::TextDelta { payload, .. } = &e {
                    counts[2] += payload.text.len() as u64;
                }
            }
        }
        add_event(&self.store.conn, &e);
        self.store.conn.execute("UPDATE runs SET snapshot_json=?1,state=?2,last_seq=?3,ordinary_count=?4,reserved_count=?5,text_bytes=?6,
            truncation_mask=?7,cancel_count=?8,terminal_count=?9 WHERE run_id=?10",
            params![encode(&next).unwrap(),next.state.as_str(),next.last_seq,counts[0],counts[1],counts[2],counts[3],counts[4],counts[5],next.run_id]).unwrap();
        self.current = next;
    }
    fn state(&mut self, to: v2::RunState) {
        self.append(v2::EventPayload::RunStateChanged(
            v2::RunStateChangedPayload {
                from: self.current.state,
                to,
                error: None,
            },
        ));
    }
    fn start(&mut self) {
        self.state(v2::RunState::Starting);
        self.state(v2::RunState::Running);
    }
    fn cancel_receipt(&self, emergency: bool) {
        let request = v2::CancelRunRequest {
            contract_version: 2,
            workspace_id: self.workspace.clone(),
            run_id: self.current.run_id.clone(),
            request_id: id(),
        };
        add_receipt(
            &self.store.conn,
            &self.workspace,
            "cancel_run_v2",
            &request.request_id,
            &request,
            &self.current,
            emergency,
        );
    }
    fn audit(&self) -> StoreResult<Option<v2::WorkspaceSnapshot>> {
        audit_copy(&self.store.conn)
    }
}

#[test]
fn mixed_legacy_fake_and_codex_history_is_read_only_and_preserves_original_prefixes() {
    for provider in [v2::ProviderId::Fake, v2::ProviderId::Codex] {
        let mut f = Fixture::new(provider);
        f.start();
        f.append(v2::EventPayload::TextDelta(v2::TextDeltaPayload {
            stream: v2::TextStream::Assistant,
            text: "Fixture only 😀".into(),
        }));
        f.append(v2::EventPayload::UsageUpdated(v2::Usage {
            input_tokens: Some(9),
            output_tokens: Some(4),
            estimated: false,
            cached_input_tokens: (provider == v2::ProviderId::Codex).then_some(2),
            cache_write_input_tokens: None,
            reasoning_output_tokens: (provider == v2::ProviderId::Codex).then_some(1),
        }));
        f.state(v2::RunState::Succeeded);
        f.cancel_receipt(false);
        let before = f.store.conn.total_changes();
        let result = f.audit().unwrap().unwrap();
        assert_eq!(f.store.conn.total_changes(), before);
        assert!(!result.execution_available);
        assert_eq!(result.runs.len(), 2);
        assert_eq!(result.runs[0].run_id, f.legacy.run_id);
        assert_eq!(result.runs[1], f.current);
        assert_eq!(
            result.codex_availability,
            v2::CodexAvailability::NotConfigured
        );
        assert!(f.audit().is_ok());
    }
}

#[test]
fn mixed_receipt_corruption_matrix_rejects_and_never_writes() {
    let mutations = [
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.confirmed',0) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.prompt','wrong') WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.profile_id','other') WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.contract_version',1) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.tools',json('[]')) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET arguments_json=' '||arguments_json WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET request_id='00000000-0000-4000-8000-000000000000' WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.last_seq',2) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.model_id',null) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.provider_id','fake','$.profile_id',null,'$.model_id',null) WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.created_at','2026-10-19T18:00:00.000Z') WHERE command_name='start_codex_run_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.provider_id','fake') WHERE command_name='create_session_v2'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.provider_id','fake') WHERE command_name='create_session_v2'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.contract_version',2) WHERE command_name='create_session'",
        "UPDATE receipts SET arguments_json=json_set(arguments_json,'$.contract_version',2) WHERE command_name='start_fake_run'",
        "UPDATE receipts SET result_json=json_set(result_json,'$.updated_at','2026-10-19T20:00:00.000Z') WHERE command_name='cancel_run'",
    ];
    for sql in mutations {
        let f = Fixture::new(v2::ProviderId::Codex);
        assert!(f.audit().is_ok(), "valid fixture before {sql}");
        f.store.conn.execute(sql, []).unwrap();
        let before = f.store.conn.total_changes();
        assert!(f.audit().is_err(), "accepted {sql}");
        assert_eq!(f.store.conn.total_changes(), before);
    }
}

#[test]
fn mixed_history_and_private_input_corruption_matrix_rejects() {
    let mutations = [
        "UPDATE runs SET prompt='changed' WHERE provider_id='codex'",
        "UPDATE runs SET snapshot_json=json_set(snapshot_json,'$.provider_id','fake','$.profile_id',null,'$.model_id',null) WHERE provider_id='codex'",
        "UPDATE runs SET snapshot_json=json_set(snapshot_json,'$.created_at','2026-10-19T18:00:00.000Z') WHERE provider_id='codex'",
        "UPDATE events SET event_json=json_set(event_json,'$.source','fixture') WHERE json_extract(event_json,'$.provider_id')='codex'",
        "UPDATE events SET event_json=json_set(event_json,'$.contract_version',1,'$.provider_id','fake','$.source','fixture') WHERE json_extract(event_json,'$.provider_id')='codex'",
        "DELETE FROM events WHERE run_id IN (SELECT run_id FROM runs WHERE provider_id='codex')",
        "UPDATE events SET event_json=json_set(event_json,'$.seq',2) WHERE json_extract(event_json,'$.provider_id')='codex'",
        "UPDATE events SET event_json=json_set(event_json,'$.extra',true) WHERE json_extract(event_json,'$.provider_id')='codex'",
        "UPDATE runs SET text_bytes=1 WHERE provider_id='codex'",
        "UPDATE sessions SET snapshot_json=json_set(snapshot_json,'$.provider_id','fake') WHERE json_extract(snapshot_json,'$.provider_id')='codex'",
        "UPDATE workspaces SET last_enqueue=1",
        "CREATE TRIGGER unexpected AFTER INSERT ON runs BEGIN SELECT 1; END",
    ];
    for sql in mutations {
        let f = Fixture::new(v2::ProviderId::Codex);
        assert!(f.audit().is_ok());
        f.store.conn.execute_batch(sql).unwrap();
        let before = f.store.conn.total_changes();
        assert!(f.audit().is_err(), "accepted {sql}");
        assert_eq!(f.store.conn.total_changes(), before);
    }
}

#[test]
fn cancellation_emergency_prefix_and_version_rules() {
    let mut f = Fixture::new(v2::ProviderId::Codex);
    f.start();
    f.state(v2::RunState::CancelRequested);
    f.cancel_receipt(true);
    assert!(f.audit().is_ok());
    f.state(v2::RunState::Cancelled);
    f.cancel_receipt(false);
    assert!(f.audit().is_ok());
    f.cancel_receipt(true); // terminal no-op must not spend emergency capacity
    assert!(f.audit().is_err());
}

#[test]
fn v2_noop_cancel_of_legacy_archive_keeps_both_receipt_versions_at_same_prefix() {
    let f = Fixture::new(v2::ProviderId::Codex);
    let request = v2::CancelRunRequest {
        contract_version: 2,
        workspace_id: f.workspace.clone(),
        run_id: f.legacy.run_id.clone(),
        request_id: id(),
    };
    let result = StoredRunSnapshot::decode(&encode(&f.legacy).unwrap())
        .unwrap()
        .projected()
        .unwrap();
    add_receipt(
        &f.store.conn,
        &f.workspace,
        "cancel_run_v2",
        &request.request_id,
        &request,
        &result,
        false,
    );
    assert!(f.audit().is_ok());
    f.store
        .conn
        .execute(
            "UPDATE receipts SET emergency=1 WHERE command_name='cancel_run_v2'",
            [],
        )
        .unwrap();
    assert!(f.audit().is_err());
}

#[test]
fn versioned_layout_receipts_and_unavailable_current_layout_remain_auditable() {
    let f = Fixture::new(v2::ProviderId::Codex);
    let mut layout: v2::Layout = decode(&encode(&empty_layout(&f.workspace)).unwrap()).unwrap();
    let request = v2::SaveLayoutRequest {
        contract_version: 2,
        workspace_id: f.workspace.clone(),
        request_id: id(),
        expected_revision: 0,
        layout: layout.clone(),
    };
    layout.revision = 1;
    add_receipt(
        &f.store.conn,
        &f.workspace,
        "save_layout_v2",
        &request.request_id,
        &request,
        &layout,
        false,
    );
    f.store
        .conn
        .execute(
            "UPDATE workspaces SET layout_json=?1,layout_revision=1",
            [encode(&layout).unwrap()],
        )
        .unwrap();
    assert!(f.audit().is_ok());
    f.store
        .conn
        .execute(
            "UPDATE workspaces SET layout_json='{\"layout_version\":9}'",
            [],
        )
        .unwrap();
    let result = f.audit().unwrap().unwrap();
    assert!(result.layout.is_none());
    assert_eq!(
        result.layout_warning.unwrap().code,
        "unsupported_layout_version"
    );
    f.store.conn.execute("UPDATE receipts SET result_json=json_set(result_json,'$.revision',2) WHERE command_name='save_layout_v2'",[]).unwrap();
    assert!(f.audit().is_err());
}

#[test]
fn original_versions_and_json_bytes_survive_mixed_read_and_legacy_cancel_projection() {
    let f = Fixture::new(v2::ProviderId::Fake);
    let original: String = f
        .store
        .conn
        .query_row(
            "SELECT snapshot_json FROM runs WHERE run_id=?1",
            [&f.legacy.run_id],
            |r| r.get(0),
        )
        .unwrap();
    let padded = format!("  {original}\n");
    f.store
        .conn
        .execute(
            "UPDATE runs SET snapshot_json=?1 WHERE run_id=?2",
            params![padded, f.legacy.run_id],
        )
        .unwrap();
    assert!(f.audit().is_ok());
    let after: String = f
        .store
        .conn
        .query_row(
            "SELECT snapshot_json FROM runs WHERE run_id=?1",
            [&f.legacy.run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, padded);
    // A projected legacy result cannot be relabelled as a historical v1 receipt.
    let projected = StoredRunSnapshot::decode(&original)
        .unwrap()
        .projected()
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE receipts SET result_json=?1 WHERE command_name='cancel_run'",
            [encode(&projected).unwrap()],
        )
        .unwrap();
    assert!(f.audit().is_err());
}

#[test]
fn legacy_preview_projection_parity_includes_order_and_layout_warnings() {
    for layout in ["{", "{\"layout_version\":9}"] {
        let directory = TemporaryDirectory::new();
        let (mut store, w, s) = fresh(&directory.0);
        for _ in 0..4 {
            queued(&mut store, &w, &s);
        }
        store
            .conn
            .execute("UPDATE workspaces SET layout_json=?1", [layout])
            .unwrap();
        drop(store);
        let before = fs::read(directory.0.join("execution.sqlite3")).unwrap();
        let preview =
            crate::storage::upgrade_preview::LegacyUpgradePreview::inspect(&directory.0, LATER)
                .unwrap();
        assert_eq!(preview.interrupted_runs(), 4);
        assert!(preview.projected_history().layout_warning.is_some());
        preview.close().unwrap();
        assert_eq!(
            fs::read(directory.0.join("execution.sqlite3")).unwrap(),
            before
        );
    }
}

#[test]
fn query_only_connection_accepts_mixed_audit_without_sqlite_writes() {
    let f = Fixture::new(v2::ProviderId::Codex);
    f.store
        .conn
        .pragma_update(None, "query_only", true)
        .unwrap();
    let before = fs::read(f._directory.0.join("execution.sqlite3")).unwrap();
    assert!(f.audit().is_ok());
    assert_eq!(
        fs::read(f._directory.0.join("execution.sqlite3")).unwrap(),
        before
    );
}

#[test]
fn full_receipt_capacity_is_accepted_and_one_extra_receipt_rejected() {
    let mut f = Fixture::new(v2::ProviderId::Fake);
    f.state(v2::RunState::Cancelled);
    let count = stored_count(&f.store, "receipts") as usize;
    f.store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    for _ in count..COMMAND_RECEIPTS {
        f.cancel_receipt(false);
    }
    f.store.conn.execute_batch("COMMIT").unwrap();
    assert!(f.audit().is_ok());
    f.cancel_receipt(false);
    assert!(f.audit().is_err());
}

#[test]
fn duplicate_emergency_requests_cannot_both_claim_first_cancellation() {
    let mut f = Fixture::new(v2::ProviderId::Codex);
    f.start();
    f.state(v2::RunState::CancelRequested);
    f.cancel_receipt(true);
    assert!(f.audit().is_ok());
    f.cancel_receipt(true);
    assert!(f.audit().is_err());
}

#[test]
fn v2_recovery_preserves_original_v2_history_and_queued_receipt() {
    let mut f = Fixture::new(v2::ProviderId::Codex);
    f.start();
    f.append(v2::EventPayload::RunRecovered(v2::RunRecoveredPayload {
        previous_state: v2::ActiveState::Running,
        state: "interrupted".into(),
    }));
    assert!(f.audit().is_ok());
    f.store.conn.execute("UPDATE events SET event_json=json_set(event_json,'$.payload.previous_state','starting') WHERE json_extract(event_json,'$.kind')='run_recovered'",[]).unwrap();
    assert!(f.audit().is_err());
}

#[test]
fn migration_restores_foreign_keys_after_sql_and_callback_errors() {
    let mut empty = Connection::open_in_memory().unwrap();
    assert!(migrate_copy(&mut empty, |_| Ok(())).is_err());
    assert_eq!(
        empty
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let directory = TemporaryDirectory::new();
    let (mut store, _, _) = fresh(&directory.0);
    assert!(migrate_copy(&mut store.conn, |_| Err(unavailable())).is_err());
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(store
        .conn
        .execute(
            "INSERT INTO sessions VALUES (?1,?2,'{}')",
            params![id(), id()]
        )
        .is_err());
    migrate_copy(&mut store.conn, |_| Ok(())).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
