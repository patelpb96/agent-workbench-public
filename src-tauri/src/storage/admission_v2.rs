//! Copy-only v2 admission rehearsal. Shared SQL knows durable queue/receipt data,
//! not how any agent is launched. No original opener or dispatch route exists.
use super::*;
use crate::execution_v2::{
    self as v2,
    admission::{PreparedAdmission, StartRequest},
    ports::{AdmissionJournal, MutationCommit},
    stored::{SessionVersion, SnapshotVersion, StoredRunSnapshot, StoredSession},
    validation as policy,
};
use upgrade_preview::LegacyUpgradePreview;

/// Owns the nonce-owned preview copy AND original exclusive lock. Consuming a
/// preview prevents its initial cached projection being mistaken for later copy
/// mutations. Dropping/closing discards rehearsal writes; originals stay v1.
pub struct AdmissionRehearsal {
    owner: LegacyUpgradePreview,
}
impl AdmissionRehearsal {
    pub(super) fn new(mut owner: LegacyUpgradePreview) -> StoreResult<Self> {
        // Admission writes the v3 `context_json` column; the disposable copy is
        // brought to storage 3 first (the original is never touched).
        let version: i64 = owner
            .copy_connection()?
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(database_error)?;
        if version == 2 {
            upgrade_preview::migrate_with(owner.copy_connection()?, store_v2::MIGRATION_V3, |_| Ok(()))?;
        }
        let snapshot = audit_v2::audit_copy(owner.copy_connection()?)?.ok_or_else(unavailable)?;
        require_store(
            snapshot.runs.iter().all(|r| terminal(r.state)),
            "storage_unavailable",
            "Copy recovery must finish before admission rehearsal.",
        )?;
        Ok(Self { owner })
    }
    /// Explicit expensive COPY audit checkpoint, not a per-command runtime read.
    /// Always execution-disabled; queued persistence does not confer dispatch.
    pub fn audit_checkpoint(&mut self) -> StoreResult<v2::WorkspaceSnapshot> {
        audit_v2::audit_copy(self.owner.copy_connection()?)?.ok_or_else(unavailable)
    }
    pub fn bootstrap(&self) -> v2::BootstrapResponse {
        self.owner.bootstrap()
    }
    pub fn close(self) -> StoreResult<()> {
        self.owner.close()
    }
    #[cfg(test)]
    pub(super) fn connection(&mut self) -> &mut Connection {
        self.owner.copy_connection().unwrap()
    }
}
fn terminal(state: v2::RunState) -> bool {
    matches!(
        state,
        v2::RunState::Succeeded
            | v2::RunState::Failed
            | v2::RunState::Cancelled
            | v2::RunState::Interrupted
    )
}
fn guard(ok: bool, code: &str, message: &str) -> StoreResult<()> {
    require_store(ok, code, message)
}
fn changed<T>(result: T, committed_events: Vec<v2::RunEvent>) -> MutationCommit<T> {
    MutationCommit {
        result,
        committed_events,
        replayed: false,
    }
}
pub(super) fn receipt<T: Serialize>(
    conn: &Connection,
    workspace: &str,
    command: v2::CommandName,
    request_id: &str,
    arguments: &str,
    result: &T,
    class: i64,
) -> StoreResult<()> {
    conn.execute("INSERT INTO receipts(workspace_id,command_name,request_id,arguments_json,result_json,emergency) VALUES (?1,?2,?3,?4,?5,?6)",
        params![workspace,command.as_str(),request_id,arguments,encode(result)?,class]).map_err(database_error)?;
    Ok(())
}
pub(super) fn replay<T: DeserializeOwned>(
    conn: &Connection,
    workspace: &str,
    command: v2::CommandName,
    request_id: &str,
    arguments: &str,
) -> StoreResult<Option<MutationCommit<T>>> {
    let found:Option<(String,String)>=conn.query_row(
        "SELECT arguments_json,result_json FROM receipts WHERE workspace_id=?1 AND command_name=?2 AND request_id=?3",
        params![workspace,command.as_str(),request_id],|r| {
            // Same allocation guard as startup audit; no unbounded stored text.
            let args=bounded_text(r,0,1024*1024)?;
            let result=bounded_text(r,1,1024*1024)?;
            Ok((args,result))
        }).optional().map_err(database_error)?;
    match found {
        None => Ok(None),
        Some((original, result)) => {
            guard(
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
fn bounded_text(row: &rusqlite::Row<'_>, column: usize, limit: usize) -> rusqlite::Result<String> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= limit => {
            String::from_utf8(bytes.to_vec()).map_err(|_| rusqlite::Error::InvalidQuery)
        }
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
fn session(conn: &Connection, workspace_id: &str, session_id: &str) -> StoreResult<StoredSession> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT snapshot_json FROM sessions WHERE workspace_id=?1 AND session_id=?2",
            params![workspace_id, session_id],
            |r| bounded_text(r, 0, 16_384),
        )
        .optional()
        .map_err(database_error)?;
    let raw = raw.ok_or_else(|| {
        error(
            "session_not_found",
            "Session was not found in the selected workspace.",
        )
    })?;
    let stored = StoredSession::decode(&raw).map_err(|_| unavailable())?;
    let value = stored.projected().map_err(|_| unavailable())?;
    guard(
        value.workspace_id == workspace_id && value.session_id == session_id,
        "storage_unavailable",
        "Stored session ownership is inconsistent.",
    )?;
    Ok(stored)
}
impl JournalConnection for AdmissionRehearsal {
    fn journal_connection(&mut self) -> StoreResult<&mut Connection> {
        self.owner.copy_connection()
    }
}
impl<J: JournalConnection> AdmissionJournal for J {
    fn create_session(
        &mut self,
        request: &v2::CreateSessionRequest,
        seed: SessionSeed,
    ) -> StoreResult<MutationCommit<v2::SessionSnapshot>> {
        policy::validate_create_session(request)?;
        let arguments = canonical_arguments(request)?;
        let conn = self.journal_connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(&request.workspace_id))?;
        if let Some(commit) = replay::<v2::SessionSnapshot>(
            &transaction,
            &request.workspace_id,
            v2::CommandName::CreateSessionV2,
            &request.request_id,
            &arguments,
        )? {
            policy::validate_session(&commit.result).map_err(|_| unavailable())?;
            let stored = session(
                &transaction,
                &request.workspace_id,
                &commit.result.session_id,
            )?;
            guard(
                matches!(stored.original(),SessionVersion::V2(value) if *value==commit.result)
                    && commit.result.provider_id == request.provider_id
                    && commit.result.label == request.label,
                "storage_unavailable",
                "Stored session receipt is inconsistent.",
            )?;
            return Ok(commit);
        }
        let class = receipt_class(&transaction, &request.workspace_id, false)?;
        let count: u64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE workspace_id=?1",
                [&request.workspace_id],
                |r| r.get(0),
            )
            .map_err(database_error)?;
        guard(
            count < WORKSPACE_SESSIONS as u64,
            "capacity_exceeded",
            "Workspace session capacity has been reached.",
        )?;
        let result = v2::SessionSnapshot {
            contract_version: 2,
            workspace_id: request.workspace_id.clone(),
            session_id: seed.session_id,
            provider_id: request.provider_id,
            label: request.label.clone(),
            created_at: seed.created_at,
        };
        policy::validate_session(&result)?;
        let collision: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
                [&result.session_id],
                |r| r.get(0),
            )
            .map_err(database_error)?;
        guard(
            !collision,
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
            v2::CommandName::CreateSessionV2,
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
        request: StartRequest<'_>,
        seed: RunSeed,
    ) -> StoreResult<MutationCommit<v2::RunSnapshot>> {
        // Native IDs are cheap to validate before querying; full typed adapter
        // policy and immutable session ownership are checked in the transaction.
        validate_id(request.workspace_id())?;
        validate_id(request.session_id())?;
        let conn = self.journal_connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        workspace(&transaction, Some(request.workspace_id()))?;
        let session = session(&transaction, request.workspace_id(), request.session_id())?
            .projected()
            .map_err(|_| unavailable())?;
        let prepared = PreparedAdmission::validate(request, &session)?;
        if let Some(commit) = replay::<v2::RunSnapshot>(
            &transaction,
            prepared.workspace_id(),
            prepared.command(),
            prepared.request_id(),
            prepared.arguments(),
        )? {
            policy::validate_snapshot(&commit.result).map_err(|_| unavailable())?;
            let (raw,prompt,scenario,provider,profile,context):(String,String,Option<String>,String,Option<String>,Option<String>)=transaction.query_row(
                "SELECT snapshot_json,prompt,scenario,provider_id,profile_id,context_json FROM runs WHERE workspace_id=?1 AND run_id=?2",
                params![prepared.workspace_id(),commit.result.run_id],|r|Ok((bounded_text(r,0,65_536)?,bounded_text(r,1,65_536)?,r.get(2)?,bounded_text(r,3,16)?,r.get(4)?,r.get(5)?))).map_err(database_error)?;
            let current = StoredRunSnapshot::decode(&raw).map_err(|_| unavailable())?;
            guard(
                matches!(current.original(), SnapshotVersion::V2(_)),
                "storage_unavailable",
                "Stored admission version is inconsistent.",
            )?;
            let current = current.projected().map_err(|_| unavailable())?;
            // Reconstruct the original queued materialization from immutable
            // stored identity, NOT the caller's fresh seed or current state.
            let expected = prepared.materialize(
                RunSeed {
                    run_id: current.run_id.clone(),
                    event_id: current.run_id.clone(),
                    created_at: current.created_at.clone(),
                },
                current.enqueue_order,
            )?;
            guard(
                commit.result == expected.snapshot
                    && current.provider_id == expected.snapshot.provider_id
                    && current.profile_id == expected.snapshot.profile_id
                    && current.model_id == expected.snapshot.model_id
                    && current.workspace_id == prepared.workspace_id()
                    && current.session_id == prepared.session_id()
                    && prompt == prepared.input().prompt()
                    && scenario.as_deref() == prepared.input().scenario().map(|s| s.as_str())
                    && provider == prepared.input().provider().as_str()
                    && profile.as_deref() == prepared.input().profile().map(|p| p.as_str())
                    && context == prepared.input().context_json(),
                "storage_unavailable",
                "Stored admission receipt is inconsistent.",
            )?;
            return Ok(commit);
        }
        // Carried context: earlier SUCCEEDED Codex runs of THIS session, strictly
        // ascending by enqueue order. Enforced here, never trusted from the caller.
        let mut previous = 0u64;
        for carried in prepared.input().context_run_ids() {
            let found: Option<(String, String, String, u64)> = transaction
                .query_row(
                    "SELECT session_id,state,provider_id,enqueue_order FROM runs WHERE workspace_id=?1 AND run_id=?2",
                    params![prepared.workspace_id(), carried],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(database_error)?;
            let eligible = matches!(&found, Some((session, state, provider, order))
                if session == prepared.session_id() && state == "succeeded"
                    && provider == "codex" && *order > previous);
            guard(
                eligible,
                "context_unavailable",
                "Carried runs must be earlier succeeded Codex runs of this session.",
            )?;
            previous = found.map_or(previous, |f| f.3);
        }
        let class = receipt_class(&transaction, prepared.workspace_id(), false)?;
        let (total,queued):(u64,u64)=transaction.query_row(
            "SELECT COUNT(*),COALESCE(SUM(CASE WHEN state='queued' THEN 1 ELSE 0 END),0) FROM runs WHERE workspace_id=?1",
            [prepared.workspace_id()],|r|Ok((r.get(0)?,r.get(1)?))).map_err(database_error)?;
        guard(
            total < WORKSPACE_RUNS as u64,
            "capacity_exceeded",
            "Workspace run-history capacity has been reached.",
        )?;
        guard(
            queued < QUEUED_RUNS as u64,
            "queue_full",
            "The run queue is full.",
        )?;
        let order:Option<u64>=transaction.query_row(
            "UPDATE workspaces SET last_enqueue=last_enqueue+1 WHERE workspace_id=?1 AND last_enqueue<?2 RETURNING last_enqueue",
            params![prepared.workspace_id(),JS_MAX_INTEGER],|r|r.get(0)).optional().map_err(database_error)?;
        let created = prepared.materialize(
            seed,
            order.ok_or_else(|| {
                error(
                    "capacity_exceeded",
                    "Workspace enqueue-order capacity has been reached.",
                )
            })?,
        )?;
        let result = created.snapshot;
        let h = created.event.header();
        let collision:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE run_id=?1) OR EXISTS(SELECT 1 FROM events WHERE event_id=?2)",
            params![result.run_id,h.event_id],|r|r.get(0)).map_err(database_error)?;
        guard(
            !collision,
            "state_conflict",
            "Run or event identity is already committed.",
        )?;
        let input = prepared.input();
        transaction.execute("INSERT INTO runs(run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,prompt,scenario,
            ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count,provider_id,profile_id,context_json)
            VALUES (?1,?2,?3,'queued',1,?4,?5,?6,?7,1,0,0,0,0,0,?8,?9,?10)",
            params![result.run_id,result.workspace_id,result.session_id,result.enqueue_order,encode(&result)?,input.prompt(),
                input.scenario().map(|s|s.as_str()),input.provider().as_str(),input.profile().map(|p|p.as_str()),input.context_json()]).map_err(database_error)?;
        transaction.execute("INSERT INTO events(event_id,workspace_id,session_id,run_id,seq,event_json) VALUES (?1,?2,?3,?4,1,?5)",
            params![h.event_id,h.workspace_id,h.session_id,h.run_id,encode(&created.event)?]).map_err(database_error)?;
        receipt(
            &transaction,
            prepared.workspace_id(),
            prepared.command(),
            prepared.request_id(),
            prepared.arguments(),
            &result,
            class,
        )?;
        transaction.commit().map_err(database_error)?;
        Ok(changed(result, vec![created.event]))
    }
}
