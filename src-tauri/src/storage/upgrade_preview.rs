//! Native-only legacy-to-v2 COPY rehearsal. No original SQLite connection is
//! opened, and no confirm/commit-to-original method exists in this checkpoint.
//! Runtime/IPC remain v1. Future upgrade wiring requires its own source gate.
use super::*;
use crate::execution_v2::{self as v2, stored::project_legacy_workspace};
use std::path::PathBuf;

const MIGRATION: &str = include_str!("migrate_v2.sql");
const MARKER: &str = "preview-owner";

fn not_reparse(path: &Path) -> StoreResult<()> {
    let meta = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    require_store(
        !meta.file_type().is_symlink(),
        "storage_unavailable",
        "Unverified preview path is preserved.",
    )?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        require_store(
            meta.file_attributes() & 0x400 == 0,
            "storage_unavailable",
            "Unverified preview path is preserved.",
        )?;
    }
    Ok(())
}

struct OwnedCopy {
    path: PathBuf,
    nonce: String,
}
impl OwnedCopy {
    fn create(parent: &Path) -> StoreResult<Self> {
        let path = parent.join(format!(".execution-v2-preview-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).map_err(|_| unavailable())?;
        let owned = Self {
            path,
            nonce: uuid::Uuid::new_v4().to_string(),
        };
        // Guard only adopts a newly created directory; marker uses create_new.
        use std::io::Write;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(owned.path.join(MARKER))
            .map_err(|_| unavailable())?;
        file.write_all(owned.nonce.as_bytes())
            .map_err(|_| unavailable())?;
        file.sync_all().map_err(|_| unavailable())?;
        Ok(owned)
    }
    fn cleanup(&self) -> StoreResult<()> {
        not_reparse(&self.path)?;
        not_reparse(&self.path.join(MARKER))?;
        require_store(
            fs::read(self.path.join(MARKER)).map_err(|_| unavailable())? == self.nonce.as_bytes(),
            "storage_unavailable",
            "Unverified preview ownership is preserved.",
        )?;
        fs::remove_dir_all(&self.path).map_err(|_| unavailable())
    }
}
impl Drop for OwnedCopy {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(super) fn migrate_copy(
    conn: &mut Connection,
    before_commit: impl FnOnce(&Connection) -> StoreResult<()>,
) -> StoreResult<()> {
    migrate_with(conn, MIGRATION, before_commit)
}

/// One transactional schema step with foreign keys off, an FK integrity check
/// before commit, and enforcement restored on every path (including rollback).
pub(super) fn migrate_with(
    conn: &mut Connection,
    sql: &str,
    before_commit: impl FnOnce(&Connection) -> StoreResult<()>,
) -> StoreResult<()> {
    // Only called on a fresh, owned copy or in-memory fixture. Never expose a
    // function that accepts an arbitrary original path as an upgrade shortcut.
    require_store(
        conn.is_autocommit(),
        "storage_unavailable",
        "Preview migration requires an idle connection.",
    )?;
    conn.pragma_update(None, "foreign_keys", "OFF")
        .map_err(database_error)?;
    let result = (|| {
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        transaction.execute_batch(sql).map_err(database_error)?;
        let fk: Option<i64> = transaction
            .query_row("SELECT 1 FROM pragma_foreign_key_check LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(database_error)?;
        require_store(
            fk.is_none(),
            "storage_unavailable",
            "Preview migration has invalid relationships.",
        )?;
        before_commit(&transaction)?;
        transaction.commit().map_err(database_error)
    })();
    // The transaction (including rollback on failure) is gone before restoring
    // enforcement. Never return a surviving reusable connection with FK off.
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(database_error)?;
    let enabled: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(database_error)?;
    require_store(
        enabled == 1,
        "storage_unavailable",
        "Foreign-key enforcement could not be restored.",
    )?;
    result
}

struct PreviewIds;
impl IdSource for PreviewIds {
    fn next_id(&mut self) -> Id {
        uuid::Uuid::new_v4().to_string()
    }
}

/// Holds original exclusive lock while inspecting a nonce-owned copy. Dropping
/// releases SQLite handles BEFORE deleting the copy and releases original lock
/// last. There is intentionally no original upgrade confirmation method yet.
pub struct LegacyUpgradePreview {
    conn: Option<Connection>,
    copy: OwnedCopy,
    _ownership: File,
    upgrade_id: String,
    snapshot: v2::WorkspaceSnapshot,
    interrupted_runs: usize,
}
impl LegacyUpgradePreview {
    pub fn inspect(directory: &Path, now: &str) -> StoreResult<Self> {
        validate_timestamp(now)?;
        not_reparse(directory)?;
        let directory = fs::canonicalize(directory).map_err(|_| unavailable())?;
        let lock = directory.join("execution.lock");
        // A v1 initialized directory already owns a lock file. Do not create
        // arbitrary directories/locks when asked to preview an unknown path.
        not_reparse(&lock)?;
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .map_err(|_| unavailable())?;
        fs2::FileExt::try_lock_exclusive(&ownership)
            .map_err(|_| error("storage_busy", "Another process owns execution storage."))?;
        require_delete_family(&directory)?;
        let database = directory.join("execution.sqlite3");
        not_reparse(&database)?;
        require_store(
            stored_file_length(&database)?.is_some_and(|n| n > 0),
            "unsupported_storage_version",
            "An existing v1 database is required for this preview.",
        )?;
        let copy = OwnedCopy::create(&directory)?;
        fs::copy(&database, copy.path.join("execution.sqlite3")).map_err(|_| unavailable())?;
        let journal = directory.join("execution.sqlite3-journal");
        if stored_file_length(&journal)?.is_some() {
            not_reparse(&journal)?;
            fs::copy(journal, copy.path.join("execution.sqlite3-journal"))
                .map_err(|_| unavailable())?;
        }
        // Original is never opened through SQLite (even read-only). A hot
        // journal can recover only a validation/preview copy before confirmation.
        require_store(
            !preflight_database(&copy.path)?,
            "unsupported_storage_version",
            "Preview requires initialized v1 storage.",
        )?;
        let mut conn = Connection::open_with_flags(
            copy.path.join("execution.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(database_error)?;
        conn.busy_timeout(Duration::ZERO).map_err(database_error)?;
        require_store(
            !inspect_database(&conn)?,
            "unsupported_storage_version",
            "Preview requires initialized v1 storage.",
        )?;
        migrate_copy(&mut conn, |_| Ok(()))?;
        // Reuse exact v1 recovery/audit on homogeneous legacy records. No
        // projected v2 value is supplied to historical receipt-prefix equality.
        let mut store = SqliteStore {
            conn,
            _ownership: ownership,
            recovered: false,
        };
        store.bootstrap(WorkspaceSeed {
            workspace_id: uuid::Uuid::new_v4().to_string(),
            created_at: now.into(),
        })?;
        let report = store.startup_recover(now, &mut PreviewIds)?;
        audit_database(&store.conn)?;
        let legacy = read_snapshot(&store.conn, None, false)?;
        let snapshot = project_legacy_workspace(&legacy)?;
        // Both independent version-aware and legacy passes must accept this
        // homogeneous rehearsal. The mixed auditor never opens the original.
        require_store(
            super::audit_v2::audit_copy(&store.conn)?.as_ref() == Some(&snapshot),
            "storage_unavailable",
            "Mixed-version preview projection disagrees with legacy history.",
        )?;
        let SqliteStore {
            conn, _ownership, ..
        } = store;
        Ok(Self {
            conn: Some(conn),
            copy,
            _ownership,
            upgrade_id: uuid::Uuid::new_v4().to_string(),
            snapshot,
            interrupted_runs: report.interrupted_runs.len(),
        })
    }
    /// Consume this initial preview to rehearse native v2 admission on its
    /// disposable copy. No original transition/confirmation is performed, and
    /// the returned journal cannot execute or dispatch queued work.
    pub fn into_admission_rehearsal(self) -> StoreResult<super::admission_v2::AdmissionRehearsal> {
        super::admission_v2::AdmissionRehearsal::new(self)
    }
    pub(super) fn copy_connection(&mut self) -> StoreResult<&mut Connection> {
        self.conn.as_mut().ok_or_else(unavailable)
    }
    pub fn bootstrap(&self) -> v2::BootstrapResponse {
        v2::BootstrapResponse {
            contract_version: 2,
            storage_state: v2::StorageState::UpgradeRequired,
            upgrade_id: Some(self.upgrade_id.clone()),
            snapshot: None,
        }
    }
    /// Hypothetical post-upgrade history for native tests/inspection; never an
    /// execution-ready workspace and never authority to resume archived work.
    pub fn projected_history(&self) -> &v2::WorkspaceSnapshot {
        &self.snapshot
    }
    pub fn interrupted_runs(&self) -> usize {
        self.interrupted_runs
    }
    #[cfg(test)]
    pub(super) fn connection(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
    pub fn close(mut self) -> StoreResult<()> {
        drop(self.conn.take());
        self.copy.cleanup()
    }
}
impl Drop for LegacyUpgradePreview {
    fn drop(&mut self) {
        drop(self.conn.take());
    }
}
