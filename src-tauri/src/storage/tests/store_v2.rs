//! Fresh v2 store: own directory, exclusive lock, audit on open, readiness only
//! after one recovery, and refusal (with byte preservation) of anything else.
use super::admission_v2::{codex, create, digest, fake, family};
use super::*;
use crate::execution_v2::{
    self as v2,
    admission::StartRequest,
    ports::{AdmissionJournal, AppendBatch, JournalReader, JournalWriter},
    validation as policy,
};
use crate::storage::store_v2::V2Store;

pub(super) fn ready(directory: &Path) -> (V2Store, String) {
    let mut store = V2Store::open(directory).unwrap();
    let snapshot = store.bootstrap(workspace_seed()).unwrap();
    assert!(!snapshot.execution_available);
    store.startup_recover(NOW, &mut Ids).unwrap();
    (store, snapshot.workspace.workspace_id)
}
fn to(run: &v2::RunSnapshot, state: v2::RunState) -> AppendBatch {
    let event = v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(),
            workspace_id: run.workspace_id.clone(),
            session_id: run.session_id.clone(),
            run_id: run.run_id.clone(),
            provider_id: run.provider_id,
            source: v2::EventSource::System,
            seq: run.last_seq + 1,
            occurred_at: LATER.into(),
        },
        v2::EventPayload::RunStateChanged(v2::RunStateChangedPayload {
            from: run.state,
            to: state,
            error: None,
        }),
    );
    let next = policy::apply_event(run, &event).unwrap();
    AppendBatch {
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        expected_last_seq: run.last_seq,
        expected_state: run.state,
        events: vec![event],
        next_snapshot: next,
    }
}

#[test]
fn fresh_store_initializes_v2_schema_and_gates_everything_on_recovery() {
    let directory = TemporaryDirectory::new();
    let mut store = V2Store::open(&directory.0).unwrap();
    let snapshot = store.bootstrap(workspace_seed()).unwrap();
    let w = snapshot.workspace.workspace_id.clone();
    assert!(snapshot.layout.is_some() && snapshot.sessions.is_empty());
    let version: i64 = store
        .raw()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
    // Not ready: admission AND reads (incl. queue peeks) are refused.
    assert!(!store.is_ready());
    assert_eq!(
        store
            .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
            .err()
            .unwrap()
            .code,
        "storage_unavailable"
    );
    assert!(store.next_queued(&w).is_err());
    assert!(store
        .startup_recover(NOW, &mut Ids)
        .unwrap()
        .interrupted_runs
        .is_empty());
    assert!(store.is_ready());
    let s = store
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    store
        .enqueue_run(
            StartRequest::CodexText(&codex(&w, &s.session_id)),
            run_seed(),
        )
        .unwrap();
    // Bootstrap is idempotent: the same workspace, never a second one.
    assert_eq!(
        store
            .bootstrap(workspace_seed())
            .unwrap()
            .workspace
            .workspace_id,
        w
    );
}

#[test]
fn reopen_audits_and_interrupts_all_unfinished_work_exactly_once() {
    let directory = TemporaryDirectory::new();
    let (mut store, w) = ready(&directory.0);
    let s = store
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let f = store
        .create_session(&create(&w, v2::ProviderId::Fake), session_seed())
        .unwrap()
        .result;
    let first = store
        .enqueue_run(
            StartRequest::CodexText(&codex(&w, &s.session_id)),
            run_seed(),
        )
        .unwrap()
        .result;
    let queued = store
        .enqueue_run(StartRequest::Fake(&fake(&w, &f.session_id)), run_seed())
        .unwrap()
        .result;
    let starting = store
        .append_batch(to(&first, v2::RunState::Starting))
        .unwrap()
        .result;
    assert_eq!(
        V2Store::open(&directory.0).err().unwrap().code,
        "storage_busy"
    );
    drop(store);
    let mut store = V2Store::open(&directory.0).unwrap();
    assert_eq!(
        store
            .bootstrap(workspace_seed())
            .unwrap()
            .workspace
            .workspace_id,
        w
    );
    let report = store.startup_recover(LATER, &mut Ids).unwrap();
    let ids: Vec<_> = report
        .interrupted_runs
        .iter()
        .map(|r| r.run_id.clone())
        .collect();
    assert_eq!(ids, [starting.run_id.clone(), queued.run_id.clone()]);
    let before = digest(store.raw());
    assert!(store
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .interrupted_runs
        .is_empty());
    assert_eq!(digest(store.raw()), before);
    assert!(store.next_queued(&w).unwrap().is_none());
    // New work after recovery is never interrupted by the same open.
    store
        .enqueue_run(StartRequest::Fake(&fake(&w, &f.session_id)), run_seed())
        .unwrap();
    assert!(store
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .interrupted_runs
        .is_empty());
    assert!(store.next_queued(&w).unwrap().is_some());
}

#[test]
fn legacy_unversioned_and_wal_storage_are_refused_and_preserved() {
    // Legacy v1 store in this directory: never upgraded in place.
    let directory = TemporaryDirectory::new();
    let (store, _, _) = fresh(&directory.0);
    drop(store);
    let original = family(&directory.0);
    assert_eq!(
        V2Store::open(&directory.0).err().unwrap().code,
        "unsupported_storage_version"
    );
    assert_eq!(family(&directory.0), original);
    // Unversioned nonempty database.
    let directory = TemporaryDirectory::new();
    {
        let conn = Connection::open(directory.0.join("execution.sqlite3")).unwrap();
        conn.execute_batch("CREATE TABLE foreign_data(x)").unwrap();
    }
    let original = family(&directory.0);
    assert_eq!(
        V2Store::open(&directory.0).err().unwrap().code,
        "unsupported_storage_version"
    );
    assert_eq!(family(&directory.0), original);
    // WAL sidecar.
    let directory = TemporaryDirectory::new();
    fs::write(directory.0.join("execution.sqlite3-wal"), b"x").unwrap();
    assert!(V2Store::open(&directory.0).is_err());
    assert_eq!(
        fs::read(directory.0.join("execution.sqlite3-wal")).unwrap(),
        b"x"
    );
    // Tampered v2 history fails the open-time audit.
    let directory = TemporaryDirectory::new();
    let (mut store, w) = ready(&directory.0);
    let s = store
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    store
        .enqueue_run(
            StartRequest::CodexText(&codex(&w, &s.session_id)),
            run_seed(),
        )
        .unwrap();
    store
        .raw()
        .execute("UPDATE runs SET text_bytes=5", [])
        .unwrap();
    drop(store);
    assert!(V2Store::open(&directory.0).is_err());
}
