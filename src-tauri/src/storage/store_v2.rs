//! Fresh v2 journal store (user decision 2026-10-21: v2 starts in its own new
//! app-data directory; legacy `simulated-execution` data is never opened,
//! upgraded or touched). Opens only an empty or an existing v2 database it owns
//! under an exclusive lock, runs the full startup audit on every open, and
//! becomes ready only after exactly one startup recovery per open.
use super::*;
use crate::execution_v2::{self as v2, ports::RecoveryReport};

/// Storage 2 -> 3 (adds `codex_text_v2` and the carried-context column).
pub(super) const MIGRATION_V3: &str = include_str!("migrate_v3.sql");

pub struct V2Store {
    // Field order keeps ownership until SQLite has closed, including on Windows.
    conn: Connection,
    _ownership: File,
    recovered: bool,
}

fn lock(directory: &Path) -> StoreResult<File> {
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
    Ok(ownership)
}

/// Consistent copy of the v2 database next to it (`VACUUM INTO`), verified before
/// the migration is allowed to proceed. Never overwrites or deletes a backup.
fn backup_before_v3(conn: &Connection, directory: &Path) -> StoreResult<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| unavailable())?
        .as_secs();
    let mut target = directory.join(format!("execution.sqlite3.pre-v3-{stamp}.bak"));
    let mut suffix = 0u32;
    while target.exists() {
        suffix += 1;
        target = directory.join(format!("execution.sqlite3.pre-v3-{stamp}-{suffix}.bak"));
    }
    let text = target.to_str().ok_or_else(unavailable)?;
    conn.execute("VACUUM INTO ?1", [text])
        .map_err(database_error)?;
    let copy = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(database_error)?;
    let version: i64 = copy
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(database_error)?;
    let integrity: String = copy
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(database_error)?;
    require_store(
        version == 2 && integrity == "ok",
        "storage_unavailable",
        "The pre-migration backup could not be verified; the journal was not changed.",
    )
}

impl V2Store {
    pub fn open(directory: &Path) -> StoreResult<Self> {
        fs::create_dir_all(directory).map_err(|_| unavailable())?;
        let ownership = lock(directory)?;
        // Same family rules as v1: no WAL images, no attached super-journals.
        // An ordinary hot rollback journal of THIS owned database is rolled
        // back by SQLite on first read, then the full audit below must pass.
        require_delete_family(directory)?;
        let journal = directory.join("execution.sqlite3-journal");
        if stored_file_length(&journal)?.is_some_and(|length| length >= 8) {
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
        let mut conn = Connection::open_with_flags(
            directory.join("execution.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(database_error)?;
        conn.busy_timeout(Duration::ZERO).map_err(database_error)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(database_error)?;
        match version {
            0 => {
                let objects: u64 = conn
                    .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))
                    .map_err(database_error)?;
                require_store(
                    objects == 0,
                    "unsupported_storage_version",
                    "Unversioned nonempty storage is preserved and cannot be initialized.",
                )?;
            }
            2 | 3 => {}
            _ => {
                return Err(error(
                    "unsupported_storage_version",
                    "Unsupported execution storage version; original data is preserved.",
                ))
            }
        }
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(database_error)?;
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            journal_mode.eq_ignore_ascii_case("delete"),
            "storage_unavailable",
            "Durable execution journaling could not be enabled.",
        )?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(database_error)?;
        if version == 0 {
            // Build the exact reviewed v2 schema: v1 base + the v2 migration,
            // on this brand-new empty database only (no rows to carry over).
            let transaction = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            transaction.execute_batch(SCHEMA).map_err(database_error)?;
            transaction
                .pragma_update(None, "user_version", STORAGE_VERSION)
                .map_err(database_error)?;
            transaction.commit().map_err(database_error)?;
            upgrade_preview::migrate_copy(&mut conn, |_| Ok(()))?;
            upgrade_preview::migrate_with(&mut conn, MIGRATION_V3, |_| Ok(()))?;
        }
        let enabled: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(database_error)?;
        require_store(
            enabled == 1,
            "storage_unavailable",
            "Foreign-key enforcement could not be enabled.",
        )?;
        // Full schema/integrity/FK/history/receipt audit on EVERY open.
        audit_v2::audit_copy(&conn)?;
        if version == 2 {
            // In-place 2 -> 3: the audited v2 file is backed up first and never
            // overwritten; the migration is one transaction (rolled back on any
            // failure, leaving the v2 file untouched) and the result is re-audited.
            backup_before_v3(&conn, directory)?;
            upgrade_preview::migrate_with(&mut conn, MIGRATION_V3, |_| Ok(()))?;
            audit_v2::audit_copy(&conn)?;
        }
        Ok(Self {
            conn,
            _ownership: ownership,
            recovered: false,
        })
    }

    /// Create the single managed workspace once; return the audited projection
    /// (execution availability is decided by the runtime, never by storage).
    pub fn bootstrap(&mut self, seed: WorkspaceSeed) -> StoreResult<v2::WorkspaceSnapshot> {
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let count: u64 = transaction
            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
            .map_err(database_error)?;
        if count == 0 {
            require_store(
                !self.recovered,
                "storage_unavailable",
                "Managed workspace disappeared after initialization.",
            )?;
            validate_id(&seed.workspace_id)?;
            validate_timestamp(&seed.created_at)?;
            let record = WorkspaceSnapshotRecord {
                workspace_id: seed.workspace_id.clone(),
                label: "Managed workspace".into(),
                created_at: seed.created_at,
            };
            let layout = empty_layout(&seed.workspace_id);
            transaction
                .execute(
                    "INSERT INTO workspaces(workspace_id,singleton,record_json,layout_json,layout_revision,last_enqueue) VALUES (?1,1,?2,?3,0,0)",
                    params![seed.workspace_id, encode(&record)?, encode(&layout)?],
                )
                .map_err(database_error)?;
        }
        let snapshot = audit_v2::read_materialized(&transaction, None)?.snapshot;
        transaction.commit().map_err(database_error)?;
        Ok(snapshot)
    }

    /// Exactly once per open: interrupts every unfinished run (queued included)
    /// BEFORE readiness. Repeats in the same open are empty and write nothing.
    pub fn startup_recover(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> StoreResult<RecoveryReport> {
        if self.recovered {
            return Ok(RecoveryReport::default());
        }
        let report = writer_v2::recover_in(&mut self.conn, now, ids)?;
        self.recovered = true;
        Ok(report)
    }

    pub fn is_ready(&self) -> bool {
        self.recovered
    }

    #[cfg(test)]
    pub(super) fn raw(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

impl JournalConnection for V2Store {
    fn journal_connection(&mut self) -> StoreResult<&mut Connection> {
        require_store(
            self.recovered,
            "storage_unavailable",
            "Startup recovery must finish before execution admission or reads.",
        )?;
        Ok(&mut self.conn)
    }
}
