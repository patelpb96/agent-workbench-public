use super::*;
use std::path::PathBuf;

mod admission_v2;
mod audit_v2;
mod context_v3;
mod read_v2;
mod receipt_audit;
mod store_v2;
mod upgrade_preview;
mod writer_v2;

const NOW: &str = "2026-10-19T19:00:00.000Z";
const LATER: &str = "2026-10-19T19:01:00.000Z";
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agent-workbench-storage-{}", id()));
        // Only a directory successfully created by this test receives a guard.
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        // This exact unique directory was created by this guard, never a root.
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Ids;
impl IdSource for Ids {
    fn next_id(&mut self) -> Id {
        id()
    }
}
fn workspace_seed() -> WorkspaceSeed {
    WorkspaceSeed {
        workspace_id: id(),
        created_at: NOW.into(),
    }
}
fn session_seed() -> SessionSeed {
    SessionSeed {
        session_id: id(),
        created_at: NOW.into(),
    }
}
fn run_seed() -> RunSeed {
    RunSeed {
        run_id: id(),
        event_id: id(),
        created_at: NOW.into(),
    }
}
fn workspace_request(workspace_id: &str) -> WorkspaceRequest {
    WorkspaceRequest {
        contract_version: 1,
        workspace_id: workspace_id.into(),
    }
}
fn create_request(workspace_id: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        contract_version: 1,
        workspace_id: workspace_id.into(),
        request_id: id(),
        label: "Simulation".into(),
    }
}
fn start_request(workspace_id: &str, session_id: &str) -> StartFakeRunRequest {
    StartFakeRunRequest {
        contract_version: 1,
        workspace_id: workspace_id.into(),
        session_id: session_id.into(),
        request_id: id(),
        provider_id: ProviderId::Fake,
        prompt: "Private simulated input".into(),
        scenario: FakeScenario::Success,
    }
}
fn cancel_request(run: &RunSnapshot) -> CancelRunRequest {
    CancelRunRequest {
        contract_version: 1,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        request_id: id(),
    }
}
fn header(run: &RunSnapshot, source: EventSource) -> EventHeader {
    EventHeader {
        contract_version: 1,
        event_id: id(),
        workspace_id: run.workspace_id.clone(),
        session_id: run.session_id.clone(),
        run_id: run.run_id.clone(),
        provider_id: ProviderId::Fake,
        source,
        seq: run.last_seq + 1,
        occurred_at: LATER.into(),
    }
}
fn batch(run: &RunSnapshot, payloads: Vec<EventPayload>) -> AppendBatch {
    let mut next = run.clone();
    let mut events = Vec::new();
    for payload in payloads {
        let source = if matches!(
            &payload,
            EventPayload::TextDelta(_) | EventPayload::ToolActivity(_)
        ) {
            EventSource::Fixture
        } else {
            EventSource::System
        };
        let event = RunEvent::from_parts(header(&next, source), payload);
        next = apply_event(&next, &event).unwrap();
        events.push(event);
    }
    AppendBatch {
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        expected_last_seq: run.last_seq,
        expected_state: run.state,
        events,
        next_snapshot: next,
    }
}
fn transition(run: &RunSnapshot, to: RunState) -> AppendBatch {
    batch(
        run,
        vec![EventPayload::RunStateChanged(RunStateChangedPayload {
            from: run.state,
            to,
            error: if to == RunState::Failed {
                Some(error("fake_start_failed", "SIMULATED failure."))
            } else {
                None
            },
        })],
    )
}
fn fresh(path: &Path) -> (SqliteStore, String, String) {
    let mut store = SqliteStore::open(path).unwrap();
    let initial = store.bootstrap(workspace_seed()).unwrap();
    assert!(!initial.execution_available);
    assert!(store
        .startup_recover(NOW, &mut Ids)
        .unwrap()
        .committed_events
        .is_empty());
    let workspace_id = initial.workspace.workspace_id;
    let session = store
        .create_session(&create_request(&workspace_id), session_seed())
        .unwrap()
        .result;
    (store, workspace_id, session.session_id)
}
fn queued(store: &mut SqliteStore, workspace_id: &str, session_id: &str) -> RunSnapshot {
    store
        .enqueue_run(&start_request(workspace_id, session_id), run_seed())
        .unwrap()
        .result
}
fn running(store: &mut SqliteStore, run: RunSnapshot) -> RunSnapshot {
    let starting = store
        .append_batch(transition(&run, RunState::Starting))
        .unwrap()
        .result;
    store
        .append_batch(transition(&starting, RunState::Running))
        .unwrap()
        .result
}
fn page_request(
    run: &RunSnapshot,
    after: u64,
    through: Option<u64>,
    limit: u64,
) -> EventPageRequest {
    EventPageRequest {
        contract_version: 1,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        after_seq: after,
        through_seq: through,
        limit,
    }
}
fn stored_count(store: &SqliteStore, table: &str) -> u64 {
    // Test-only fixed identifiers, never request-controlled SQL.
    let sql = match table {
        "runs" => "SELECT COUNT(*) FROM runs",
        "events" => "SELECT COUNT(*) FROM events",
        "receipts" => "SELECT COUNT(*) FROM receipts",
        _ => panic!("unknown test table"),
    };
    store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn bootstrap_reopen_identity_and_exclusive_ownership() {
    let directory = TemporaryDirectory::new();
    let mut store = SqliteStore::open(&directory.0).unwrap();
    let first = store.bootstrap(workspace_seed()).unwrap();
    assert_eq!(first.layout.as_ref().unwrap().revision, 0);
    assert!(!first.execution_available);
    assert!(matches!(SqliteStore::open(&directory.0), Err(value) if value.code == "storage_busy"));
    assert_eq!(
        store.bootstrap(workspace_seed()).unwrap().workspace,
        first.workspace
    );
    drop(store);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    let second = reopened.bootstrap(workspace_seed()).unwrap();
    assert_eq!(second.workspace, first.workspace);
    assert_eq!(second.layout, first.layout);
    assert!(!second.execution_available);
    reopened.startup_recover(NOW, &mut Ids).unwrap();
    assert!(
        reopened
            .workspace_snapshot(&workspace_request(&first.workspace.workspace_id))
            .unwrap()
            .execution_available
    );
}

#[test]
fn enqueue_is_atomic_idempotent_scoped_and_fifo() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let request = start_request(&workspace_id, &session_id);
    let original = store.enqueue_run(&request, run_seed()).unwrap();
    assert_eq!(original.result.enqueue_order, 1);
    assert_eq!(original.committed_events.len(), 1);
    let replay = store
        .enqueue_run(
            &request,
            RunSeed {
                run_id: "invalid ignored seed".into(),
                event_id: "ignored".into(),
                created_at: "ignored".into(),
            },
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.result, original.result);
    assert!(replay.committed_events.is_empty());
    let mut conflict = request.clone();
    conflict.prompt.push('!');
    assert_eq!(
        store.enqueue_run(&conflict, run_seed()).unwrap_err().code,
        "request_id_conflict"
    );
    let second = queued(&mut store, &workspace_id, &session_id);
    assert_eq!(second.enqueue_order, 2);
    assert_eq!(
        store.next_queued(&workspace_id).unwrap().unwrap().snapshot,
        original.result
    );
    assert_eq!(
        store
            .append_batch(transition(&second, RunState::Starting))
            .unwrap_err()
            .code,
        "state_conflict"
    );
    let active = store
        .append_batch(transition(&original.result, RunState::Starting))
        .unwrap()
        .result;
    assert!(store.next_queued(&workspace_id).unwrap().is_none());
    assert_eq!(
        store.get_run(&workspace_id, &active.run_id).unwrap().prompt,
        request.prompt
    );
    assert_eq!(
        store.get_run(&id(), &active.run_id).unwrap_err().code,
        "workspace_not_found"
    );
    assert_eq!(stored_count(&store, "runs"), 2);
    let before = stored_count(&store, "receipts");
    let mut disabled = start_request(&workspace_id, &session_id);
    disabled.provider_id = ProviderId::Codex;
    assert_eq!(
        store.enqueue_run(&disabled, run_seed()).unwrap_err().code,
        "provider_execution_disabled"
    );
    assert_eq!(stored_count(&store, "receipts"), before);
}

#[test]
fn compound_batch_cannot_bypass_another_runs_active_slot() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let first = queued(&mut store, &workspace_id, &session_id);
    let first = running(&mut store, first);
    let second = queued(&mut store, &workspace_id, &session_id);
    let effects = [
        (RunState::Queued, RunState::Starting),
        (RunState::Starting, RunState::Running),
        (RunState::Running, RunState::Succeeded),
    ]
    .into_iter()
    .map(|(from, to)| {
        EventPayload::RunStateChanged(RunStateChangedPayload {
            from,
            to,
            error: None,
        })
    })
    .collect();
    let before = stored_count(&store, "events");
    assert_eq!(
        store
            .append_batch(batch(&second, effects))
            .unwrap_err()
            .code,
        "state_conflict"
    );
    assert_eq!(stored_count(&store, "events"), before);
    assert_eq!(
        store
            .get_run(&workspace_id, &second.run_id)
            .unwrap()
            .snapshot,
        second
    );
    assert_eq!(
        store
            .get_run(&workspace_id, &first.run_id)
            .unwrap()
            .snapshot,
        first
    );
}

#[test]
fn fixture_pages_and_workspace_snapshot_match_shared_reduction() {
    let directory = TemporaryDirectory::new();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../contracts/fixtures/fake-success.json"
    ))
    .unwrap();
    let events: Vec<RunEvent> = serde_json::from_value(fixture["events"].clone()).unwrap();
    let expected: RunSnapshot = serde_json::from_value(fixture["snapshot"].clone()).unwrap();
    let mut store = SqliteStore::open(&directory.0).unwrap();
    store
        .bootstrap(WorkspaceSeed {
            workspace_id: expected.workspace_id.clone(),
            created_at: expected.created_at.clone(),
        })
        .unwrap();
    store.startup_recover(NOW, &mut Ids).unwrap();
    store
        .create_session(
            &create_request(&expected.workspace_id),
            SessionSeed {
                session_id: expected.session_id.clone(),
                created_at: expected.created_at.clone(),
            },
        )
        .unwrap();
    let created = store
        .enqueue_run(
            &start_request(&expected.workspace_id, &expected.session_id),
            RunSeed {
                run_id: expected.run_id.clone(),
                event_id: events[0].header().event_id,
                created_at: expected.created_at.clone(),
            },
        )
        .unwrap();
    assert_eq!(created.committed_events, vec![events[0].clone()]);
    let first_page = store
        .list_run_events(&page_request(&created.result, 0, None, 2))
        .unwrap();
    assert_eq!(first_page.through_seq, 1);
    assert!(!first_page.has_more);
    store
        .append_batch(AppendBatch {
            workspace_id: expected.workspace_id.clone(),
            run_id: expected.run_id.clone(),
            expected_last_seq: 1,
            expected_state: RunState::Queued,
            events: events[1..].to_vec(),
            next_snapshot: expected.clone(),
        })
        .unwrap();
    let pinned = store
        .list_run_events(&page_request(&expected, 0, Some(first_page.through_seq), 2))
        .unwrap();
    assert_eq!(pinned.events, first_page.events);
    assert_eq!(pinned.last_seq, 6);
    let mut loaded = Vec::new();
    let mut after = 0;
    loop {
        let page = store
            .list_run_events(&page_request(&expected, after, Some(6), 2))
            .unwrap();
        loaded.extend(page.events);
        after = page.next_after_seq;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(loaded, events);
    assert_eq!(after, 6);
    let snapshot = store
        .workspace_snapshot(&workspace_request(&expected.workspace_id))
        .unwrap();
    assert_eq!(snapshot.runs, vec![expected.clone()]);
    assert!(store
        .list_run_events(&page_request(&expected, 0, Some(7), 2))
        .is_err());
    assert!(store
        .list_run_events(&page_request(&expected, 7, None, 2))
        .is_err());
    let empty = store
        .list_run_events(&page_request(&expected, 6, Some(6), 2))
        .unwrap();
    assert!(empty.events.is_empty());
    assert_eq!(empty.next_after_seq, 6);
    assert!(!empty.has_more);
}

#[test]
fn queued_and_active_cancellation_have_atomic_immutable_receipts() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let queued_run = queued(&mut store, &workspace_id, &session_id);
    let request = cancel_request(&queued_run);
    let before = stored_count(&store, "receipts");
    assert_eq!(
        store.cancel_run(&request, None).unwrap_err().code,
        "state_conflict"
    );
    assert_eq!(stored_count(&store, "receipts"), before);
    let stopped = store
        .cancel_run(&request, Some(transition(&queued_run, RunState::Cancelled)))
        .unwrap();
    assert_eq!(stopped.result.state, RunState::Cancelled);
    assert_eq!(stopped.committed_events.len(), 1);
    let replay = store
        .cancel_run(&request, Some(transition(&queued_run, RunState::Starting)))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.result, stopped.result);
    assert!(replay.committed_events.is_empty());
    let next = queued(&mut store, &workspace_id, &session_id);
    let active = running(&mut store, next);
    let active_request = cancel_request(&active);
    assert!(store
        .cancel_run(
            &active_request,
            Some(transition(&active, RunState::WaitingInput))
        )
        .is_err());
    let requested = store
        .cancel_run(
            &active_request,
            Some(transition(&active, RunState::CancelRequested)),
        )
        .unwrap();
    let no_op = store
        .cancel_run(&cancel_request(&requested.result), None)
        .unwrap();
    assert!(no_op.committed_events.is_empty());
    store
        .append_batch(transition(&requested.result, RunState::Cancelled))
        .unwrap();
    let old_reply = store.cancel_run(&active_request, None).unwrap();
    assert!(old_reply.replayed);
    assert_eq!(old_reply.result.state, RunState::CancelRequested);
    assert!(old_reply.committed_events.is_empty());
    assert_eq!(
        store
            .get_run(&workspace_id, &active.run_id)
            .unwrap()
            .snapshot
            .state,
        RunState::Cancelled
    );
}

#[test]
fn recovery_runs_once_per_open_and_never_interrupts_new_work_in_same_store() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let first = queued(&mut store, &workspace_id, &session_id);
    let first = running(&mut store, first);
    let cancel = cancel_request(&first);
    store
        .cancel_run(&cancel, Some(transition(&first, RunState::CancelRequested)))
        .unwrap();
    let second = queued(&mut store, &workspace_id, &session_id);
    drop(store);
    let mut store = SqliteStore::open(&directory.0).unwrap();
    assert!(
        !store
            .bootstrap(workspace_seed())
            .unwrap()
            .execution_available
    );
    assert!(store
        .enqueue_run(&start_request(&workspace_id, &session_id), run_seed())
        .is_err());
    let recovered = store.startup_recover(LATER, &mut Ids).unwrap();
    assert_eq!(recovered.interrupted_runs.len(), 2);
    assert_eq!(recovered.committed_events.len(), 2);
    assert!(recovered
        .interrupted_runs
        .iter()
        .all(|run| run.state == RunState::Interrupted));
    assert!(recovered.committed_events.iter().all(|event| matches!(
        event,
        RunEvent::RunRecovered {
            source: EventSource::System,
            ..
        }
    )));
    assert_eq!(
        store
            .get_run(&workspace_id, &second.run_id)
            .unwrap()
            .snapshot
            .state,
        RunState::Interrupted
    );
    let new_run = queued(&mut store, &workspace_id, &session_id);
    let again = store
        .startup_recover("ignored after successful recovery", &mut Ids)
        .unwrap();
    assert!(again.interrupted_runs.is_empty());
    assert!(again.committed_events.is_empty());
    assert_eq!(
        store
            .get_run(&workspace_id, &new_run.run_id)
            .unwrap()
            .snapshot
            .state,
        RunState::Queued
    );
    drop(store);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    reopened.bootstrap(workspace_seed()).unwrap();
    let report = reopened.startup_recover(LATER, &mut Ids).unwrap();
    assert_eq!(report.interrupted_runs.len(), 1);
    assert_eq!(report.interrupted_runs[0].run_id, new_run.run_id);
}

#[test]
fn layout_receipt_revision_and_rollback_are_atomic() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, _) = fresh(&directory.0);
    let initial = store
        .workspace_snapshot(&workspace_request(&workspace_id))
        .unwrap()
        .layout
        .unwrap();
    let first = SaveLayoutRequest {
        contract_version: 1,
        workspace_id: workspace_id.clone(),
        request_id: id(),
        expected_revision: 0,
        layout: initial,
    };
    let saved = store.save_layout(&first).unwrap();
    assert_eq!(saved.result.revision, 1);
    let mut second = SaveLayoutRequest {
        contract_version: 1,
        workspace_id: workspace_id.clone(),
        request_id: id(),
        expected_revision: 1,
        layout: saved.result.clone(),
    };
    second.layout.sidebar_width = 320.0;
    store.conn.execute_batch("CREATE TRIGGER reject_layout_receipt BEFORE INSERT ON receipts WHEN NEW.command_name='save_layout' BEGIN SELECT RAISE(ABORT,'test fault'); END;").unwrap();
    let receipt_count = stored_count(&store, "receipts");
    assert!(store.save_layout(&second).is_err());
    assert_eq!(stored_count(&store, "receipts"), receipt_count);
    assert_eq!(
        store
            .workspace_snapshot(&workspace_request(&workspace_id))
            .unwrap()
            .layout
            .unwrap(),
        saved.result
    );
    store
        .conn
        .execute_batch("DROP TRIGGER reject_layout_receipt;")
        .unwrap();
    let latest = store.save_layout(&second).unwrap().result;
    assert_eq!(latest.revision, 2);
    let replay = store.save_layout(&first).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.result.revision, 1);
    assert_eq!(
        store
            .workspace_snapshot(&workspace_request(&workspace_id))
            .unwrap()
            .layout
            .unwrap(),
        latest
    );
    let mut stale = first.clone();
    stale.request_id = id();
    assert_eq!(
        store.save_layout(&stale).unwrap_err().code,
        "layout_revision_conflict"
    );
    let mut conflict = first.clone();
    conflict.layout.sidebar_width = 301.0;
    assert_eq!(
        store.save_layout(&conflict).unwrap_err().code,
        "request_id_conflict"
    );
    drop(store);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    assert_eq!(
        reopened
            .bootstrap(workspace_seed())
            .unwrap()
            .layout
            .unwrap(),
        latest
    );
}

#[test]
fn unsupported_and_corrupt_layouts_remain_stored_and_cannot_be_overwritten() {
    for (raw, code) in [
        (
            "{\"layout_version\":2,\"preserved\":true}",
            "unsupported_layout_version",
        ),
        ("{broken", "layout_invalid"),
    ] {
        let directory = TemporaryDirectory::new();
        let (mut store, workspace_id, _) = fresh(&directory.0);
        let valid = store
            .workspace_snapshot(&workspace_request(&workspace_id))
            .unwrap()
            .layout
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE workspaces SET layout_json=?1 WHERE workspace_id=?2",
                params![raw, workspace_id],
            )
            .unwrap();
        drop(store);
        let mut reopened = SqliteStore::open(&directory.0).unwrap();
        let snapshot = reopened.bootstrap(workspace_seed()).unwrap();
        assert!(snapshot.layout.is_none());
        assert_eq!(snapshot.layout_warning.unwrap().code, code);
        reopened.startup_recover(NOW, &mut Ids).unwrap();
        let request = SaveLayoutRequest {
            contract_version: 1,
            workspace_id: workspace_id.clone(),
            request_id: id(),
            expected_revision: 0,
            layout: valid,
        };
        let before = stored_count(&reopened, "receipts");
        assert_eq!(reopened.save_layout(&request).unwrap_err().code, code);
        assert_eq!(stored_count(&reopened, "receipts"), before);
        let preserved: String = reopened
            .conn
            .query_row("SELECT layout_json FROM workspaces", [], |row| row.get(0))
            .unwrap();
        assert_eq!(preserved, raw);
    }
}

#[test]
fn unsupported_unversioned_and_corrupt_databases_preserve_original_bytes() {
    for mode in 0..4 {
        let directory = TemporaryDirectory::new();
        let database = directory.0.join("execution.sqlite3");
        if mode == 3 {
            fs::write(&database, b"corrupt execution database fixture").unwrap();
        } else {
            let connection = Connection::open(&database).unwrap();
            match mode {
                0 => connection
                    .execute_batch("PRAGMA user_version=2; CREATE TABLE sentinel(value TEXT);")
                    .unwrap(),
                1 => connection
                    .execute_batch("CREATE TABLE sentinel(value TEXT);")
                    .unwrap(),
                2 => connection
                    .execute_batch("PRAGMA user_version=1; CREATE TABLE sentinel(value TEXT);")
                    .unwrap(),
                _ => unreachable!(),
            }
            drop(connection);
        }
        let before = fs::read(&database).unwrap();
        let failure = match SqliteStore::open(&directory.0) {
            Ok(_) => panic!("unknown database was accepted"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.code,
            if mode == 3 {
                "storage_unavailable"
            } else {
                "unsupported_storage_version"
            }
        );
        assert_eq!(fs::read(&database).unwrap(), before);
    }
}

#[test]
fn failed_enqueue_rolls_back_order_private_input_event_snapshot_and_receipt() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let request = start_request(&workspace_id, &session_id);
    let seed = run_seed();
    let receipts = stored_count(&store, "receipts");
    store.conn.execute_batch("CREATE TRIGGER reject_creation BEFORE INSERT ON events WHEN NEW.seq=1 BEGIN SELECT RAISE(ABORT,'test fault'); END;").unwrap();
    assert!(store.enqueue_run(&request, seed.clone()).is_err());
    assert_eq!(stored_count(&store, "runs"), 0);
    assert_eq!(stored_count(&store, "events"), 0);
    assert_eq!(stored_count(&store, "receipts"), receipts);
    let order: u64 = store
        .conn
        .query_row("SELECT last_enqueue FROM workspaces", [], |row| row.get(0))
        .unwrap();
    assert_eq!(order, 0);
    store
        .conn
        .execute_batch("DROP TRIGGER reject_creation;")
        .unwrap();
    let committed = store.enqueue_run(&request, seed).unwrap();
    assert_eq!(committed.result.enqueue_order, 1);
    assert_eq!(stored_count(&store, "runs"), 1);
    assert_eq!(stored_count(&store, "events"), 1);
    assert_eq!(stored_count(&store, "receipts"), receipts + 1);
}

#[test]
fn truncation_is_one_shot_across_transactions_and_forbids_later_ordinary_events() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let active = running(&mut store, initial);
    let marker = || {
        EventPayload::OutputTruncated(OutputTruncatedPayload {
            scope: TruncationScope::EventCount,
            discarded_bytes: None,
            discarded_events: 1,
        })
    };
    let stop = EventPayload::RunStateChanged(RunStateChangedPayload {
        from: RunState::Running,
        to: RunState::CancelRequested,
        error: None,
    });
    let stopped = store
        .append_batch(batch(&active, vec![marker(), stop]))
        .unwrap()
        .result;
    let before = stored_count(&store, "events");
    let usage = EventPayload::UsageUpdated(Usage {
        input_tokens: None,
        output_tokens: None,
        estimated: false,
    });
    assert_eq!(
        store
            .append_batch(batch(&stopped, vec![usage]))
            .unwrap_err()
            .code,
        "state_conflict"
    );
    assert_eq!(
        store
            .append_batch(batch(&stopped, vec![marker()]))
            .unwrap_err()
            .code,
        "state_conflict"
    );
    assert_eq!(stored_count(&store, "events"), before);
    assert_eq!(
        store
            .get_run(&workspace_id, &stopped.run_id)
            .unwrap()
            .snapshot,
        stopped
    );
    let terminal = store
        .append_batch(transition(&stopped, RunState::Cancelled))
        .unwrap()
        .result;
    assert_eq!(terminal.last_seq, before + 1);
    let (ordinary, reserve, mask): (u64, u64, u64) = store
        .conn
        .query_row(
            "SELECT ordinary_count,reserved_count,truncation_mask FROM runs WHERE run_id=?1",
            [&terminal.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((ordinary, reserve, mask), (3, 3, 4));
}

#[test]
fn duplicate_global_event_ids_and_missing_history_fail_without_partial_results() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let first_event = store
        .list_run_events(&page_request(&initial, 0, None, 10))
        .unwrap()
        .events
        .remove(0);
    let mut proposed = transition(&initial, RunState::Starting);
    let mut duplicate_header = proposed.events[0].header();
    duplicate_header.event_id = first_event.header().event_id;
    proposed.events[0] = RunEvent::from_parts(
        duplicate_header,
        EventPayload::RunStateChanged(RunStateChangedPayload {
            from: RunState::Queued,
            to: RunState::Starting,
            error: None,
        }),
    );
    assert_eq!(
        store.append_batch(proposed).unwrap_err().code,
        "state_conflict"
    );
    assert_eq!(stored_count(&store, "events"), 1);
    let active = running(&mut store, initial);
    store
        .conn
        .execute(
            "DELETE FROM events WHERE run_id=?1 AND seq=2",
            [&active.run_id],
        )
        .unwrap();
    assert_eq!(
        store
            .list_run_events(&page_request(&active, 0, None, 1))
            .unwrap_err()
            .code,
        "event_history_unavailable"
    );
}

#[test]
fn ordinary_receipt_capacity_preserves_all_33_emergency_cancellations() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let stopped_run = queued(&mut store, &workspace_id, &session_id);
    let stopped_request = cancel_request(&stopped_run);
    let stopped = store
        .cancel_run(
            &stopped_request,
            Some(transition(&stopped_run, RunState::Cancelled)),
        )
        .unwrap()
        .result;
    let first = queued(&mut store, &workspace_id, &session_id);
    let mut outstanding = vec![running(&mut store, first)];
    outstanding.extend((0..QUEUED_RUNS).map(|_| queued(&mut store, &workspace_id, &session_id)));
    assert_eq!(outstanding.len(), CANCEL_RECEIPTS);

    // Fill the real production budget, not fabricated metadata or test-only limits.
    let remaining = COMMAND_RECEIPTS as u64 - stored_count(&store, "receipts");
    let mut last_no_op = None;
    for _ in 0..remaining {
        let request = cancel_request(&stopped);
        store.cancel_run(&request, None).unwrap();
        last_no_op = Some(request);
    }
    assert_eq!(stored_count(&store, "receipts"), COMMAND_RECEIPTS as u64);
    assert_eq!(
        store
            .cancel_run(&cancel_request(&stopped), None)
            .unwrap_err()
            .code,
        "capacity_exceeded"
    );
    let mut stopped_active = None;
    for original in outstanding {
        let target = if original.state == RunState::Queued {
            RunState::Cancelled
        } else {
            RunState::CancelRequested
        };
        let request = cancel_request(&original);
        let saved = store
            .cancel_run(&request, Some(transition(&original, target)))
            .unwrap();
        assert_eq!(saved.result.state, target);
        assert!(store.cancel_run(&request, None).unwrap().replayed);
        assert_eq!(
            store
                .cancel_run(&cancel_request(&saved.result), None)
                .unwrap_err()
                .code,
            "capacity_exceeded"
        );
        if target == RunState::CancelRequested {
            stopped_active = Some(saved.result);
        }
    }
    let count: u64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM receipts WHERE emergency=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, CANCEL_RECEIPTS as u64);
    let active = stopped_active.unwrap();
    store
        .append_batch(transition(&active, RunState::Cancelled))
        .unwrap();
    assert!(
        store
            .cancel_run(&last_no_op.unwrap(), None)
            .unwrap()
            .replayed
    );
    assert_eq!(
        store
            .enqueue_run(&start_request(&workspace_id, &session_id), run_seed())
            .unwrap_err()
            .code,
        "capacity_exceeded"
    );
    assert!(store.cancel_run(&stopped_request, None).unwrap().replayed);
    assert!(store.next_queued(&workspace_id).unwrap().is_none());
    drop(store);
    // Audit the actual full 4096+33 ledger, including historical first-cancel
    // snapshots and thousands of ordinary no-op receipts sharing a prefix.
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    assert!(reopened
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .committed_events
        .is_empty());
    assert_eq!(
        stored_count(&reopened, "receipts"),
        (COMMAND_RECEIPTS + CANCEL_RECEIPTS) as u64
    );
    assert!(
        reopened
            .cancel_run(&stopped_request, None)
            .unwrap()
            .replayed
    );
}

fn assert_reopens_terminal_history(directory: &Path, expected: &RunSnapshot) {
    let mut reopened = SqliteStore::open(directory).unwrap();
    assert!(reopened
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .committed_events
        .is_empty());
    assert_eq!(
        reopened
            .get_run(&expected.workspace_id, &expected.run_id)
            .unwrap()
            .snapshot,
        *expected
    );
}

fn stop_for_budget(
    store: &mut SqliteStore,
    current: &RunSnapshot,
    scope: TruncationScope,
    bytes: Option<u64>,
) -> RunSnapshot {
    let effects = vec![
        EventPayload::OutputTruncated(OutputTruncatedPayload {
            scope,
            discarded_bytes: bytes,
            discarded_events: 1,
        }),
        EventPayload::RunStateChanged(RunStateChangedPayload {
            from: current.state,
            to: RunState::CancelRequested,
            error: None,
        }),
    ];
    let stopping = store.append_batch(batch(current, effects)).unwrap().result;
    store
        .append_batch(transition(&stopping, RunState::Cancelled))
        .unwrap()
        .result
}

#[test]
fn production_ordinary_event_limit_preserves_reserved_terminal_path() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let mut current = running(&mut store, initial);
    let mut remaining = ORDINARY_EVENTS - 3;
    while remaining > 0 {
        let count = remaining.min(PAGE_EVENTS);
        let mut state = current.state;
        let effects = (0..count)
            .map(|_| {
                let next = if state == RunState::Running {
                    RunState::WaitingInput
                } else {
                    RunState::Running
                };
                let payload = EventPayload::RunStateChanged(RunStateChangedPayload {
                    from: state,
                    to: next,
                    error: None,
                });
                state = next;
                payload
            })
            .collect();
        current = store.append_batch(batch(&current, effects)).unwrap().result;
        remaining -= count;
    }
    assert_eq!(current.last_seq, ORDINARY_EVENTS as u64);
    let extra = EventPayload::UsageUpdated(Usage {
        input_tokens: None,
        output_tokens: None,
        estimated: false,
    });
    assert_eq!(
        store
            .append_batch(batch(&current, vec![extra]))
            .unwrap_err()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(stored_count(&store, "events"), ORDINARY_EVENTS as u64);
    let terminal = stop_for_budget(&mut store, &current, TruncationScope::EventCount, None);
    assert_eq!(terminal.state, RunState::Cancelled);
    assert_eq!(terminal.last_seq, ORDINARY_EVENTS as u64 + 3);
    drop(store);
    assert_reopens_terminal_history(&directory.0, &terminal);
}

#[test]
fn production_persisted_text_limit_is_transactional_and_can_still_cancel() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let mut current = running(&mut store, initial);
    let text = format!("SIMULATED: {}", "x".repeat(TEXT_DELTA_BYTES - 11));
    assert_eq!(text.len(), TEXT_DELTA_BYTES);
    assert_eq!(PERSISTED_TEXT_BYTES % TEXT_DELTA_BYTES, 0);
    let mut chunks = PERSISTED_TEXT_BYTES / TEXT_DELTA_BYTES;
    while chunks > 0 {
        let count = chunks.min(48); // full encoded ordinary batch stays below 1 MiB
        let effects = (0..count)
            .map(|_| {
                EventPayload::TextDelta(TextDeltaPayload {
                    stream: TextStream::Assistant,
                    text: text.clone(),
                })
            })
            .collect();
        current = store.append_batch(batch(&current, effects)).unwrap().result;
        chunks -= count;
    }
    let before = stored_count(&store, "events");
    let extra = EventPayload::TextDelta(TextDeltaPayload {
        stream: TextStream::Assistant,
        text: "🧪".into(),
    });
    assert_eq!(
        store
            .append_batch(batch(&current, vec![extra]))
            .unwrap_err()
            .code,
        "capacity_exceeded"
    );
    assert_eq!(stored_count(&store, "events"), before);
    let terminal = stop_for_budget(
        &mut store,
        &current,
        TruncationScope::PersistedTextBytes,
        Some(4),
    );
    assert_eq!(terminal.state, RunState::Cancelled);
    let bytes: u64 = store
        .conn
        .query_row(
            "SELECT text_bytes FROM runs WHERE run_id=?1",
            [&terminal.run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bytes, PERSISTED_TEXT_BYTES as u64);
    drop(store);
    assert_reopens_terminal_history(&directory.0, &terminal);
}

#[test]
fn startup_audit_rejects_forged_history_snapshots_counters_and_oversized_rows() {
    // One compact mutation matrix exercises independent durable-data seams.
    for damage in [
        "event_id",
        "session_id",
        "sequence",
        "first_creation",
        "snapshot_time",
        "text_counter",
        "oversized_row",
        "oversized_nontext_row",
    ] {
        let directory = TemporaryDirectory::new();
        let (mut store, workspace_id, session_id) = fresh(&directory.0);
        let initial = queued(&mut store, &workspace_id, &session_id);
        let active = running(&mut store, initial);
        let current = store
            .append_batch(batch(
                &active,
                vec![EventPayload::TextDelta(TextDeltaPayload {
                    stream: TextStream::Assistant,
                    text: "SIMULATED audit".into(),
                })],
            ))
            .unwrap()
            .result;
        if damage == "snapshot_time" {
            let mut changed = current.clone();
            changed.updated_at = NOW.into();
            validate_snapshot(&changed).unwrap();
            store
                .conn
                .execute(
                    "UPDATE runs SET snapshot_json=?1 WHERE run_id=?2",
                    params![encode(&changed).unwrap(), current.run_id],
                )
                .unwrap();
        } else if damage == "text_counter" {
            store
                .conn
                .execute(
                    "UPDATE runs SET text_bytes=text_bytes+1 WHERE run_id=?1",
                    [&current.run_id],
                )
                .unwrap();
        } else {
            let seq = if damage == "first_creation" { 1 } else { 2 };
            let raw: String = store
                .conn
                .query_row(
                    "SELECT event_json FROM events WHERE run_id=?1 AND seq=?2",
                    params![current.run_id, seq],
                    |row| row.get(0),
                )
                .unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            match damage {
                "event_id" => value["event_id"] = id().into(),
                "session_id" => value["session_id"] = id().into(),
                "sequence" => value["seq"] = 99.into(),
                "first_creation" => {
                    value["kind"] = "run_state_changed".into();
                    value["payload"] =
                        serde_json::json!({"from":"queued", "to":"starting", "error":null});
                }
                _ => {}
            }
            let mut damaged = serde_json::to_string(&value).unwrap();
            if damage == "oversized_row" {
                damaged.push_str(&" ".repeat(EVENT_BYTES + 1));
            } else if damage == "oversized_nontext_row" {
                damaged.push_str(&" ".repeat(NONTEXT_EVENT_BYTES + 1 - damaged.len()));
                assert!(damaged.len() <= EVENT_BYTES);
                // Semantically valid decoded event, but over the stored wire budget.
                validate_event(&serde_json::from_str::<RunEvent>(&damaged).unwrap()).unwrap();
            }
            store
                .conn
                .execute(
                    "UPDATE events SET event_json=?1 WHERE run_id=?2 AND seq=?3",
                    params![damaged, current.run_id, seq],
                )
                .unwrap();
        }
        drop(store);
        let database = directory.0.join("execution.sqlite3");
        let before = fs::read(&database).unwrap();
        let failure = SqliteStore::open(&directory.0).err().expect(damage);
        assert_eq!(failure.code, "storage_unavailable", "{damage}");
        assert_eq!(fs::read(&database).unwrap(), before, "{damage}");
    }
}

#[test]
fn startup_rejects_unexpected_and_orphan_sidecars_without_touching_them() {
    for existing_database in [false, true] {
        for suffix in ["wal", "shm", "journal"] {
            if existing_database && suffix == "journal" {
                continue;
            }
            let directory = TemporaryDirectory::new();
            if existing_database {
                drop(fresh(&directory.0));
            }
            let database = directory.0.join("execution.sqlite3");
            let before = fs::read(&database).ok();
            let sidecar = directory.0.join(format!("execution.sqlite3-{suffix}"));
            let contents = b"SIMULATED sidecar: must be preserved";
            fs::write(&sidecar, contents).unwrap();
            assert_eq!(
                SqliteStore::open(&directory.0).err().expect(suffix).code,
                "storage_unavailable"
            );
            assert_eq!(fs::read(&database).ok(), before);
            assert_eq!(fs::read(&sidecar).unwrap(), contents);
        }
    }
}

#[test]
fn startup_rejects_wal_header_even_after_sidecars_were_checkpointed() {
    let directory = TemporaryDirectory::new();
    drop(fresh(&directory.0));
    let database = directory.0.join("execution.sqlite3");
    let conn = Connection::open(&database).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(conn);
    assert!(!directory.0.join("execution.sqlite3-wal").exists());
    let before = fs::read(&database).unwrap();
    assert_eq!(before[18], 2);
    assert_eq!(
        SqliteStore::open(&directory.0)
            .err()
            .expect("WAL must fail closed")
            .code,
        "storage_unavailable"
    );
    assert_eq!(fs::read(&database).unwrap(), before);
    assert!(!directory.0.join("execution.sqlite3-shm").exists());
}

fn assert_startup_refuses_corrupt_history(malformed: bool) {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let current = running(&mut store, initial);
    let raw: String = store
        .conn
        .query_row(
            "SELECT event_json FROM events WHERE run_id=?1 AND seq=2",
            [&current.run_id],
            |row| row.get(0),
        )
        .unwrap();
    let damaged = if malformed {
        "{broken".into()
    } else {
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value["payload"]["from"] = "running".into();
        value["payload"]["to"] = "succeeded".into();
        // Individually valid, but cannot follow the committed queued creation.
        validate_event(&serde_json::from_value::<RunEvent>(value.clone()).unwrap()).unwrap();
        serde_json::to_string(&value).unwrap()
    };
    store
        .conn
        .execute(
            "UPDATE events SET event_json=?1 WHERE run_id=?2 AND seq=2",
            params![damaged, current.run_id],
        )
        .unwrap();
    drop(store);
    let database = directory.0.join("execution.sqlite3");
    let original = fs::read(&database).unwrap();
    let outcome = (|| -> StoreResult<RecoveryReport> {
        let mut reopened = SqliteStore::open(&directory.0)?;
        reopened.bootstrap(workspace_seed())?;
        reopened.startup_recover(LATER, &mut Ids)
    })();
    assert_eq!(outcome.unwrap_err().code, "storage_unavailable");
    assert!(
        fs::read(&database).unwrap() == original,
        "corrupt committed history must not receive recovery writes"
    );
}

#[test]
fn startup_refuses_malformed_event_before_recovery_writes() {
    assert_startup_refuses_corrupt_history(true);
}
#[test]
fn startup_refuses_contradictory_event_before_recovery_writes() {
    assert_startup_refuses_corrupt_history(false);
}

// Test-only subprocess entry, explicitly exercised by hot_journal_recovery below.
// This is not Workbench, a provider, or production process-launch code.
#[test]
#[ignore = "entry point for the bounded owned SQLite crash fixture"]
fn crash_fixture_child() {
    let directory = PathBuf::from(std::env::var_os("WORKBENCH_SQLITE_CRASH_DIR").unwrap());
    let directory = directory.canonicalize().unwrap();
    assert_eq!(
        directory.parent().unwrap(),
        std::env::temp_dir().canonicalize().unwrap()
    );
    let name = directory.file_name().unwrap().to_str().unwrap();
    let suffix = name.strip_prefix("agent-workbench-storage-").unwrap();
    validate_id(suffix).unwrap();
    let nonce = std::env::var("WORKBENCH_SQLITE_CRASH_NONCE").unwrap();
    assert_eq!(
        fs::read_to_string(directory.join("crash-fixture-owner")).unwrap(),
        nonce
    );
    let database = directory.join("execution.sqlite3");
    assert_eq!(
        database.canonicalize().unwrap().parent().unwrap(),
        directory
    );
    let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=1; PRAGMA cache_spill=ON; BEGIN IMMEDIATE; DELETE FROM receipts;").unwrap();
    conn.execute(
        "UPDATE workspaces SET record_json=?1",
        ["x".repeat(2 * 1024 * 1024)],
    )
    .unwrap();
    // Deliberately skip SQLite/Rust destructors after forcing dirty page spill.
    std::process::exit(0);
}

fn leave_hot_journal(directory: &TemporaryDirectory) {
    let database = directory.0.join("execution.sqlite3");
    let before = fs::metadata(&database).unwrap().len();
    let nonce = id();
    fs::write(directory.0.join("crash-fixture-owner"), &nonce).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "storage::tests::crash_fixture_child",
            "--ignored",
        ])
        .env("WORKBENCH_SQLITE_CRASH_DIR", &directory.0)
        .env("WORKBENCH_SQLITE_CRASH_NONCE", &nonce)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            // Only the child test harness owned above, never the Workbench app.
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("SQLite crash fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let journal = fs::read(directory.0.join("execution.sqlite3-journal")).unwrap();
    assert!(journal.len() > 512 && journal[..8].iter().any(|byte| *byte != 0));
    assert!(
        fs::metadata(&database).unwrap().len() > before,
        "fixture must actually spill dirty database pages"
    );
}

fn validation_children(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".execution-validation-")
        })
        .collect()
}

#[test]
fn rejected_hot_images_preserve_database_journal_and_cleanup_owned_copy() {
    for unsupported in [false, true] {
        let directory = TemporaryDirectory::new();
        let (mut store, workspace_id, session_id) = fresh(&directory.0);
        let initial = queued(&mut store, &workspace_id, &session_id);
        let active = running(&mut store, initial);
        if unsupported {
            store.conn.pragma_update(None, "user_version", 2).unwrap();
        } else {
            store
                .conn
                .execute(
                    "UPDATE events SET event_json='{broken' WHERE run_id=?1 AND seq=2",
                    [&active.run_id],
                )
                .unwrap();
        }
        drop(store);
        leave_hot_journal(&directory);
        let database = directory.0.join("execution.sqlite3");
        let journal = directory.0.join("execution.sqlite3-journal");
        let before = (fs::read(&database).unwrap(), fs::read(&journal).unwrap());
        let failure = SqliteStore::open(&directory.0)
            .err()
            .expect("rejected recovered copy");
        assert_eq!(
            failure.code,
            if unsupported {
                "unsupported_storage_version"
            } else {
                "storage_unavailable"
            }
        );
        assert_eq!(fs::read(&database).unwrap(), before.0);
        assert_eq!(fs::read(&journal).unwrap(), before.1);
        assert!(validation_children(&directory.0).is_empty());
    }
}

#[test]
fn startup_rejects_attached_journal_footer_and_never_deletes_existing_validation_paths() {
    let directory = TemporaryDirectory::new();
    drop(fresh(&directory.0));
    let stale = directory.0.join(format!(".execution-validation-{}", id()));
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("owner-marker"), b"not owned by this open").unwrap();
    let database = directory.0.join("execution.sqlite3");
    let journal = directory.0.join("execution.sqlite3-journal");
    let before = fs::read(&database).unwrap();
    let mut footer_fixture = vec![0; 512];
    footer_fixture.extend_from_slice(b"\xd9\xd5\x05\xf9\x20\xa1\x63\xd7");
    fs::write(&journal, &footer_fixture).unwrap();
    assert_eq!(
        SqliteStore::open(&directory.0)
            .err()
            .expect("attached family")
            .code,
        "storage_unavailable"
    );
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(fs::read(&journal).unwrap(), footer_fixture);
    fs::remove_file(&journal).unwrap(); // exact synthetic file created by this test
    leave_hot_journal(&directory);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    reopened.startup_recover(LATER, &mut Ids).unwrap();
    assert_eq!(validation_children(&directory.0), vec![stale.clone()]);
    assert_eq!(
        fs::read(stale.join("owner-marker")).unwrap(),
        b"not owned by this open"
    );
}

#[test]
fn hot_journal_recovery_preserves_committed_receipts_and_interrupts_once() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let request = start_request(&workspace_id, &session_id);
    let original = store.enqueue_run(&request, run_seed()).unwrap().result;
    let active = running(&mut store, original.clone());
    let receipts = stored_count(&store, "receipts");
    drop(store);
    leave_hot_journal(&directory);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    reopened.bootstrap(workspace_seed()).unwrap();
    let report = reopened.startup_recover(LATER, &mut Ids).unwrap();
    assert!(validation_children(&directory.0).is_empty());
    assert_eq!(report.interrupted_runs.len(), 1);
    assert_eq!(report.interrupted_runs[0].state, RunState::Interrupted);
    assert_eq!(report.interrupted_runs[0].last_seq, active.last_seq + 1);
    assert_eq!(stored_count(&reopened, "receipts"), receipts);
    assert!(reopened.enqueue_run(&request, run_seed()).unwrap().replayed);
    assert_eq!(
        reopened
            .get_run(&workspace_id, &original.run_id)
            .unwrap()
            .snapshot
            .state,
        RunState::Interrupted
    );
    assert!(reopened
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .committed_events
        .is_empty());
}

#[test]
fn system_sourced_usage_is_accepted_and_returned_by_native_history() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, session_id) = fresh(&directory.0);
    let initial = queued(&mut store, &workspace_id, &session_id);
    let active = running(&mut store, initial);
    let updated = store
        .append_batch(batch(
            &active,
            vec![EventPayload::UsageUpdated(Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
            })],
        ))
        .unwrap()
        .result;
    let page = store
        .list_run_events(&page_request(&updated, 0, None, 200))
        .unwrap();
    assert!(matches!(
        page.events.last().unwrap(),
        RunEvent::UsageUpdated {
            source: EventSource::System,
            ..
        }
    ));
}
