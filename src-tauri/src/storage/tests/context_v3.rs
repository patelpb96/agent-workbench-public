//! Storage 3: the `codex_text_v2` profile with carried same-session context, and the
//! in-place 2 -> 3 migration of an existing fresh-v2 journal. Native fixtures only.
use super::admission_v2::{codex, create};
use super::store_v2::ready;
use super::writer_v2::{batch, reported, text, to};
use super::*;
use crate::execution_v2::{
    self as v2,
    admission::StartRequest,
    ports::{AdmissionJournal, JournalReader, JournalWriter, QueuedInput},
    validation as policy,
};
use crate::storage::store_v2::V2Store;

fn session(store: &mut V2Store, w: &str) -> String {
    store
        .create_session(&create(w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result
        .session_id
}
fn start(store: &mut V2Store, request: &v2::StartCodexRunRequest) -> StoreResult<v2::RunSnapshot> {
    store
        .enqueue_run(StartRequest::CodexText(request), run_seed())
        .map(|commit| commit.result)
}
/// Enqueue and drive a Codex v1 run to `succeeded` with a one-chunk reply.
fn succeed(store: &mut V2Store, w: &str, s: &str, prompt: &str) -> v2::RunSnapshot {
    let mut request = codex(w, s);
    request.prompt = prompt.into();
    let run = start(store, &request).unwrap();
    let run = store.append_batch(to(&run, v2::RunState::Starting)).unwrap().result;
    let run = store.append_batch(to(&run, v2::RunState::Running)).unwrap().result;
    let run = store
        .append_batch(batch(&run, vec![text("reply"), reported()]))
        .unwrap()
        .result;
    store.append_batch(to(&run, v2::RunState::Succeeded)).unwrap().result
}
fn follow_up(w: &str, s: &str, context: &[&v2::RunSnapshot]) -> v2::StartCodexRunRequest {
    let mut request = codex(w, s);
    request.prompt = "and now?".into();
    request.profile_id = v2::CodexProfileId::CodexTextV2;
    request.context_run_ids = Some(context.iter().map(|r| r.run_id.clone()).collect());
    request
}

#[test]
fn v2_profile_carries_only_earlier_succeeded_runs_of_the_same_session() {
    let directory = TemporaryDirectory::new();
    let (mut store, w) = ready(&directory.0);
    let (s, other) = (session(&mut store, &w), session(&mut store, &w));
    let first = succeed(&mut store, &w, &s, "one");
    let second = succeed(&mut store, &w, &s, "two");
    let elsewhere = succeed(&mut store, &w, &other, "three");

    let request = follow_up(&w, &s, &[&first, &second]);
    let run = start(&mut store, &request).unwrap();
    assert_eq!(run.profile_id, Some(v2::CodexProfileId::CodexTextV2));
    let stored = store.get_run(&w, &run.run_id).unwrap();
    assert_eq!(stored.input.context_run_ids(), [first.run_id.clone(), second.run_id.clone()]);
    assert_eq!(stored.input.prompt(), "and now?");
    assert!(matches!(stored.input, QueuedInput::CodexText { profile_id: v2::CodexProfileId::CodexTextV2, .. }));
    // The identical request replays; nothing is admitted twice.
    let replay = store
        .enqueue_run(StartRequest::CodexText(&request), run_seed())
        .unwrap();
    assert!(replay.replayed && replay.result == run);
    // Drain the queued follow-up so the next admissions see an idle slot.
    let run = store.append_batch(to(&run, v2::RunState::Starting)).unwrap().result;
    let run = store.append_batch(to(&run, v2::RunState::Running)).unwrap().result;
    let run = store
        .append_batch(batch(&run, vec![text("ok"), reported()]))
        .unwrap()
        .result;
    let third = store.append_batch(to(&run, v2::RunState::Succeeded)).unwrap().result;

    let refused = |store: &mut V2Store, request: v2::StartCodexRunRequest, code: &str| {
        let error = start(store, &request).expect_err("must be refused");
        assert_eq!(error.code, code, "{request:?}");
    };
    // Another session's run, an unknown run, descending order, a duplicate id.
    refused(&mut store, follow_up(&w, &s, &[&elsewhere]), "context_unavailable");
    let mut unknown = follow_up(&w, &s, &[&first]);
    unknown.context_run_ids = Some(vec![id()]);
    refused(&mut store, unknown, "context_unavailable");
    refused(&mut store, follow_up(&w, &s, &[&second, &first]), "context_unavailable");
    refused(&mut store, follow_up(&w, &s, &[&first, &first]), "validation_failed");
    // A run that has not succeeded cannot be carried.
    let pending = start(&mut store, &codex(&w, &s)).unwrap();
    refused(&mut store, follow_up(&w, &s, &[&first, &pending]), "context_unavailable");
    // Shape rules: v1 never carries, v2 always does, at most six.
    let mut v1_with = codex(&w, &s);
    v1_with.context_run_ids = Some(vec![first.run_id.clone()]);
    refused(&mut store, v1_with, "validation_failed");
    let mut v2_without = follow_up(&w, &s, &[&first]);
    v2_without.context_run_ids = None;
    refused(&mut store, v2_without, "validation_failed");
    let mut many = follow_up(&w, &s, &[&first]);
    many.context_run_ids = Some((0..7).map(|_| id()).collect());
    assert!(policy::validate_codex_start(&many).is_err());
    let mut empty = follow_up(&w, &s, &[&first]);
    empty.context_run_ids = Some(Vec::new());
    assert!(policy::validate_codex_start(&empty).is_err());
    assert!(third.state == v2::RunState::Succeeded);

    // Reopening audits the whole journal (schema 3, context eligibility, receipts).
    drop(store);
    let (mut store, _) = ready(&directory.0);
    assert_eq!(
        store.get_run(&w, &run.run_id).unwrap().input.context_run_ids().len(),
        2
    );
}

#[test]
fn v1_canonical_receipt_bytes_do_not_change() {
    // Absent optional context never serializes, so old receipts stay byte-identical.
    let request = codex("w", "s");
    let text = serde_json::to_string(&request).unwrap();
    assert!(!text.contains("context_run_ids"), "{text}");
    let back: v2::StartCodexRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(back, request);
}

/// A real storage-2 file: the empty v2 schema filled from a v3 journal (rows that
/// only use storage-2 features), exactly what an already-installed app has.
fn downgrade_copy(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    let mut conn = Connection::open(target.join("execution.sqlite3")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    crate::storage::upgrade_preview::migrate_copy(&mut conn, |_| Ok(())).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    conn.execute(
        "ATTACH DATABASE ?1 AS src",
        [source.join("execution.sqlite3").to_str().unwrap()],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO workspaces SELECT * FROM src.workspaces;
         INSERT INTO sessions SELECT * FROM src.sessions;
         INSERT INTO runs SELECT run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,prompt,scenario,
             ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count,provider_id,profile_id FROM src.runs;
         INSERT INTO events SELECT * FROM src.events;
         INSERT INTO receipts SELECT * FROM src.receipts;
         DETACH DATABASE src;",
    )
    .unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 2);
}
fn run_rows(conn: &Connection) -> Vec<(String, String, String, String)> {
    conn.prepare("SELECT run_id,prompt,state,snapshot_json FROM runs ORDER BY run_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn backups(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("execution.sqlite3.pre-v3-") && n.ends_with(".bak"))
        })
        .collect()
}

#[test]
fn an_existing_storage_2_journal_is_backed_up_and_migrated_in_place() {
    let origin = TemporaryDirectory::new();
    let (first_id, rows_before) = {
        let (mut store, w) = ready(&origin.0);
        let s = session(&mut store, &w);
        let first = succeed(&mut store, &w, &s, "carry me");
        let rows = run_rows(store.raw());
        let _ = &w;
        (first.run_id, rows)
    };
    let old = TemporaryDirectory::new();
    downgrade_copy(&origin.0, &old.0);
    assert!(backups(&old.0).is_empty());

    // Opening migrates: schema 3, original rows intact, a verified v2 backup beside it.
    let (mut store, w) = ready(&old.0);
    let version: i64 = store.raw().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 3);
    assert_eq!(run_rows(store.raw()), rows_before);
    let saved = backups(&old.0);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let backup = Connection::open_with_flags(&saved[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(backup.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(run_rows(&backup), rows_before);
    drop(backup);

    // The migrated journal accepts a carried-context turn for a pre-existing run.
    let s: String = store
        .raw()
        .query_row("SELECT session_id FROM runs LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let mut request = codex(&w, &s);
    request.profile_id = v2::CodexProfileId::CodexTextV2;
    request.context_run_ids = Some(vec![first_id.clone()]);
    let run = start(&mut store, &request).unwrap();
    assert_eq!(run.profile_id, Some(v2::CodexProfileId::CodexTextV2));

    // Reopening neither migrates nor backs up again, and audits cleanly.
    drop(store);
    let (mut store, _) = ready(&old.0);
    assert_eq!(backups(&old.0).len(), 1);
    assert_eq!(store.get_run(&w, &run.run_id).unwrap().input.context_run_ids(), [first_id]);
}

#[test]
fn a_failed_migration_rolls_back_and_restores_foreign_keys() {
    let origin = TemporaryDirectory::new();
    {
        let (mut store, w) = ready(&origin.0);
        let s = session(&mut store, &w);
        succeed(&mut store, &w, &s, "keep me");
    }
    let old = TemporaryDirectory::new();
    downgrade_copy(&origin.0, &old.0);
    let mut conn = Connection::open(old.0.join("execution.sqlite3")).unwrap();
    let before = run_rows(&conn);
    let failed = crate::storage::upgrade_preview::migrate_with(
        &mut conn,
        crate::storage::store_v2::MIGRATION_V3,
        |_| Err(unavailable()),
    );
    assert!(failed.is_err());
    // Still storage 2 with the original table shape and rows; FK enforcement is back on.
    assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(run_rows(&conn), before);
    let has_context: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('runs') WHERE name='context_json'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!has_context);
    assert_eq!(conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    // And the real open path still migrates the same file afterwards.
    drop(conn);
    let (mut store, _) = ready(&old.0);
    assert_eq!(store.raw().query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
}

/// Opt-in probe for a COPY of a real journal: `AW_JOURNAL_COPY=<dir containing execution.sqlite3>`.
/// Never point it at the live directory; it migrates in place (after taking its own backup).
#[test]
#[ignore = "needs AW_JOURNAL_COPY; run explicitly against a copy of a real journal"]
fn real_journal_copy_migrates_and_audits() {
    let directory = PathBuf::from(std::env::var("AW_JOURNAL_COPY").expect("AW_JOURNAL_COPY"));
    let before = Connection::open_with_flags(
        directory.join("execution.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let version: i64 = before.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    let rows = run_rows(&before);
    drop(before);
    let (mut store, _) = ready(&directory);
    let after: i64 = store.raw().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(after, 3);
    assert_eq!(run_rows(store.raw()).len(), rows.len());
    println!("migrated {version} -> {after}: {} runs, {} backup(s)", rows.len(), backups(&directory).len());
}
