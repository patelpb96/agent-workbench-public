use super::*;
use crate::execution_v2::{stored::StoredRunEvent, validation as v2};
use crate::storage::upgrade_preview::{migrate_copy, LegacyUpgradePreview};

fn family(path: &Path) -> Vec<(String, Vec<u8>)> {
    [
        "execution.sqlite3",
        "execution.sqlite3-journal",
        "execution.sqlite3-wal",
        "execution.sqlite3-shm",
    ]
    .into_iter()
    .filter_map(|n| fs::read(path.join(n)).ok().map(|b| (n.into(), b)))
    .collect()
}
fn preview_children(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".execution-v2-preview-")
        })
        .collect()
}
fn rows(conn: &Connection, table: &str) -> Vec<String> {
    let sql=match table {
        "events" => "SELECT event_json FROM events ORDER BY run_id,seq",
        "receipts" => "SELECT workspace_id||command_name||request_id||arguments_json||result_json||emergency FROM receipts ORDER BY workspace_id,command_name,request_id",
        "sessions" => "SELECT snapshot_json FROM sessions ORDER BY session_id",
        "workspaces" => "SELECT record_json||layout_json||layout_revision||last_enqueue FROM workspaces",
        _ => panic!("fixed test allowlist"),
    };
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

#[test]
fn preview_holds_exclusive_ownership_without_opening_original_for_writes() {
    let dir = TemporaryDirectory::new();
    let (store, _, _) = fresh(&dir.0);
    let before = family(&dir.0);
    let err = LegacyUpgradePreview::inspect(&dir.0, LATER).err().unwrap();
    assert_eq!(err.code, "storage_busy");
    assert_eq!(family(&dir.0), before);
    assert!(preview_children(&dir.0).is_empty());
    drop(store);
    let p = LegacyUpgradePreview::inspect(&dir.0, LATER).unwrap();
    assert_eq!(
        SqliteStore::open(&dir.0).err().unwrap().code,
        "storage_busy"
    );
    p.close().unwrap();
    assert!(preview_children(&dir.0).is_empty());
    assert_eq!(family(&dir.0), before);
}

#[test]
fn copy_migration_keeps_legacy_json_receipts_layout_and_recovery_version_exact() {
    let dir = TemporaryDirectory::new();
    let (mut store, w, s) = fresh(&dir.0);
    let run = queued(&mut store, &w, &s);
    let tables: [Vec<String>; 4] = [
        rows(&store.conn, "sessions"),
        rows(&store.conn, "workspaces"),
        rows(&store.conn, "receipts"),
        rows(&store.conn, "events"),
    ];
    drop(store);
    let before = family(&dir.0);
    let p = LegacyUpgradePreview::inspect(&dir.0, LATER).unwrap();
    assert!(v2::validate_bootstrap(&p.bootstrap()).is_ok());
    assert!(p.bootstrap().snapshot.is_none());
    assert!(!p.projected_history().execution_available);
    assert_eq!(p.interrupted_runs(), 1);
    assert_eq!(p.projected_history().runs[0].state.as_str(), "interrupted");
    let c = p.connection();
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    for (index, table) in ["sessions", "workspaces", "receipts"].iter().enumerate() {
        assert_eq!(rows(c, table), tables[index]);
    }
    let events = rows(c, "events");
    assert_eq!(&events[..tables[3].len()], tables[3].as_slice());
    assert_eq!(events.len(), tables[3].len() + 1);
    for json in &events {
        assert_eq!(
            StoredRunEvent::decode(json)
                .unwrap()
                .projected()
                .unwrap()
                .header()
                .contract_version,
            2
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(json).unwrap()["contract_version"],
            1
        );
    }
    assert_eq!(family(&dir.0), before);
    p.close().unwrap();
    assert_eq!(family(&dir.0), before);
    // Reopening the original still sees version1 and unfinished work; its own
    // real recovery has not already been executed by the rehearsal.
    let mut original = SqliteStore::open(&dir.0).unwrap();
    original.bootstrap(workspace_seed()).unwrap();
    let recovered = original.startup_recover(LATER, &mut Ids).unwrap();
    assert_eq!(recovered.interrupted_runs[0].run_id, run.run_id);
}

#[test]
fn hot_journal_rehearsal_recovers_only_copy_and_leaves_original_family_byte_exact() {
    let dir = TemporaryDirectory::new();
    let (mut store, w, s) = fresh(&dir.0);
    queued(&mut store, &w, &s);
    drop(store);
    leave_hot_journal(&dir);
    let before = family(&dir.0);
    let p = LegacyUpgradePreview::inspect(&dir.0, LATER).unwrap();
    assert_eq!(p.interrupted_runs(), 1);
    assert_eq!(family(&dir.0), before);
    p.close().unwrap();
    assert_eq!(family(&dir.0), before);
    assert!(preview_children(&dir.0).is_empty());
}

#[test]
fn corrupt_receipt_rejects_copy_before_migration_and_preserves_original_hot_journal() {
    let dir = TemporaryDirectory::new();
    let (mut store, w, s) = fresh(&dir.0);
    queued(&mut store, &w, &s);
    store
        .conn
        .execute(
            "UPDATE receipts SET result_json='{}' WHERE command_name='start_fake_run'",
            [],
        )
        .unwrap();
    drop(store);
    leave_hot_journal(&dir);
    let before = family(&dir.0);
    assert!(LegacyUpgradePreview::inspect(&dir.0, LATER).is_err());
    assert_eq!(family(&dir.0), before);
    assert!(preview_children(&dir.0).is_empty());
}

#[test]
fn unsupported_schema_is_preserved_without_automatic_replacement() {
    let dir = TemporaryDirectory::new();
    let (store, _, _) = fresh(&dir.0);
    store.conn.pragma_update(None, "user_version", 99).unwrap();
    drop(store);
    let before = family(&dir.0);
    assert_eq!(
        LegacyUpgradePreview::inspect(&dir.0, LATER)
            .err()
            .unwrap()
            .code,
        "unsupported_storage_version"
    );
    assert_eq!(family(&dir.0), before);
    assert!(preview_children(&dir.0).is_empty());
}

#[test]
fn migration_failure_rolls_back_schema_version_indices_and_legacy_bytes_on_fixture() {
    let dir = TemporaryDirectory::new();
    let (mut store, w, s) = fresh(&dir.0);
    queued(&mut store, &w, &s);
    let schema = schema_signature(&store.conn).unwrap();
    let receipts = rows(&store.conn, "receipts");
    let events = rows(&store.conn, "events");
    let failure = migrate_copy(&mut store.conn, |c| {
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        Err(unavailable())
    });
    assert!(failure.is_err());
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(schema_signature(&store.conn).unwrap(), schema);
    assert_eq!(rows(&store.conn, "receipts"), receipts);
    assert_eq!(rows(&store.conn, "events"), events);
    assert!(audit_database(&store.conn).is_ok());
}

#[test]
fn sql_profile_prompt_and_emergency_constraints_are_not_null_loopholes() {
    let dir = TemporaryDirectory::new();
    let (mut store, w, s) = fresh(&dir.0);
    migrate_copy(&mut store.conn, |_| Ok(())).unwrap();
    // SQL-only shape fixtures: deliberately not native admission or receipt-
    // content acceptance. Their '{}' JSON is never loaded through the audit.
    let insert = |provider: &str,
                  scenario: Option<&str>,
                  profile: Option<&str>,
                  prompt: &str,
                  order: u64| {
        store.conn.execute("INSERT INTO runs(run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,prompt,scenario,ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count,provider_id,profile_id) VALUES (?1,?2,?3,'queued',1,?4,'{}',?5,?6,1,0,0,0,0,0,?7,?8)",params![id(),w,s,order,prompt,scenario,provider,profile])
    };
    assert!(insert("codex", None, Some("codex_text_v1"), &"😀".repeat(256), 1).is_ok());
    assert!(insert(
        "codex",
        None,
        Some("codex_text_v1"),
        &("😀".repeat(256) + "x"),
        2
    )
    .is_err());
    assert!(insert("codex", None, None, "x", 2).is_err());
    assert!(insert("codex", Some("success"), Some("codex_text_v1"), "x", 2).is_err());
    assert!(insert("fake", None, None, "x", 2).is_err());
    assert!(insert("fake", Some("success"), Some("codex_text_v1"), "x", 2).is_err());
    assert!(insert("fake", Some("success"), None, &"a".repeat(65536), 2).is_ok());
    for command in ["cancel_run", "cancel_run_v2"] {
        assert!(store
            .conn
            .execute(
                "INSERT INTO receipts VALUES (?1,?2,?3,'{}','{}',1)",
                params![w, command, id()]
            )
            .is_ok());
    }
    assert!(store
        .conn
        .execute(
            "INSERT INTO receipts VALUES (?1,'start_codex_run_v2',?2,'{}','{}',1)",
            params![w, id()]
        )
        .is_err());
}

#[test]
fn v1_opener_refuses_upgraded_fixture_before_original_writes() {
    let dir = TemporaryDirectory::new();
    let (mut store, _, _) = fresh(&dir.0);
    migrate_copy(&mut store.conn, |_| Ok(())).unwrap();
    drop(store);
    let before = family(&dir.0);
    assert_eq!(
        SqliteStore::open(&dir.0).err().unwrap().code,
        "unsupported_storage_version"
    );
    assert_eq!(family(&dir.0), before);
}
