//! Copy-only persistence tests, not live adapter dispatch or production upgrade.
use super::*;
use crate::execution_v2::{
    self as v2, admission::StartRequest, ports::AdmissionJournal, validation as policy,
};
use crate::storage::{admission_v2::AdmissionRehearsal, upgrade_preview::LegacyUpgradePreview};

pub(super) fn rehearsal(directory: &Path) -> (AdmissionRehearsal, String, String) {
    let (mut store, workspace, session) = fresh(directory);
    queued(&mut store, &workspace, &session);
    drop(store);
    (
        LegacyUpgradePreview::inspect(directory, LATER)
            .unwrap()
            .into_admission_rehearsal()
            .unwrap(),
        workspace,
        session,
    )
}
pub(super) fn create(workspace: &str, provider: v2::ProviderId) -> v2::CreateSessionRequest {
    v2::CreateSessionRequest {
        contract_version: 2,
        workspace_id: workspace.into(),
        request_id: id(),
        label: "Native copy fixture".into(),
        provider_id: provider,
    }
}
pub(super) fn codex(workspace: &str, session: &str) -> v2::StartCodexRunRequest {
    v2::StartCodexRunRequest {
        contract_version: 2,
        workspace_id: workspace.into(),
        session_id: session.into(),
        request_id: id(),
        prompt: "No model invocation: persistence fixture 😀".into(),
        profile_id: v2::CodexProfileId::CodexTextV1,
        confirmed: true,
        context_run_ids: None,
    }
}
pub(super) fn fake(workspace: &str, session: &str) -> v2::StartFakeRunRequest {
    v2::StartFakeRunRequest {
        contract_version: 2,
        workspace_id: workspace.into(),
        session_id: session.into(),
        request_id: id(),
        provider_id: v2::ProviderId::Fake,
        prompt: "Simulated fixture".into(),
        scenario: v2::FakeScenario::Success,
    }
}
fn invalid_seed() -> RunSeed {
    RunSeed {
        run_id: "invalid ignored seed".into(),
        event_id: "invalid".into(),
        created_at: "invalid".into(),
    }
}
pub(super) fn digest(conn: &Connection) -> Vec<Vec<String>> {
    ["SELECT json_array(workspace_id,record_json,layout_json,layout_revision,last_enqueue) FROM workspaces",
    "SELECT json_array(session_id,workspace_id,snapshot_json) FROM sessions ORDER BY session_id",
    "SELECT json_array(run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,prompt,scenario,provider_id,profile_id,ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count) FROM runs ORDER BY run_id",
    "SELECT json_array(event_id,workspace_id,session_id,run_id,seq,event_json) FROM events ORDER BY event_id",
    "SELECT json_array(workspace_id,command_name,request_id,arguments_json,result_json,emergency) FROM receipts ORDER BY command_name,request_id"]
    .iter().map(|sql|conn.prepare(sql).unwrap().query_map([],|r|r.get(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()).collect()
}
pub(super) fn family(directory: &Path) -> Vec<(String, Vec<u8>)> {
    [
        "execution.sqlite3",
        "execution.sqlite3-journal",
        "execution.sqlite3-wal",
        "execution.sqlite3-shm",
    ]
    .into_iter()
    .filter_map(|n| fs::read(directory.join(n)).ok().map(|b| (n.into(), b)))
    .collect()
}

#[test]
fn shared_copy_admission_supports_both_profiles_and_keeps_originals_untouched() {
    let directory = TemporaryDirectory::new();
    let (mut journal, w, legacy_session) = rehearsal(&directory.0);
    let original = family(&directory.0);
    let create = create(&w, v2::ProviderId::Codex);
    let session = journal.create_session(&create, session_seed()).unwrap();
    assert!(!session.replayed && session.committed_events.is_empty());
    let fake = fake(&w, &legacy_session);
    let codex = codex(&w, &session.result.session_id);
    let a = journal
        .enqueue_run(StartRequest::Fake(&fake), run_seed())
        .unwrap();
    let b = journal
        .enqueue_run(StartRequest::CodexText(&codex), run_seed())
        .unwrap();
    assert!(!a.replayed && !b.replayed);
    assert_eq!(a.result.enqueue_order, 2);
    assert_eq!(b.result.enqueue_order, 3);
    assert_eq!(a.committed_events.len(), 1);
    assert_eq!(b.committed_events.len(), 1);
    assert_eq!(a.result.state, v2::RunState::Queued);
    assert_eq!(b.result.state, v2::RunState::Queued);
    assert!(a.result.profile_id.is_none() && a.result.model_id.is_none());
    assert_eq!(b.result.profile_id, Some(v2::CodexProfileId::CodexTextV1));
    assert_eq!(b.result.model_id, Some(v2::CodexModelId::Gpt61Sol));
    let raw: (Option<String>, Option<String>, String) = journal
        .connection()
        .query_row(
            "SELECT scenario,profile_id,prompt FROM runs WHERE run_id=?1",
            [&b.result.run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        raw,
        (None, Some("codex_text_v1".into()), codex.prompt.clone())
    );
    let snapshot = journal.audit_checkpoint().unwrap();
    assert!(!snapshot.execution_available);
    assert_eq!(snapshot.runs.len(), 3);
    assert_eq!(snapshot.runs[0].state, v2::RunState::Interrupted);
    assert_eq!(
        journal.bootstrap().storage_state,
        v2::StorageState::UpgradeRequired
    );
    assert!(journal.bootstrap().snapshot.is_none());
    assert_eq!(family(&directory.0), original);
    assert_eq!(
        SqliteStore::open(&directory.0).err().unwrap().code,
        "storage_busy"
    );
    journal.close().unwrap();
    assert_eq!(family(&directory.0), original);
    let mut store = SqliteStore::open(&directory.0).unwrap();
    store.bootstrap(workspace_seed()).unwrap();
    assert_eq!(
        store
            .startup_recover(LATER, &mut Ids)
            .unwrap()
            .interrupted_runs
            .len(),
        1
    );
    assert_eq!(stored_count(&store, "runs"), 1);
}

#[test]
fn replay_uses_immutable_receipts_not_new_seeds_and_conflicts_do_not_write() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let c = create(&w, v2::ProviderId::Codex);
    let session = j.create_session(&c, session_seed()).unwrap().result;
    let replay = j
        .create_session(
            &c,
            SessionSeed {
                session_id: "invalid ignored".into(),
                created_at: "invalid".into(),
            },
        )
        .unwrap();
    assert!(replay.replayed && replay.committed_events.is_empty());
    assert_eq!(replay.result, session);
    let request = codex(&w, &session.session_id);
    let first = j
        .enqueue_run(StartRequest::CodexText(&request), run_seed())
        .unwrap();
    let before = digest(j.connection());
    let replay = j
        .enqueue_run(StartRequest::CodexText(&request), invalid_seed())
        .unwrap();
    assert!(replay.replayed && replay.committed_events.is_empty());
    assert_eq!(replay.result, first.result);
    assert_eq!(digest(j.connection()), before);
    let mut changed = request.clone();
    changed.prompt.push('x');
    assert_eq!(
        j.enqueue_run(StartRequest::CodexText(&changed), run_seed())
            .err()
            .unwrap()
            .code,
        "request_id_conflict"
    );
    let mut changed = c.clone();
    changed.label.push('x');
    assert_eq!(
        j.create_session(&changed, session_seed())
            .err()
            .unwrap()
            .code,
        "request_id_conflict"
    );
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn profile_policy_session_ownership_and_utf8_limits_precede_any_commit() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let session = j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let original = codex(&w, &session.session_id);
    let before = digest(j.connection());
    let mut bad = Vec::new();
    let mut r = original.clone();
    r.confirmed = false;
    bad.push(r);
    let mut r = original.clone();
    r.contract_version = 1;
    bad.push(r);
    let mut r = original.clone();
    r.prompt = "😀".repeat(256) + "x";
    bad.push(r);
    let mut r = original.clone();
    r.prompt = "x\0y".into();
    bad.push(r);
    let mut r = original.clone();
    r.request_id = "invalid".into();
    bad.push(r);
    let mut r = original.clone();
    r.session_id = legacy.clone();
    bad.push(r);
    let mut r = original.clone();
    r.session_id = id();
    bad.push(r);
    let mut r = original.clone();
    r.workspace_id = id();
    bad.push(r);
    for request in bad {
        assert!(j
            .enqueue_run(StartRequest::CodexText(&request), run_seed())
            .is_err());
        assert_eq!(digest(j.connection()), before);
    }
    assert!(j
        .enqueue_run(
            StartRequest::Fake(&fake(&w, &session.session_id)),
            run_seed()
        )
        .is_err());
    let mut fake_request = fake(&w, &legacy);
    fake_request.provider_id = v2::ProviderId::Codex;
    assert!(j
        .enqueue_run(StartRequest::Fake(&fake_request), run_seed())
        .is_err());
    assert_eq!(digest(j.connection()), before);
    let mut boundary = original;
    boundary.prompt = "😀".repeat(256);
    j.enqueue_run(StartRequest::CodexText(&boundary), run_seed())
        .unwrap();
    let mut fake_request = fake(&w, &legacy);
    fake_request.prompt = "\"".repeat(65536);
    j.enqueue_run(StartRequest::Fake(&fake_request), run_seed())
        .unwrap();
    j.audit_checkpoint().unwrap();
}

#[test]
fn seed_collisions_and_transaction_failures_roll_back_the_entire_admission() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let first = j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
        .unwrap();
    let before = digest(j.connection());
    let mut seed = run_seed();
    seed.run_id = first.result.run_id.clone();
    assert!(j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), seed)
        .is_err());
    let mut seed = run_seed();
    seed.event_id = first.committed_events[0].header().event_id;
    assert!(j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), seed)
        .is_err());
    assert!(j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), invalid_seed())
        .is_err());
    assert_eq!(digest(j.connection()), before);
    for table in ["runs", "events", "receipts"] {
        j.connection().execute_batch(&format!("CREATE TEMP TRIGGER fail_admission BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END")).unwrap();
        assert!(j
            .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .is_err());
        assert_eq!(
            digest(j.connection()),
            before,
            "rollback after {table} failure"
        );
        j.connection()
            .execute_batch("DROP TRIGGER fail_admission")
            .unwrap();
    }
    j.connection().execute_batch("CREATE TEMP TRIGGER fail_session_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'injected'); END").unwrap();
    assert!(j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .is_err());
    assert_eq!(digest(j.connection()), before);
    j.connection()
        .execute_batch("DROP TRIGGER fail_session_receipt")
        .unwrap();
    j.audit_checkpoint().unwrap();
}

#[test]
fn commit_failure_rolls_back_session_and_run_receipts_events_and_order() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let before = digest(j.connection());
    j.connection().execute_batch("PRAGMA defer_foreign_keys=ON;
        CREATE TEMP TRIGGER fail_commit AFTER INSERT ON receipts BEGIN
        INSERT INTO sessions(session_id,workspace_id,snapshot_json) VALUES ('invalid-orphan','missing-workspace','{}'); END;").unwrap();
    assert!(j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
        .is_err());
    assert_eq!(digest(j.connection()), before);
    j.connection()
        .execute_batch("PRAGMA defer_foreign_keys=ON")
        .unwrap();
    assert!(j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .is_err());
    assert_eq!(digest(j.connection()), before);
    j.connection()
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    j.audit_checkpoint().unwrap();
}

#[test]
fn shared_fifo_queue_and_session_capacity_keep_replays_available() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let c = create(&w, v2::ProviderId::Codex);
    let s = j.create_session(&c, session_seed()).unwrap().result;
    for _ in 2..WORKSPACE_SESSIONS {
        j.create_session(&create(&w, v2::ProviderId::Fake), session_seed())
            .unwrap();
    }
    assert_eq!(
        j.create_session(&create(&w, v2::ProviderId::Codex), session_seed())
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert!(j.create_session(&c, session_seed()).unwrap().replayed);
    let first = fake(&w, &legacy);
    let first_result = j
        .enqueue_run(StartRequest::Fake(&first), run_seed())
        .unwrap()
        .result;
    for index in 1..QUEUED_RUNS {
        let result = if index % 2 == 0 {
            j.enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
                .unwrap()
        } else {
            j.enqueue_run(
                StartRequest::CodexText(&codex(&w, &s.session_id)),
                run_seed(),
            )
            .unwrap()
        };
        assert_eq!(result.result.enqueue_order, 2 + index as u64);
    }
    let before = digest(j.connection());
    assert_eq!(
        j.enqueue_run(
            StartRequest::CodexText(&codex(&w, &s.session_id)),
            run_seed()
        )
        .err()
        .unwrap()
        .code,
        "queue_full"
    );
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&first), invalid_seed())
            .unwrap()
            .result,
        first_result
    );
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn full_receipt_ledger_blocks_new_work_but_never_blocks_exact_replay() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let c = create(&w, v2::ProviderId::Codex);
    let session = j.create_session(&c, session_seed()).unwrap().result;
    let start = codex(&w, &session.session_id);
    let run = j
        .enqueue_run(StartRequest::CodexText(&start), run_seed())
        .unwrap()
        .result;
    let archived = j
        .audit_checkpoint()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    let count: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    j.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
    for _ in count..COMMAND_RECEIPTS as u64 {
        let request = v2::CancelRunRequest {
            contract_version: 2,
            workspace_id: w.clone(),
            run_id: archived.run_id.clone(),
            request_id: id(),
        };
        j.connection()
            .execute(
                "INSERT INTO receipts VALUES (?1,'cancel_run_v2',?2,?3,?4,0)",
                params![
                    w,
                    request.request_id,
                    canonical_arguments(&request).unwrap(),
                    encode(&archived).unwrap()
                ],
            )
            .unwrap();
    }
    j.connection().execute_batch("COMMIT").unwrap();
    let before = digest(j.connection());
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(
        j.create_session(&create(&w, v2::ProviderId::Fake), session_seed())
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(
        j.enqueue_run(StartRequest::CodexText(&start), invalid_seed())
            .unwrap()
            .result,
        run
    );
    assert!(j.create_session(&c, session_seed()).unwrap().replayed);
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn command_names_isolate_request_ids_across_harnesses_and_legacy_receipts() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let s = j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let legacy_id: String = j
        .connection()
        .query_row(
            "SELECT request_id FROM receipts WHERE command_name='start_fake_run'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut a = fake(&w, &legacy);
    a.request_id = legacy_id.clone();
    let mut b = codex(&w, &s.session_id);
    b.request_id = legacy_id;
    let a_result = j
        .enqueue_run(StartRequest::Fake(&a), run_seed())
        .unwrap()
        .result;
    let b_result = j
        .enqueue_run(StartRequest::CodexText(&b), run_seed())
        .unwrap()
        .result;
    assert_ne!(a_result.run_id, b_result.run_id);
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&a), invalid_seed())
            .unwrap()
            .result,
        a_result
    );
    assert_eq!(
        j.enqueue_run(StartRequest::CodexText(&b), invalid_seed())
            .unwrap()
            .result,
        b_result
    );
    j.audit_checkpoint().unwrap();
}

pub(super) fn terminal_fixture(j: &mut AdmissionRehearsal, run: &v2::RunSnapshot) {
    let e = v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(),
            workspace_id: run.workspace_id.clone(),
            session_id: run.session_id.clone(),
            run_id: run.run_id.clone(),
            provider_id: run.provider_id,
            source: v2::EventSource::System,
            seq: 2,
            occurred_at: LATER.into(),
        },
        v2::EventPayload::RunStateChanged(v2::RunStateChangedPayload {
            from: v2::RunState::Queued,
            to: v2::RunState::Cancelled,
            error: None,
        }),
    );
    let next = policy::apply_event(run, &e).unwrap();
    let h = e.header();
    j.connection()
        .execute(
            "INSERT INTO events VALUES (?1,?2,?3,?4,2,?5)",
            params![
                h.event_id,
                h.workspace_id,
                h.session_id,
                h.run_id,
                encode(&e).unwrap()
            ],
        )
        .unwrap();
    j.connection().execute("UPDATE runs SET snapshot_json=?1,state='cancelled',last_seq=2,reserved_count=1,terminal_count=1 WHERE run_id=?2",
        params![encode(&next).unwrap(),next.run_id]).unwrap();
}

#[test]
fn original_start_receipt_survives_later_history_and_run_capacity() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let first = fake(&w, &legacy);
    let result = j
        .enqueue_run(StartRequest::Fake(&first), run_seed())
        .unwrap()
        .result;
    terminal_fixture(&mut j, &result);
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&first), invalid_seed())
            .unwrap()
            .result,
        result
    );
    for _ in 2..WORKSPACE_RUNS {
        let request = fake(&w, &legacy);
        let result = j
            .enqueue_run(StartRequest::Fake(&request), run_seed())
            .unwrap()
            .result;
        terminal_fixture(&mut j, &result);
    }
    let before = digest(j.connection());
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&first), invalid_seed())
            .unwrap()
            .result,
        result
    );
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn enqueue_order_capacity_and_session_seed_collision_do_not_commit() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let before = digest(j.connection());
    assert_eq!(
        j.create_session(
            &create(&w, v2::ProviderId::Fake),
            SessionSeed {
                session_id: legacy.clone(),
                created_at: NOW.into()
            }
        )
        .err()
        .unwrap()
        .code,
        "state_conflict"
    );
    assert_eq!(digest(j.connection()), before);
    j.connection()
        .execute("UPDATE workspaces SET last_enqueue=?1", [JS_MAX_INTEGER])
        .unwrap();
    let before = digest(j.connection());
    assert_eq!(
        j.enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn shared_sql_does_not_hardcode_agent_profiles_models_or_dispatch() {
    let source = include_str!("../admission_v2.rs");
    for forbidden in [
        "CodexProfileId",
        "CodexModelId",
        "ProviderId::Codex",
        "StartRequest::CodexText",
        "codex_text_v1",
        "gpt-6.1-sol",
        "std::process",
        "run_turn",
        "spawn(",
    ] {
        assert!(
            !source.contains(forbidden),
            "adapter/execution policy leaked into shared persistence: {forbidden}"
        );
    }
}

#[test]
fn shared_policy_does_not_treat_fake_or_other_harness_identity_as_codex_authority() {
    let request = fake(&id(), &id());
    let session = v2::SessionSnapshot {
        contract_version: 2,
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.clone(),
        provider_id: v2::ProviderId::Fake,
        label: "Fake".into(),
        created_at: NOW.into(),
    };
    let prepared =
        v2::admission::PreparedAdmission::validate(StartRequest::Fake(&request), &session).unwrap();
    assert_eq!(prepared.input().provider(), v2::ProviderId::Fake);
    assert!(prepared.input().profile().is_none());
    let created = prepared.materialize(run_seed(), 1).unwrap();
    policy::validate_event(&created.event).unwrap();
    assert_eq!(created.event.header().source, v2::EventSource::Fixture);
    assert!(created.snapshot.model_id.is_none());
    assert!(prepared.materialize(run_seed(), 0).is_err());
    // Closed/versioned support is intentional: no arbitrary plugin ID can
    // silently acquire a profile's permissions or be coerced into fake/Codex.
    assert!(serde_json::from_str::<v2::ProviderId>("\"new-agent\"").is_err());
}
