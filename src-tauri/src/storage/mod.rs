//! Native SQLite journal. This module performs no provider or UI work.
use crate::execution::ports::*;
use crate::execution::validation::*;
use crate::execution::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

pub mod admission_v2;
pub mod store_v2;
mod writer_v2;

/// Owner of an audited v2 journal connection. Gates readiness in ONE place:
/// the copy rehearsal is ready after construction; the fresh v2 store only
/// after its once-per-open startup audit and recovery. Not a public opener.
pub(crate) trait JournalConnection {
    fn journal_connection(&mut self) -> StoreResult<&mut Connection>;
}
mod audit_v2;
mod read_v2;
mod receipt_audit;
pub mod upgrade_preview;

const SCHEMA: &str = include_str!("schema.sql");
const STORAGE_VERSION: i64 = 1;

pub struct SqliteStore {
    // Field order keeps ownership until SQLite has closed, including on Windows.
    conn: Connection,
    _ownership: File,
    recovered: bool,
}

fn unavailable() -> Error {
    error(
        "storage_unavailable",
        "Execution storage is unavailable; existing data is preserved.",
    )
}
fn database_error(value: rusqlite::Error) -> Error {
    match value {
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            error(
                "storage_busy",
                "Execution storage is busy. Try again explicitly.",
            )
        }
        _ => unavailable(),
    }
}
fn require_store(condition: bool, code: &str, message: &str) -> StoreResult<()> {
    if condition {
        Ok(())
    } else {
        Err(error(code, message))
    }
}
fn encode<T: Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_string(value).map_err(|_| unavailable())
}
fn decode<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    serde_json::from_str(value).map_err(|_| unavailable())
}
fn checked_stored(result: StoreResult<()>) -> StoreResult<()> {
    result.map_err(|_| unavailable())
}
fn changed<T>(result: T, committed_events: Vec<RunEvent>) -> MutationCommit<T> {
    MutationCommit {
        result,
        committed_events,
        replayed: false,
    }
}

fn schema_signature(conn: &Connection) -> StoreResult<Vec<(String, String, String, String)>> {
    let mut statement = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY type, name"
    ).map_err(database_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(database_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(database_error)
}

/// Read-only inspection precedes journal-mode changes and schema writes.
fn inspect_database(conn: &Connection) -> StoreResult<bool> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(database_error)?;
    require_store(
        version == 0 || version == STORAGE_VERSION,
        "unsupported_storage_version",
        "Unsupported execution storage version; original data is preserved.",
    )?;
    let checks = {
        let mut statement = conn
            .prepare("PRAGMA integrity_check")
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(database_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?
    };
    require_store(
        checks == vec!["ok".to_owned()],
        "storage_unavailable",
        "Execution storage failed its integrity check; original data is preserved.",
    )?;
    if version == 0 {
        let objects: u64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))
            .map_err(database_error)?;
        let pages: u64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .map_err(database_error)?;
        let free: u64 = conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .map_err(database_error)?;
        let application: i64 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            objects == 0 && pages <= 1 && free == 0 && application == 0,
            "unsupported_storage_version",
            "Unversioned nonempty storage is preserved and cannot be initialized.",
        )?;
        return Ok(true);
    }
    // Comparing against an isolated database also verifies constraints, indexes,
    // foreign keys and unexpected triggers without modifying the supplied file.
    let expected = Connection::open_in_memory().map_err(database_error)?;
    expected.execute_batch(SCHEMA).map_err(database_error)?;
    require_store(
        schema_signature(conn)? == schema_signature(&expected)?,
        "unsupported_storage_version",
        "Execution storage schema is not recognized; original data is preserved.",
    )?;
    let foreign_key_failure: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(database_error)?;
    require_store(
        foreign_key_failure.is_none(),
        "storage_unavailable",
        "Execution storage has invalid relationships; original data is preserved.",
    )?;
    checked_stored(audit_database(conn))?;
    Ok(false)
}

fn stored_file_length(path: &Path) -> StoreResult<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata.len())),
        Err(failure) if failure.kind() == io::ErrorKind::NotFound => Ok(None),
        _ => Err(unavailable()),
    }
}

fn require_delete_family(directory: &Path) -> StoreResult<()> {
    // Never inspect an incomplete WAL image or let a read-only probe create SHM.
    for name in ["execution.sqlite3-wal", "execution.sqlite3-shm"] {
        require_store(
            stored_file_length(&directory.join(name))?.is_none(),
            "storage_unavailable",
            "Unexpected WAL storage is preserved and cannot be opened.",
        )?;
    }
    let database = directory.join("execution.sqlite3");
    if stored_file_length(&database)?.is_some_and(|length| length >= 20) {
        let mut header = [0u8; 20];
        File::open(&database)
            .and_then(|mut file| file.read_exact(&mut header))
            .map_err(|_| unavailable())?;
        require_store(
            header[18] != 2 && header[19] != 2,
            "storage_unavailable",
            "Unexpected WAL storage is preserved and cannot be opened.",
        )?;
    }
    Ok(())
}

fn preflight_database(directory: &Path) -> StoreResult<bool> {
    require_delete_family(directory)?;
    let database = directory.join("execution.sqlite3");
    let journal = directory.join("execution.sqlite3-journal");
    let database_length = stored_file_length(&database)?;
    let journal_length = stored_file_length(&journal)?;
    if database_length.is_none() || database_length == Some(0) {
        require_store(
            journal_length.is_none(),
            "storage_unavailable",
            "Storage sidecars without a database are preserved.",
        )?;
        return Ok(true);
    }
    if journal_length.is_none() {
        let probe = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(database_error)?;
        probe.busy_timeout(Duration::ZERO).map_err(database_error)?;
        return inspect_database(&probe);
    }

    // A super-journal names an attached file family outside this single-DB store.
    // Reject it rather than let a copied journal consult an uncopied dependency.
    if journal_length.is_some_and(|length| length >= 8) {
        let mut file = File::open(&journal).map_err(|_| unavailable())?;
        file.seek(SeekFrom::End(-8)).map_err(|_| unavailable())?;
        let mut footer = [0u8; 8];
        file.read_exact(&mut footer).map_err(|_| unavailable())?;
        require_store(
            footer != *b"\xd9\xd5\x05\xf9\x20\xa1\x63\xd7",
            "storage_unavailable",
            "Attached-database rollback journals are preserved and cannot be opened.",
        )?;
    }

    // Ownership is already held. Only this successfully created native child
    // directory is eligible for cleanup; an existing path is never adopted.
    let validation_directory =
        directory.join(format!(".execution-validation-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&validation_directory).map_err(|_| unavailable())?;
    let result = (|| -> StoreResult<bool> {
        let copy = validation_directory.join("execution.sqlite3");
        fs::copy(&database, &copy).map_err(|_| unavailable())?;
        fs::copy(
            &journal,
            validation_directory.join("execution.sqlite3-journal"),
        )
        .map_err(|_| unavailable())?;
        let probe = Connection::open_with_flags(
            &copy,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(database_error)?;
        probe.busy_timeout(Duration::ZERO).map_err(database_error)?;
        // SQLite may recover only this owned copy until all checks succeed.
        let initialize = inspect_database(&probe)?;
        require_delete_family(&validation_directory)?;
        Ok(initialize)
    })();
    // The closure has dropped every SQLite handle before directory cleanup.
    let cleanup = fs::remove_dir_all(&validation_directory).map_err(|_| unavailable());
    match result {
        Ok(initialize) => {
            cleanup?;
            Ok(initialize)
        }
        Err(failure) => Err(failure),
    }
}

fn decode_stored_event(row: &rusqlite::Row<'_>, column: usize) -> StoreResult<RunEvent> {
    // Check borrowed SQLite bytes before deserialization allocates any strings.
    let bytes = match row.get_ref(column).map_err(database_error)? {
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= EVENT_BYTES => bytes,
        _ => return Err(unavailable()),
    };
    let event: RunEvent = serde_json::from_slice(bytes).map_err(|_| unavailable())?;
    let limit = if matches!(&event, RunEvent::TextDelta { .. }) {
        EVENT_BYTES
    } else {
        NONTEXT_EVENT_BYTES
    };
    require_store(
        bytes.len() <= limit,
        "storage_unavailable",
        "Stored event data exceeds its byte budget.",
    )?;
    Ok(event)
}

impl SqliteStore {
    pub fn open(directory: &Path) -> StoreResult<Self> {
        fs::create_dir_all(directory).map_err(|_| unavailable())?;
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("execution.lock"))
            .map_err(|_| unavailable())?;
        fs2::FileExt::try_lock_exclusive(&ownership).map_err(|failure| {
            if failure.kind() == io::ErrorKind::WouldBlock
                || failure.raw_os_error() == fs2::lock_contended_error().raw_os_error()
            {
                error("storage_busy", "Another process owns execution storage.")
            } else {
                unavailable()
            }
        })?;
        let initialize = preflight_database(directory)?;
        let database = directory.join("execution.sqlite3");
        let mut conn = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(database_error)?;
        conn.busy_timeout(Duration::ZERO).map_err(database_error)?;
        // This first original read may now recover a validated rollback image.
        // Exclusive ownership keeps it identical to the preflight source family.
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            version == if initialize { 0 } else { STORAGE_VERSION },
            "storage_unavailable",
            "Execution storage changed after validation.",
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(database_error)?;
        let enabled: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            enabled == 1,
            "storage_unavailable",
            "Foreign-key enforcement could not be enabled.",
        )?;
        let journal: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            journal.eq_ignore_ascii_case("delete"),
            "storage_unavailable",
            "Durable execution journaling could not be enabled.",
        )?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(database_error)?;
        if initialize {
            let transaction = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            transaction.execute_batch(SCHEMA).map_err(database_error)?;
            transaction
                .pragma_update(None, "user_version", STORAGE_VERSION)
                .map_err(database_error)?;
            transaction.commit().map_err(database_error)?;
        }
        Ok(Self {
            conn,
            _ownership: ownership,
            recovered: false,
        })
    }

    fn ready(&self) -> StoreResult<()> {
        require_store(
            self.recovered,
            "storage_unavailable",
            "Startup recovery must finish before execution admission.",
        )
    }
}

fn empty_layout(workspace_id: &str) -> Layout {
    Layout {
        layout_version: 1,
        workspace_id: workspace_id.into(),
        revision: 0,
        theme: ThemeName::Dark,
        active_module: ModuleName::Agents,
        sidebar_width: 300.0,
        panels: Vec::new(),
        containers: Vec::new(),
    }
}

struct WorkspaceRow {
    record: WorkspaceSnapshotRecord,
    layout_json: String,
    revision: u64,
    last_enqueue: u64,
}
fn workspace(conn: &Connection, selected: Option<&str>) -> StoreResult<WorkspaceRow> {
    let raw: Option<(String, String, String, u64, u64)> = conn.query_row(
        "SELECT workspace_id, record_json, layout_json, layout_revision, last_enqueue FROM workspaces WHERE singleton=1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
    ).optional().map_err(database_error)?;
    let (id, json, layout_json, revision, last_enqueue) =
        raw.ok_or_else(|| error("workspace_not_found", "Managed workspace was not found."))?;
    if selected.is_some_and(|selected| selected != id) {
        return Err(error(
            "workspace_not_found",
            "Managed workspace was not found.",
        ));
    }
    let record: WorkspaceSnapshotRecord = decode(&json)?;
    require_store(
        record.workspace_id == id,
        "storage_unavailable",
        "Stored workspace identity is inconsistent.",
    )?;
    checked_stored(validate_counter(revision))?;
    checked_stored(validate_counter(last_enqueue))?;
    checked_stored(validate_workspace_snapshot(&WorkspaceSnapshot {
        contract_version: 1,
        workspace: record.clone(),
        sessions: Vec::new(),
        runs: Vec::new(),
        layout: Some(empty_layout(&id)),
        layout_warning: None,
        execution_available: false,
    }))?;
    Ok(WorkspaceRow {
        record,
        layout_json,
        revision,
        last_enqueue,
    })
}

fn sessions(conn: &Connection, workspace_id: &str) -> StoreResult<Vec<SessionSnapshot>> {
    let mut statement = conn.prepare(
        "SELECT session_id, snapshot_json FROM sessions WHERE workspace_id=?1 ORDER BY session_id"
    ).map_err(database_error)?;
    let rows = statement
        .query_map([workspace_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(database_error)?;
    let mut output = Vec::new();
    for row in rows {
        let (id, json) = row.map_err(database_error)?;
        let value: SessionSnapshot = decode(&json)?;
        checked_stored(validate_session(&value))?;
        require_store(
            value.workspace_id == workspace_id && value.session_id == id,
            "storage_unavailable",
            "Stored session identity is inconsistent.",
        )?;
        output.push(value);
    }
    require_store(
        output.len() <= WORKSPACE_SESSIONS,
        "storage_unavailable",
        "Stored session capacity is inconsistent.",
    )?;
    Ok(output)
}
fn owned_session(
    conn: &Connection,
    workspace_id: &str,
    session_id: &str,
) -> StoreResult<SessionSnapshot> {
    sessions(conn, workspace_id)?
        .into_iter()
        .find(|value| value.session_id == session_id)
        .ok_or_else(|| {
            error(
                "session_not_found",
                "Session was not found in the selected workspace.",
            )
        })
}

fn stored_layout(
    row: &WorkspaceRow,
    sessions: &[SessionSnapshot],
) -> (Option<Layout>, Option<Error>) {
    let invalid = || {
        error(
            "layout_invalid",
            "Stored layout is invalid; original data is preserved and saving is disabled.",
        )
    };
    let value: serde_json::Value = match serde_json::from_str(&row.layout_json) {
        Ok(value) => value,
        Err(_) => return (None, Some(invalid())),
    };
    if value
        .get("layout_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version != 1)
    {
        return (None, Some(error("unsupported_layout_version",
            "Stored layout version is unsupported; original data is preserved and saving is disabled.")));
    }
    let layout: Layout = match serde_json::from_value(value) {
        Ok(layout) => layout,
        Err(_) => return (None, Some(invalid())),
    };
    if layout.revision != row.revision
        || validate_layout(&layout, &row.record.workspace_id, sessions).is_err()
    {
        return (None, Some(invalid()));
    }
    (Some(layout), None)
}

struct DurableRun {
    snapshot: RunSnapshot,
    prompt: String,
    scenario: FakeScenario,
    ordinary: u64,
    reserved: u64,
    text: u64,
    mask: u64,
    cancellations: u64,
    terminals: u64,
}
impl DurableRun {
    fn queued(&self) -> QueuedRun {
        QueuedRun {
            snapshot: self.snapshot.clone(),
            prompt: self.prompt.clone(),
            scenario: self.scenario,
        }
    }
}
fn load_run(conn: &Connection, workspace_id: &str, run_id: &str) -> StoreResult<DurableRun> {
    let raw = conn.query_row(
        "SELECT r.session_id, r.state, r.last_seq, r.enqueue_order, r.snapshot_json, r.prompt, r.scenario,
                r.ordinary_count, r.reserved_count, r.text_bytes, r.truncation_mask, r.cancel_count, r.terminal_count
         FROM runs r JOIN sessions s ON s.workspace_id=r.workspace_id AND s.session_id=r.session_id
         WHERE r.workspace_id=?1 AND r.run_id=?2",
        params![workspace_id, run_id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?, row.get::<_, u64>(3)?,
            row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?,
            row.get::<_, u64>(7)?, row.get::<_, u64>(8)?, row.get::<_, u64>(9)?,
            row.get::<_, u64>(10)?, row.get::<_, u64>(11)?, row.get::<_, u64>(12)?
        ))
    ).optional().map_err(database_error)?
        .ok_or_else(|| error("run_not_found", "Run was not found in the selected workspace."))?;
    let snapshot: RunSnapshot = decode(&raw.4)?;
    checked_stored(validate_snapshot(&snapshot))?;
    let scenario: FakeScenario =
        serde_json::from_value(serde_json::Value::String(raw.6)).map_err(|_| unavailable())?;
    checked_stored(validate_start_fake_run(&StartFakeRunRequest {
        contract_version: 1,
        workspace_id: workspace_id.into(),
        session_id: raw.0.clone(),
        request_id: run_id.into(),
        provider_id: snapshot.provider_id,
        prompt: raw.5.clone(),
        scenario,
    }))?;
    require_store(
        snapshot.workspace_id == workspace_id
            && snapshot.run_id == run_id
            && snapshot.session_id == raw.0
            && snapshot.state.as_str() == raw.1
            && snapshot.last_seq == raw.2
            && snapshot.enqueue_order == raw.3,
        "storage_unavailable",
        "Stored run identity or snapshot is inconsistent.",
    )?;
    let value = DurableRun {
        snapshot,
        prompt: raw.5,
        scenario,
        ordinary: raw.7,
        reserved: raw.8,
        text: raw.9,
        mask: raw.10,
        cancellations: raw.11,
        terminals: raw.12,
    };
    require_store(
        value.ordinary > 0
            && value.ordinary <= ORDINARY_EVENTS as u64
            && value.reserved <= RESERVED_EVENTS as u64
            && value.text <= PERSISTED_TEXT_BYTES as u64
            && value.mask <= 7
            && value.cancellations <= 1
            && value.terminals <= 1
            && value.ordinary + value.reserved == value.snapshot.last_seq
            && value.reserved
                == value.mask.count_ones() as u64 + value.cancellations + value.terminals
            && value.terminals == u64::from(is_terminal(value.snapshot.state))
            && (value.snapshot.state != RunState::CancelRequested || value.cancellations == 1)
            && (value.mask == 0
                || matches!(
                    value.snapshot.state,
                    RunState::CancelRequested
                        | RunState::Cancelled
                        | RunState::Failed
                        | RunState::Interrupted
                )),
        "storage_unavailable",
        "Stored run counters are inconsistent.",
    )?;
    let (count, first, last): (u64, u64, u64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MIN(seq),0), COALESCE(MAX(seq),0) FROM events WHERE workspace_id=?1 AND run_id=?2",
        params![workspace_id, run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).map_err(database_error)?;
    require_store(
        count == value.snapshot.last_seq && first == 1 && last == value.snapshot.last_seq,
        "event_history_unavailable",
        "The committed event history is incomplete.",
    )?;
    Ok(value)
}

fn run_ids(
    conn: &Connection,
    workspace_id: &str,
    nonterminal_only: bool,
) -> StoreResult<Vec<String>> {
    let sql = if nonterminal_only {
        "SELECT run_id FROM runs WHERE workspace_id=?1 AND state NOT IN ('succeeded','failed','cancelled','interrupted') ORDER BY enqueue_order"
    } else {
        "SELECT run_id FROM runs WHERE workspace_id=?1 ORDER BY enqueue_order"
    };
    let mut statement = conn.prepare(sql).map_err(database_error)?;
    let rows = statement
        .query_map([workspace_id], |row| row.get::<_, String>(0))
        .map_err(database_error)?;
    let ids = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(database_error)?;
    require_store(
        ids.len() <= WORKSPACE_RUNS,
        "storage_unavailable",
        "Stored run capacity is inconsistent.",
    )?;
    Ok(ids)
}
fn read_snapshot(
    conn: &Connection,
    workspace_id: Option<&str>,
    available: bool,
) -> StoreResult<WorkspaceSnapshot> {
    let row = workspace(conn, workspace_id)?;
    let id = &row.record.workspace_id;
    let session_values = sessions(conn, id)?;
    let mut runs = Vec::new();
    for run_id in run_ids(conn, id, false)? {
        let run = load_run(conn, id, &run_id)?.snapshot;
        require_store(
            run.enqueue_order <= row.last_enqueue,
            "storage_unavailable",
            "Stored queue ordering is inconsistent.",
        )?;
        runs.push(run);
    }
    let (layout, layout_warning) = stored_layout(&row, &session_values);
    let value = WorkspaceSnapshot {
        contract_version: 1,
        workspace: row.record,
        sessions: session_values,
        runs,
        layout,
        layout_warning,
        execution_available: available,
    };
    checked_stored(validate_workspace_snapshot(&value))?;
    Ok(value)
}

/// Startup-only audit, before the original database can receive any writes.
fn audit_database(conn: &Connection) -> StoreResult<()> {
    let (workspace_count, session_count, run_count, ordinary_receipts, emergency_receipts): (
        u64,
        u64,
        u64,
        u64,
        u64,
    ) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM workspaces),
                (SELECT COUNT(*) FROM sessions),
                (SELECT COUNT(*) FROM runs),
                (SELECT COUNT(*) FROM receipts WHERE emergency=0),
                (SELECT COUNT(*) FROM receipts WHERE emergency=1)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(database_error)?;
    require_store(
        workspace_count <= 1
            && session_count <= WORKSPACE_SESSIONS as u64
            && run_count <= WORKSPACE_RUNS as u64
            && ordinary_receipts <= COMMAND_RECEIPTS as u64
            && emergency_receipts <= CANCEL_RECEIPTS as u64,
        "storage_unavailable",
        "Stored workspace capacity is inconsistent.",
    )?;
    // inspect_database has already checked all foreign keys, so no dependent
    // records can exist when the supported schema has not yet been bootstrapped.
    if workspace_count == 0 {
        return Ok(());
    }
    let snapshot = read_snapshot(conn, None, false)?;
    let mut receipts = receipt_audit::audit(conn, &snapshot)?;
    for expected in &snapshot.runs {
        let run = load_run(conn, &snapshot.workspace.workspace_id, &expected.run_id)?;
        let mut prefixes = receipts.remove(&expected.run_id).unwrap_or_default();
        audit_history(conn, &run, &mut prefixes)?;
        require_store(
            prefixes.is_empty(),
            "storage_unavailable",
            "Stored receipt references an unavailable event prefix.",
        )?;
    }
    require_store(
        receipts.is_empty(),
        "storage_unavailable",
        "Stored receipt references an unknown run.",
    )
}

fn audit_history(
    conn: &Connection,
    run: &DurableRun,
    receipts: &mut receipt_audit::PrefixReceipts,
) -> StoreResult<()> {
    let mut statement = conn
        .prepare(
            "SELECT event_id,session_id,seq,event_json FROM events
         WHERE workspace_id=?1 AND run_id=?2 ORDER BY seq",
        )
        .map_err(database_error)?;
    let mut rows = statement
        .query(params![run.snapshot.workspace_id, run.snapshot.run_id])
        .map_err(database_error)?;
    let mut derived: Option<RunSnapshot> = None;
    let (mut ordinary, mut reserved, mut text, mut mask, mut cancellations, mut terminals) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    while let Some(row) = rows.next().map_err(database_error)? {
        let event = decode_stored_event(row, 3)?;
        checked_stored(validate_event(&event))?;
        let header = event.header();
        let seq: u64 = row.get(2).map_err(database_error)?;
        require_store(
            matches!(row.get_ref(0).map_err(database_error)?,
                rusqlite::types::ValueRef::Text(value) if value == header.event_id.as_bytes())
                && matches!(row.get_ref(1).map_err(database_error)?,
                    rusqlite::types::ValueRef::Text(value) if value == header.session_id.as_bytes())
                && header.seq == seq
                && header.workspace_id == run.snapshot.workspace_id
                && header.session_id == run.snapshot.session_id
                && header.run_id == run.snapshot.run_id
                && header.provider_id == run.snapshot.provider_id,
            "storage_unavailable",
            "Committed event identities are inconsistent.",
        )?;
        derived = Some(match derived.as_ref() {
            Some(current) => apply_event(current, &event).map_err(|_| unavailable())?,
            None => {
                require_store(
                    matches!(&event, RunEvent::RunCreated { .. }),
                    "storage_unavailable",
                    "Committed history must begin with queued creation.",
                )?;
                let mut initial = run.snapshot.clone();
                initial.state = RunState::Queued;
                initial.last_seq = 1;
                initial.created_at = header.occurred_at.clone();
                initial.updated_at = header.occurred_at.clone();
                initial.usage = Usage {
                    input_tokens: None,
                    output_tokens: None,
                    estimated: false,
                };
                initial.terminal_error = None;
                initial
            }
        });
        receipt_audit::prefix(receipts, derived.as_ref().ok_or_else(unavailable)?, &event)?;
        if is_reserved_event(&event) {
            reserved += 1;
            match &event {
                RunEvent::OutputTruncated { payload, .. } => {
                    let bit = truncation_bit(payload.scope);
                    require_store(
                        mask & bit == 0,
                        "storage_unavailable",
                        "Committed history repeats a truncation scope.",
                    )?;
                    mask |= bit;
                }
                RunEvent::RunRecovered { .. } => terminals += 1,
                RunEvent::RunStateChanged { payload, .. }
                    if payload.to == RunState::CancelRequested =>
                {
                    cancellations += 1;
                }
                RunEvent::RunStateChanged { payload, .. } if is_terminal(payload.to) => {
                    require_store(
                        payload.to != RunState::Succeeded || mask == 0,
                        "storage_unavailable",
                        "A truncated committed history cannot succeed.",
                    )?;
                    terminals += 1;
                }
                _ => return Err(unavailable()),
            }
        } else {
            require_store(
                mask == 0,
                "storage_unavailable",
                "Committed ordinary events follow truncation.",
            )?;
            ordinary += 1;
            if let RunEvent::TextDelta { payload, .. } = &event {
                text += payload.text.len() as u64;
            }
        }
        require_store(
            ordinary <= ORDINARY_EVENTS as u64
                && reserved <= RESERVED_EVENTS as u64
                && text <= PERSISTED_TEXT_BYTES as u64
                && cancellations <= 1
                && terminals <= 1,
            "storage_unavailable",
            "Committed history exceeds its cumulative budgets.",
        )?;
    }
    require_store(
        derived.as_ref() == Some(&run.snapshot)
            && ordinary == run.ordinary
            && reserved == run.reserved
            && text == run.text
            && mask == run.mask
            && cancellations == run.cancellations
            && terminals == run.terminals,
        "storage_unavailable",
        "Committed history disagrees with its stored snapshot or counters.",
    )
}

fn replay<T: DeserializeOwned>(
    conn: &Connection,
    workspace_id: &str,
    command: CommandName,
    request_id: &str,
    arguments: &str,
) -> StoreResult<Option<MutationCommit<T>>> {
    let found: Option<(String, String)> = conn.query_row(
        "SELECT arguments_json, result_json FROM receipts WHERE workspace_id=?1 AND command_name=?2 AND request_id=?3",
        params![workspace_id, command.as_str(), request_id], |row| Ok((row.get(0)?, row.get(1)?))
    ).optional().map_err(database_error)?;
    match found {
        None => Ok(None),
        Some((original, result)) => {
            require_store(
                original == arguments,
                "request_id_conflict",
                "Request identity was already used with different arguments.",
            )?;
            Ok(Some(MutationCommit {
                result: decode(&result)?,
                committed_events: Vec::new(),
                replayed: true,
            }))
        }
    }
}
fn receipt_class(
    conn: &Connection,
    workspace_id: &str,
    first_cancellation: bool,
) -> StoreResult<i64> {
    let (ordinary, emergency): (u64, u64) = conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN emergency=0 THEN 1 ELSE 0 END),0), COALESCE(SUM(emergency),0)
         FROM receipts WHERE workspace_id=?1", [workspace_id], |row| Ok((row.get(0)?, row.get(1)?))
    ).map_err(database_error)?;
    require_store(
        ordinary <= COMMAND_RECEIPTS as u64 && emergency <= CANCEL_RECEIPTS as u64,
        "storage_unavailable",
        "Stored receipt capacity is inconsistent.",
    )?;
    if ordinary < COMMAND_RECEIPTS as u64 {
        return Ok(0);
    }
    if first_cancellation && emergency < CANCEL_RECEIPTS as u64 {
        return Ok(1);
    }
    Err(error(
        "capacity_exceeded",
        "Durable command receipt capacity has been reached.",
    ))
}
fn receipt<T: Serialize>(
    conn: &Connection,
    workspace_id: &str,
    command: CommandName,
    request_id: &str,
    arguments: &str,
    result: &T,
    emergency: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO receipts(workspace_id,command_name,request_id,arguments_json,result_json,emergency) VALUES (?1,?2,?3,?4,?5,?6)",
        params![workspace_id, command.as_str(), request_id, arguments, encode(result)?, emergency]
    ).map_err(database_error)?;
    Ok(())
}
fn event_identity_available(conn: &Connection, event: &RunEvent) -> StoreResult<()> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM events WHERE event_id=?1",
            [event.header().event_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(database_error)?;
    require_store(
        found.is_none(),
        "state_conflict",
        "Event identity is already committed.",
    )
}
fn insert_event(conn: &Connection, event: &RunEvent) -> StoreResult<()> {
    let header = event.header();
    conn.execute("INSERT INTO events(event_id,workspace_id,session_id,run_id,seq,event_json) VALUES (?1,?2,?3,?4,?5,?6)",
        params![header.event_id, header.workspace_id, header.session_id, header.run_id, header.seq, encode(event)?])
        .map_err(database_error)?;
    Ok(())
}
fn truncation_bit(scope: TruncationScope) -> u64 {
    match scope {
        TruncationScope::PendingBytes => 1,
        TruncationScope::PersistedTextBytes => 2,
        TruncationScope::EventCount => 4,
    }
}

/// Called only within a writer transaction; every validation precedes its writes.
fn append(conn: &Connection, batch: &AppendBatch) -> StoreResult<RunSnapshot> {
    let current = load_run(conn, &batch.workspace_id, &batch.run_id)?;
    require_store(
        batch.expected_last_seq == current.snapshot.last_seq
            && batch.expected_state == current.snapshot.state,
        "state_conflict",
        "Run state or sequence changed before append.",
    )?;
    validate_append(&current.snapshot, batch)?;
    // A terminal final snapshot cannot hide an intermediate claim of the slot.
    // Validate every state in the atomic batch, not just the final materialization.
    let claims_active_slot = (current.snapshot.state != RunState::Queued
        && !is_terminal(current.snapshot.state))
        || batch.events.iter().any(|event| {
            matches!(event,
            RunEvent::RunStateChanged { payload, .. }
                if payload.to != RunState::Queued && !is_terminal(payload.to))
        });
    if claims_active_slot {
        let owners: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE workspace_id=?1 AND run_id<>?2
             AND state NOT IN ('queued','succeeded','failed','cancelled','interrupted')",
                params![batch.workspace_id, batch.run_id],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        require_store(
            owners == 0,
            "state_conflict",
            "Another run owns the execution slot.",
        )?;
    }
    if current.snapshot.state == RunState::Queued
        && batch.events.iter().any(|event| {
            matches!(event, RunEvent::RunStateChanged { payload, .. }
            if payload.from == RunState::Queued && payload.to == RunState::Starting)
        })
    {
        let earlier: u64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE workspace_id=?1 AND state='queued' AND enqueue_order<?2",
            params![batch.workspace_id, current.snapshot.enqueue_order], |row| row.get(0)
        ).map_err(database_error)?;
        require_store(
            earlier == 0,
            "state_conflict",
            "An earlier queued run must be admitted first.",
        )?;
    }
    let mut ordinary = current.ordinary;
    let mut reserved = current.reserved;
    let mut text = current.text;
    let mut mask = current.mask;
    let mut cancellations = current.cancellations;
    let mut terminals = current.terminals;
    for event in &batch.events {
        event_identity_available(conn, event)?;
        if is_reserved_event(event) {
            reserved += 1;
            match event {
                RunEvent::OutputTruncated { payload, .. } => {
                    let bit = truncation_bit(payload.scope);
                    require_store(
                        mask & bit == 0,
                        "state_conflict",
                        "Truncation scope was already committed.",
                    )?;
                    mask |= bit;
                }
                RunEvent::RunRecovered { .. } => terminals += 1,
                RunEvent::RunStateChanged { payload, .. }
                    if payload.to == RunState::CancelRequested =>
                {
                    cancellations += 1
                }
                RunEvent::RunStateChanged { payload, .. } if is_terminal(payload.to) => {
                    require_store(
                        payload.to != RunState::Succeeded || mask == 0,
                        "state_conflict",
                        "A truncated run cannot succeed.",
                    )?;
                    terminals += 1;
                }
                _ => return Err(unavailable()),
            }
        } else {
            require_store(
                mask == 0,
                "state_conflict",
                "Ordinary events cannot follow committed truncation.",
            )?;
            ordinary += 1;
            if let RunEvent::TextDelta { payload, .. } = event {
                text += payload.text.len() as u64;
            }
        }
        require_store(
            ordinary <= ORDINARY_EVENTS as u64 && text <= PERSISTED_TEXT_BYTES as u64,
            "capacity_exceeded",
            "Run output requires an atomic truncation and stop decision.",
        )?;
        require_store(
            reserved <= RESERVED_EVENTS as u64 && cancellations <= 1 && terminals <= 1,
            "storage_unavailable",
            "Run control-event capacity is inconsistent.",
        )?;
    }
    for event in &batch.events {
        insert_event(conn, event)?;
    }
    let updated = conn.execute(
        "UPDATE runs SET state=?1,last_seq=?2,snapshot_json=?3,ordinary_count=?4,reserved_count=?5,
            text_bytes=?6,truncation_mask=?7,cancel_count=?8,terminal_count=?9
         WHERE workspace_id=?10 AND run_id=?11 AND last_seq=?12 AND state=?13",
        params![batch.next_snapshot.state.as_str(), batch.next_snapshot.last_seq, encode(&batch.next_snapshot)?,
            ordinary, reserved, text, mask, cancellations, terminals, batch.workspace_id, batch.run_id,
            current.snapshot.last_seq, current.snapshot.state.as_str()]
    ).map_err(database_error)?;
    require_store(
        updated == 1,
        "state_conflict",
        "Run changed before its snapshot was committed.",
    )?;
    Ok(batch.next_snapshot.clone())
}

impl JournalStore for SqliteStore {
    fn bootstrap(&mut self, seed: WorkspaceSeed) -> StoreResult<WorkspaceSnapshot> {
        let available = self.recovered;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let count: u64 = transaction
            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
            .map_err(database_error)?;
        if count == 0 {
            require_store(
                !available,
                "storage_unavailable",
                "Managed workspace disappeared after initialization.",
            )?;
            validate_id(&seed.workspace_id)?;
            validate_timestamp(&seed.created_at)?;
            let record = WorkspaceSnapshotRecord {
                workspace_id: seed.workspace_id.clone(),
                label: "Managed simulated workspace".into(),
                created_at: seed.created_at,
            };
            let layout = empty_layout(&seed.workspace_id);
            validate_layout(&layout, &seed.workspace_id, &[])?;
            validate_workspace_snapshot(&WorkspaceSnapshot {
                contract_version: 1,
                workspace: record.clone(),
                sessions: Vec::new(),
                runs: Vec::new(),
                layout: Some(layout.clone()),
                layout_warning: None,
                execution_available: false,
            })?;
            transaction.execute(
                "INSERT INTO workspaces(workspace_id,singleton,record_json,layout_json,layout_revision,last_enqueue) VALUES (?1,1,?2,?3,0,0)",
                params![seed.workspace_id, encode(&record)?, encode(&layout)?]
            ).map_err(database_error)?;
        }
        let result = read_snapshot(&transaction, None, available)?;
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn startup_recover(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> StoreResult<RecoveryReport> {
        if self.recovered {
            return Ok(RecoveryReport::default());
        }
        validate_timestamp(now)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let before = read_snapshot(&transaction, None, false)?;
        let workspace_id = before.workspace.workspace_id;
        let mut report = RecoveryReport::default();
        for run_id in run_ids(&transaction, &workspace_id, true)? {
            let current = load_run(&transaction, &workspace_id, &run_id)?.snapshot;
            let previous_state: ActiveState =
                serde_json::from_value(serde_json::Value::String(current.state.as_str().into()))
                    .map_err(|_| unavailable())?;
            let event = RunEvent::from_parts(
                EventHeader {
                    contract_version: 1,
                    event_id: ids.next_id(),
                    workspace_id: workspace_id.clone(),
                    session_id: current.session_id.clone(),
                    run_id: run_id.clone(),
                    provider_id: current.provider_id,
                    source: EventSource::System,
                    seq: current.last_seq + 1,
                    occurred_at: now.into(),
                },
                EventPayload::RunRecovered(RunRecoveredPayload {
                    previous_state,
                    state: "interrupted".into(),
                }),
            );
            let next_snapshot = apply_event(&current, &event)?;
            let batch = AppendBatch {
                workspace_id: workspace_id.clone(),
                run_id,
                expected_last_seq: current.last_seq,
                expected_state: current.state,
                events: vec![event.clone()],
                next_snapshot,
            };
            report.interrupted_runs.push(append(&transaction, &batch)?);
            report.committed_events.push(event);
        }
        transaction.commit().map_err(database_error)?;
        self.recovered = true;
        Ok(report)
    }

    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
        seed: SessionSeed,
    ) -> StoreResult<MutationCommit<SessionSnapshot>> {
        self.ready()?;
        validate_create_session(request)?;
        let arguments = canonical_arguments(request)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(&request.workspace_id))?;
        if let Some(result) = replay::<SessionSnapshot>(
            &transaction,
            &request.workspace_id,
            CommandName::CreateSession,
            &request.request_id,
            &arguments,
        )? {
            checked_stored(validate_session(&result.result))?;
            require_store(
                result.result.workspace_id == request.workspace_id,
                "storage_unavailable",
                "Stored receipt ownership is inconsistent.",
            )?;
            return Ok(result);
        }
        let class = receipt_class(&transaction, &request.workspace_id, false)?;
        require_store(
            sessions(&transaction, &request.workspace_id)?.len() < WORKSPACE_SESSIONS,
            "capacity_exceeded",
            "Workspace session capacity has been reached.",
        )?;
        let result = SessionSnapshot {
            contract_version: 1,
            workspace_id: request.workspace_id.clone(),
            session_id: seed.session_id,
            provider_id: ProviderId::Fake,
            label: request.label.clone(),
            created_at: seed.created_at,
        };
        validate_session(&result)?;
        let collision: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM sessions WHERE session_id=?1",
                [&result.session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?;
        require_store(
            collision.is_none(),
            "state_conflict",
            "Session identity is already committed.",
        )?;
        transaction
            .execute(
                "INSERT INTO sessions(session_id,workspace_id,snapshot_json) VALUES (?1,?2,?3)",
                params![result.session_id, result.workspace_id, encode(&result)?],
            )
            .map_err(database_error)?;
        receipt(
            &transaction,
            &request.workspace_id,
            CommandName::CreateSession,
            &request.request_id,
            &arguments,
            &result,
            class,
        )?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, Vec::new()))
    }

    fn enqueue_run(
        &mut self,
        request: &StartFakeRunRequest,
        seed: RunSeed,
    ) -> StoreResult<MutationCommit<RunSnapshot>> {
        self.ready()?;
        validate_start_fake_run(request)?;
        let arguments = canonical_arguments(request)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(&request.workspace_id))?;
        owned_session(&transaction, &request.workspace_id, &request.session_id)?;
        if let Some(result) = replay::<RunSnapshot>(
            &transaction,
            &request.workspace_id,
            CommandName::StartFakeRun,
            &request.request_id,
            &arguments,
        )? {
            checked_stored(validate_snapshot(&result.result))?;
            require_store(
                result.result.workspace_id == request.workspace_id
                    && result.result.session_id == request.session_id,
                "storage_unavailable",
                "Stored receipt ownership is inconsistent.",
            )?;
            return Ok(result);
        }
        let class = receipt_class(&transaction, &request.workspace_id, false)?;
        let (total, queued): (u64, u64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN state='queued' THEN 1 ELSE 0 END),0) FROM runs WHERE workspace_id=?1",
            [&request.workspace_id], |row| Ok((row.get(0)?, row.get(1)?))
        ).map_err(database_error)?;
        require_store(
            total < WORKSPACE_RUNS as u64,
            "capacity_exceeded",
            "Workspace run-history capacity has been reached.",
        )?;
        require_store(
            queued < QUEUED_RUNS as u64,
            "queue_full",
            "The simulated run queue is full.",
        )?;
        validate_id(&seed.run_id)?;
        validate_id(&seed.event_id)?;
        validate_timestamp(&seed.created_at)?;
        let collision: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM runs WHERE run_id=?1",
                [&seed.run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?;
        require_store(
            collision.is_none(),
            "state_conflict",
            "Run identity is already committed.",
        )?;
        let order: Option<u64> = transaction.query_row(
            "UPDATE workspaces SET last_enqueue=last_enqueue+1 WHERE workspace_id=?1 AND last_enqueue<?2 RETURNING last_enqueue",
            params![request.workspace_id, JS_MAX_INTEGER], |row| row.get(0)
        ).optional().map_err(database_error)?;
        let order = order.ok_or_else(|| {
            error(
                "capacity_exceeded",
                "Workspace enqueue-order capacity has been reached.",
            )
        })?;
        let result = RunSnapshot {
            contract_version: 1,
            workspace_id: request.workspace_id.clone(),
            session_id: request.session_id.clone(),
            run_id: seed.run_id,
            provider_id: ProviderId::Fake,
            state: RunState::Queued,
            last_seq: 1,
            created_at: seed.created_at.clone(),
            updated_at: seed.created_at.clone(),
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
            },
            terminal_error: None,
            enqueue_order: order,
        };
        let event = RunEvent::from_parts(
            EventHeader {
                contract_version: 1,
                event_id: seed.event_id,
                workspace_id: result.workspace_id.clone(),
                session_id: result.session_id.clone(),
                run_id: result.run_id.clone(),
                provider_id: ProviderId::Fake,
                source: EventSource::Fixture,
                seq: 1,
                occurred_at: seed.created_at,
            },
            EventPayload::RunCreated(RunCreatedPayload {
                state: "queued".into(),
            }),
        );
        validate_snapshot(&result)?;
        validate_event(&event)?;
        event_identity_available(&transaction, &event)?;
        transaction.execute(
            "INSERT INTO runs(run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,prompt,scenario,
                ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count)
             VALUES (?1,?2,?3,'queued',1,?4,?5,?6,?7,1,0,0,0,0,0)",
            params![result.run_id, result.workspace_id, result.session_id, order, encode(&result)?, request.prompt, request.scenario.as_str()]
        ).map_err(database_error)?;
        insert_event(&transaction, &event)?;
        receipt(
            &transaction,
            &request.workspace_id,
            CommandName::StartFakeRun,
            &request.request_id,
            &arguments,
            &result,
            class,
        )?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, vec![event]))
    }

    fn cancel_run(
        &mut self,
        request: &CancelRunRequest,
        mutation: Option<AppendBatch>,
    ) -> StoreResult<MutationCommit<RunSnapshot>> {
        self.ready()?;
        validate_cancel(request)?;
        let arguments = canonical_arguments(request)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(&request.workspace_id))?;
        let current = load_run(&transaction, &request.workspace_id, &request.run_id)?.snapshot;
        if let Some(result) = replay::<RunSnapshot>(
            &transaction,
            &request.workspace_id,
            CommandName::CancelRun,
            &request.request_id,
            &arguments,
        )? {
            checked_stored(validate_snapshot(&result.result))?;
            require_store(
                result.result.workspace_id == request.workspace_id
                    && result.result.run_id == request.run_id,
                "storage_unavailable",
                "Stored receipt ownership is inconsistent.",
            )?;
            return Ok(result);
        }
        let target = match current.state {
            RunState::Queued => Some(RunState::Cancelled),
            RunState::Starting | RunState::Running | RunState::WaitingInput => {
                Some(RunState::CancelRequested)
            }
            state if state == RunState::CancelRequested || is_terminal(state) => None,
            _ => return Err(unavailable()),
        };
        let (result, events) = match (target, mutation) {
            (None, None) => (current.clone(), Vec::new()),
            (Some(target), Some(batch)) => {
                require_store(
                    batch.workspace_id == request.workspace_id
                        && batch.run_id == request.run_id
                        && batch.events.len() == 1,
                    "state_conflict",
                    "Cancellation must contain exactly its required transition.",
                )?;
                let correct = matches!(&batch.events[0], RunEvent::RunStateChanged { source, payload, .. }
                    if *source == EventSource::System && payload.from == current.state && payload.to == target && payload.error.is_none());
                require_store(
                    correct,
                    "state_conflict",
                    "Cancellation proposed an unrelated event or transition.",
                )?;
                // Capacity is checked before committing the optional append.
                let class = receipt_class(&transaction, &request.workspace_id, true)?;
                let result = append(&transaction, &batch)?;
                receipt(
                    &transaction,
                    &request.workspace_id,
                    CommandName::CancelRun,
                    &request.request_id,
                    &arguments,
                    &result,
                    class,
                )?;
                transaction.commit().map_err(database_error)?;
                return Ok(changed(result, batch.events));
            }
            _ => {
                return Err(error(
                    "state_conflict",
                    "Cancellation proposal does not match the current run state.",
                ))
            }
        };
        let class = receipt_class(&transaction, &request.workspace_id, false)?;
        receipt(
            &transaction,
            &request.workspace_id,
            CommandName::CancelRun,
            &request.request_id,
            &arguments,
            &result,
            class,
        )?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, events))
    }

    fn append_batch(&mut self, mutation: AppendBatch) -> StoreResult<MutationCommit<RunSnapshot>> {
        self.ready()?;
        validate_id(&mutation.workspace_id)?;
        validate_id(&mutation.run_id)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(&mutation.workspace_id))?;
        let result = append(&transaction, &mutation)?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, mutation.events))
    }

    fn workspace_snapshot(&mut self, request: &WorkspaceRequest) -> StoreResult<WorkspaceSnapshot> {
        validate_workspace_request(request)?;
        let available = self.recovered;
        let transaction = self.conn.transaction().map_err(database_error)?;
        let result = read_snapshot(&transaction, Some(&request.workspace_id), available)?;
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn get_run(&mut self, workspace_id: &str, run_id: &str) -> StoreResult<QueuedRun> {
        validate_id(workspace_id)?;
        validate_id(run_id)?;
        let transaction = self.conn.transaction().map_err(database_error)?;
        workspace(&transaction, Some(workspace_id))?;
        let result = load_run(&transaction, workspace_id, run_id)?.queued();
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn next_queued(&mut self, workspace_id: &str) -> StoreResult<Option<QueuedRun>> {
        self.ready()?;
        validate_id(workspace_id)?;
        let transaction = self.conn.transaction().map_err(database_error)?;
        // This bounded snapshot validates both the active slot and all ownership.
        let snapshot = read_snapshot(&transaction, Some(workspace_id), true)?;
        let result = if snapshot
            .runs
            .iter()
            .any(|run| run.state != RunState::Queued && !is_terminal(run.state))
        {
            None
        } else {
            match snapshot
                .runs
                .iter()
                .filter(|run| run.state == RunState::Queued)
                .min_by_key(|run| run.enqueue_order)
            {
                Some(run) => Some(load_run(&transaction, workspace_id, &run.run_id)?.queued()),
                None => None,
            }
        };
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn list_run_events(&mut self, request: &EventPageRequest) -> StoreResult<EventPage> {
        validate_event_page(request)?;
        let transaction = self.conn.transaction().map_err(database_error)?;
        workspace(&transaction, Some(&request.workspace_id))?;
        let run = load_run(&transaction, &request.workspace_id, &request.run_id)?.snapshot;
        let through = request.through_seq.unwrap_or(run.last_seq);
        require_store(
            through <= run.last_seq && request.after_seq <= through,
            "validation_failed",
            "Event cursor exceeds committed history or its pinned high-water mark.",
        )?;
        let mut events = Vec::new();
        {
            let mut statement = transaction.prepare(
                "SELECT event_id,seq,event_json FROM events WHERE workspace_id=?1 AND run_id=?2 AND seq>?3 AND seq<=?4 ORDER BY seq LIMIT ?5"
            ).map_err(database_error)?;
            let mut rows = statement
                .query(params![
                    request.workspace_id,
                    request.run_id,
                    request.after_seq,
                    through,
                    request.limit
                ])
                .map_err(database_error)?;
            while let Some(row) = rows.next().map_err(database_error)? {
                let event = decode_stored_event(row, 2)?;
                let seq: u64 = row.get(1).map_err(database_error)?;
                validate_event(&event).map_err(|_| {
                    error(
                        "event_history_unavailable",
                        "Committed event data is invalid.",
                    )
                })?;
                let header = event.header();
                require_store(
                    matches!(row.get_ref(0).map_err(database_error)?,
                        rusqlite::types::ValueRef::Text(value) if value == header.event_id.as_bytes())
                        && header.seq == seq
                        && seq == request.after_seq + events.len() as u64 + 1
                        && header.workspace_id == request.workspace_id
                        && header.run_id == request.run_id
                        && header.session_id == run.session_id
                        && header.provider_id == run.provider_id,
                    "event_history_unavailable",
                    "Committed event identities or sequences are inconsistent.",
                )?;
                events.push(event);
            }
        }
        let expected = (through - request.after_seq).min(request.limit);
        require_store(
            events.len() as u64 == expected,
            "event_history_unavailable",
            "The requested committed event range is incomplete.",
        )?;
        let next = request.after_seq + events.len() as u64;
        let result = EventPage {
            contract_version: 1,
            workspace_id: request.workspace_id.clone(),
            run_id: request.run_id.clone(),
            events,
            last_seq: run.last_seq,
            through_seq: through,
            next_after_seq: next,
            has_more: next < through,
        };
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn save_layout(&mut self, request: &SaveLayoutRequest) -> StoreResult<MutationCommit<Layout>> {
        self.ready()?;
        validate_version(request.contract_version)?;
        validate_id(&request.workspace_id)?;
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let row = workspace(&transaction, Some(&request.workspace_id))?;
        let session_values = sessions(&transaction, &request.workspace_id)?;
        validate_save_layout(request, &session_values)?;
        let arguments = canonical_arguments(request)?;
        if let Some(result) = replay::<Layout>(
            &transaction,
            &request.workspace_id,
            CommandName::SaveLayout,
            &request.request_id,
            &arguments,
        )? {
            checked_stored(validate_layout(
                &result.result,
                &request.workspace_id,
                &session_values,
            ))?;
            return Ok(result);
        }
        let (_, warning) = stored_layout(&row, &session_values);
        if let Some(warning) = warning {
            return Err(warning);
        }
        require_store(
            row.revision == request.expected_revision,
            "layout_revision_conflict",
            "Layout revision has changed.",
        )?;
        require_store(
            row.revision < JS_MAX_INTEGER,
            "capacity_exceeded",
            "Layout revision capacity has been reached.",
        )?;
        let class = receipt_class(&transaction, &request.workspace_id, false)?;
        let mut result = request.layout.clone();
        result.revision += 1;
        validate_layout(&result, &request.workspace_id, &session_values)?;
        let updated = transaction.execute(
            "UPDATE workspaces SET layout_json=?1,layout_revision=?2 WHERE workspace_id=?3 AND layout_revision=?4",
            params![encode(&result)?, result.revision, request.workspace_id, request.expected_revision]
        ).map_err(database_error)?;
        require_store(
            updated == 1,
            "layout_revision_conflict",
            "Layout revision has changed.",
        )?;
        receipt(
            &transaction,
            &request.workspace_id,
            CommandName::SaveLayout,
            &request.request_id,
            &arguments,
            &result,
            class,
        )?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, Vec::new()))
    }
}

#[cfg(test)]
mod tests;
