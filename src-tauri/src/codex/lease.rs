use crate::codex::{CodexError, SMOKE_MODEL};
use crate::execution::validation::validate_id;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const LEASE_MARKER_FILE: &str = "codex-lease.json";
pub const LEASE_DIR_PREFIX: &str = "agent-workbench-codex-";
pub const SMOKE_ENV_VAR: &str = "WORKBENCH_CODEX_SMOKE_ONCE";
pub const SMOKE_PURPOSE: &str = "codex_ack_smoke_once";
pub const DEFAULT_SMOKE_TOKEN_REL_PATH: &str = "artifacts/codex-smoke/approval-token.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseMarker {
    root: PathBuf,
    nonce: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SmokeApprovalToken {
    schema_version: u32,
    purpose: String,
    model: String,
    nonce: String,
}

pub(crate) fn not_reparse(path: &Path) -> Result<(), CodexError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| CodexError::LeaseError)?;
    if metadata.file_type().is_symlink() {
        return Err(CodexError::LeaseError);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(CodexError::LeaseError);
        }
    }
    Ok(())
}

/// Owned disposable workspace directory under the OS temp root.
/// Cleanup requires confirmed process-tree quiescence and never sweeps prefixes/globs.
pub struct CodexLease {
    root: PathBuf,
    workspace: PathBuf,
    nonce: String,
    cleaned: bool,
}

impl CodexLease {
    pub fn create() -> Result<Self, CodexError> {
        let temp_raw = std::env::temp_dir();
        not_reparse(&temp_raw)?;
        let temp_root = fs::canonicalize(&temp_raw).map_err(|_| CodexError::LeaseError)?;
        not_reparse(&temp_root)?;
        let folder_id = Uuid::new_v4().to_string();
        let nonce = Uuid::new_v4().to_string();
        let root = temp_root.join(format!("{LEASE_DIR_PREFIX}{folder_id}"));
        // fs::create_dir fails if the path already exists; never adopt pre-existing paths.
        fs::create_dir(&root).map_err(|_| CodexError::LeaseError)?;
        let init_result = (|| -> Result<PathBuf, CodexError> {
            not_reparse(&root)?;
            let canonical_root = fs::canonicalize(&root).map_err(|_| CodexError::LeaseError)?;
            if canonical_root != root {
                return Err(CodexError::LeaseError);
            }
            let marker_path = canonical_root.join(LEASE_MARKER_FILE);
            let marker = LeaseMarker {
                root: canonical_root.clone(),
                nonce: nonce.clone(),
            };
            let bytes = serde_json::to_vec(&marker).map_err(|_| CodexError::LeaseError)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker_path)
                .map_err(|_| CodexError::LeaseError)?;
            file.write_all(&bytes).map_err(|_| CodexError::LeaseError)?;
            file.sync_all().map_err(|_| CodexError::LeaseError)?;
            drop(file);
            not_reparse(&marker_path)?;
            let workspace = canonical_root.join("workspace");
            fs::create_dir(&workspace).map_err(|_| CodexError::LeaseError)?;
            not_reparse(&workspace)?;
            Ok(workspace)
        })();
        match init_result {
            Ok(workspace) => Ok(Self {
                root,
                workspace,
                nonce,
                cleaned: false,
            }),
            Err(err) => {
                let _ = fs::remove_dir_all(&root);
                Err(err)
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn is_cleaned(&self) -> bool {
        self.cleaned
    }

    pub fn validate(&self) -> Result<PathBuf, CodexError> {
        if self.cleaned {
            return Err(CodexError::LeaseError);
        }
        not_reparse(&self.root)?;
        let canonical = fs::canonicalize(&self.root).map_err(|_| CodexError::LeaseError)?;
        not_reparse(&canonical)?;
        let temp_root =
            fs::canonicalize(std::env::temp_dir()).map_err(|_| CodexError::LeaseError)?;
        let name = canonical.file_name().and_then(|v| v.to_str()).unwrap_or("");
        let valid_name = name
            .strip_prefix(LEASE_DIR_PREFIX)
            .is_some_and(|suffix| validate_id(suffix).is_ok());
        if canonical.parent() != Some(temp_root.as_path())
            || !valid_name
            || validate_id(&self.nonce).is_err()
        {
            return Err(CodexError::LeaseError);
        }
        let marker_path = canonical.join(LEASE_MARKER_FILE);
        not_reparse(&marker_path)?;
        let marker_meta = fs::metadata(&marker_path).map_err(|_| CodexError::LeaseError)?;
        if !marker_meta.is_file() || marker_meta.len() == 0 || marker_meta.len() > 4096 {
            return Err(CodexError::LeaseError);
        }
        let raw = fs::read(&marker_path).map_err(|_| CodexError::LeaseError)?;
        let stored: LeaseMarker =
            serde_json::from_slice(&raw).map_err(|_| CodexError::LeaseError)?;
        if stored.root != canonical || stored.nonce != self.nonce {
            return Err(CodexError::LeaseError);
        }
        let expected_workspace = canonical.join("workspace");
        not_reparse(&expected_workspace)?;
        let canonical_workspace =
            fs::canonicalize(&self.workspace).map_err(|_| CodexError::LeaseError)?;
        if canonical_workspace != expected_workspace || !canonical_workspace.is_dir() {
            return Err(CodexError::LeaseError);
        }
        Ok(canonical)
    }

    /// Removes the verified disposable workspace ONLY after the caller proves
    /// the owned process tree is quiescent (zero active processes, pipes closed).
    pub fn cleanup(&mut self, tree_quiescent: bool) -> Result<(), CodexError> {
        if !tree_quiescent {
            return Err(CodexError::CleanupFailed);
        }
        if self.cleaned {
            return Ok(());
        }
        let canonical = self.validate().map_err(|_| CodexError::CleanupFailed)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if (fs::remove_dir_all(&canonical).is_ok() || !canonical.exists())
                && !canonical.exists()
            {
                self.cleaned = true;
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(CodexError::CleanupFailed);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Atomic one-shot authorization gate for the single Phase 2B live smoke task.
/// Consumes and deletes the approval token BEFORE executable verification or process spawn.
pub struct OneShotSmokeGate;

impl OneShotSmokeGate {
    pub fn consume_from_env(token_path: &Path) -> Result<String, CodexError> {
        let env_val = std::env::var_os(SMOKE_ENV_VAR);
        Self::consume_with_env(token_path, env_val.as_deref())
    }

    pub fn consume_with_env(
        token_path: &Path,
        env_value: Option<&OsStr>,
    ) -> Result<String, CodexError> {
        if env_value != Some(OsStr::new("1")) {
            return Err(CodexError::SmokeGateNotArmed);
        }
        let parent = token_path.parent().ok_or(CodexError::SmokeGateNotArmed)?;
        if !parent.as_os_str().is_empty() {
            not_reparse(parent).map_err(|_| CodexError::SmokeGateNotArmed)?;
        }
        not_reparse(token_path).map_err(|_| CodexError::SmokeGateNotArmed)?;
        let claimed_path = parent.join(format!(".codex-smoke-claimed-{}.tmp", Uuid::new_v4()));
        atomic_claim_file(token_path, &claimed_path)?;

        // Always delete the claimed file before returning so a bad token can never be retried.
        let read_result = (|| -> Result<String, CodexError> {
            not_reparse(&claimed_path).map_err(|_| CodexError::SmokeGateConsumeFailed)?;
            let meta =
                fs::metadata(&claimed_path).map_err(|_| CodexError::SmokeGateConsumeFailed)?;
            if !meta.is_file() || meta.len() == 0 || meta.len() > 4096 {
                return Err(CodexError::SmokeGateConsumeFailed);
            }
            let bytes = fs::read(&claimed_path).map_err(|_| CodexError::SmokeGateConsumeFailed)?;
            let parsed: SmokeApprovalToken =
                serde_json::from_slice(&bytes).map_err(|_| CodexError::SmokeGateConsumeFailed)?;
            if parsed.schema_version != 1
                || parsed.purpose != SMOKE_PURPOSE
                || parsed.model != SMOKE_MODEL
                || validate_id(&parsed.nonce).is_err()
            {
                return Err(CodexError::SmokeGateConsumeFailed);
            }
            Ok(parsed.nonce)
        })();
        let remove_ok = fs::remove_file(&claimed_path).is_ok() && !claimed_path.exists();
        if !remove_ok {
            return Err(CodexError::SmokeGateConsumeFailed);
        }
        read_result
    }
}

#[cfg(windows)]
fn atomic_claim_file(src: &Path, dst: &Path) -> Result<(), CodexError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    // dwFlags = 0 forbids overwriting dst and fails atomically if src was already claimed.
    let moved = unsafe { MoveFileExW(src_wide.as_ptr(), dst_wide.as_ptr(), 0) };
    if moved == 0 {
        return Err(CodexError::SmokeGateNotArmed);
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_claim_file(src: &Path, dst: &Path) -> Result<(), CodexError> {
    fs::rename(src, dst).map_err(|_| CodexError::SmokeGateNotArmed)
}
