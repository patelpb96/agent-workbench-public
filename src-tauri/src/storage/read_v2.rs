//! Bounded direct reads on the already-audited disposable COPY. No schema/audit
//! reconstruction on refresh, no original opener, no writes or dispatch arms.
use super::*;
use crate::execution_v2::{
    self as v2,
    ports::{JournalReader, QueuedRun},
    stored::{EventVersion, SnapshotVersion, StoredRunEvent},
    validation as policy,
};

fn terminal(state: v2::RunState) -> bool {
    matches!(
        state,
        v2::RunState::Succeeded
            | v2::RunState::Failed
            | v2::RunState::Cancelled
            | v2::RunState::Interrupted
    )
}
pub(super) fn materialized(
    conn: &Connection,
    workspace_id: &str,
) -> StoreResult<audit_v2::Materialized> {
    let rows = audit_v2::read_materialized(conn, Some(workspace_id))?;
    // Post-recovery copy reads must not offer unfinished legacy work.
    require_store(
        rows.runs.values().all(|r| {
            !matches!(r.original.original(), SnapshotVersion::V1(_)) || terminal(r.snapshot.state)
        }),
        "storage_unavailable",
        "Legacy recovery must finish before journal reads.",
    )?;
    Ok(rows)
}
fn queued(run: audit_v2::Run) -> QueuedRun {
    QueuedRun {
        snapshot: run.snapshot,
        input: run.input,
    }
}
fn unavailable_history() -> Error {
    error(
        "event_history_unavailable",
        "The requested committed event range is unavailable.",
    )
}
impl<J: JournalConnection> JournalReader for J {
    fn workspace_snapshot(
        &mut self,
        request: &v2::WorkspaceRequest,
    ) -> StoreResult<v2::WorkspaceSnapshot> {
        policy::validate_workspace_request(request)?;
        let transaction = self
            .journal_connection()?
            .transaction()
            .map_err(database_error)?;
        let value = materialized(&transaction, &request.workspace_id)?.snapshot;
        transaction.commit().map_err(database_error)?;
        Ok(value)
    }
    fn get_run(&mut self, workspace_id: &str, run_id: &str) -> StoreResult<QueuedRun> {
        validate_id(workspace_id)?;
        validate_id(run_id)?;
        let transaction = self
            .journal_connection()?
            .transaction()
            .map_err(database_error)?;
        let mut rows = materialized(&transaction, workspace_id)?;
        let value = rows.runs.remove(run_id).ok_or_else(|| {
            error(
                "run_not_found",
                "Run was not found in the selected workspace.",
            )
        })?;
        transaction.commit().map_err(database_error)?;
        Ok(queued(value))
    }
    fn next_queued(&mut self, workspace_id: &str) -> StoreResult<Option<QueuedRun>> {
        validate_id(workspace_id)?;
        let transaction = self
            .journal_connection()?
            .transaction()
            .map_err(database_error)?;
        let mut rows = materialized(&transaction, workspace_id)?;
        let value = if rows
            .snapshot
            .runs
            .iter()
            .any(|r| r.state != v2::RunState::Queued && !terminal(r.state))
        {
            None
        } else {
            let id = rows
                .snapshot
                .runs
                .iter()
                .filter(|r| r.state == v2::RunState::Queued)
                .min_by_key(|r| r.enqueue_order)
                .map(|r| r.run_id.clone());
            id.map(|id| rows.runs.remove(&id).map(queued).ok_or_else(unavailable))
                .transpose()?
        };
        transaction.commit().map_err(database_error)?;
        Ok(value)
    }
    fn list_run_events(&mut self, request: &v2::EventPageRequest) -> StoreResult<v2::EventPage> {
        policy::validate_event_page(request)?;
        let transaction = self
            .journal_connection()?
            .transaction()
            .map_err(database_error)?;
        let mut rows = materialized(&transaction, &request.workspace_id)?;
        let run = rows.runs.remove(&request.run_id).ok_or_else(|| {
            error(
                "run_not_found",
                "Run was not found in the selected workspace.",
            )
        })?;
        let through = request.through_seq.unwrap_or(run.snapshot.last_seq);
        require_store(
            through <= run.snapshot.last_seq && request.after_seq <= through,
            "validation_failed",
            "Event cursor exceeds committed history or its pinned high-water mark.",
        )?;
        let mut events = Vec::new();
        {
            let mut statement = transaction
                .prepare(
                    "SELECT event_id,workspace_id,session_id,run_id,seq,event_json FROM events
                WHERE workspace_id=?1 AND run_id=?2 AND seq>?3 AND seq<=?4 ORDER BY seq LIMIT ?5",
                )
                .map_err(database_error)?;
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
                let stored = StoredRunEvent::decode(
                    audit_v2::text(row, 5, v2::EVENT_BYTES).map_err(|_| unavailable_history())?,
                )
                .map_err(|_| unavailable_history())?;
                require_store(
                    matches!(
                        (run.original.original(), stored.original()),
                        (SnapshotVersion::V1(_), EventVersion::V1(_))
                            | (SnapshotVersion::V2(_), EventVersion::V2(_))
                    ),
                    "event_history_unavailable",
                    "Stored event version does not match its original run.",
                )?;
                let event = stored.projected().map_err(|_| unavailable_history())?;
                let h = event.header();
                let seq: u64 = row.get(4).map_err(database_error)?;
                require_store(
                    audit_v2::text(row, 0, 36)? == h.event_id
                        && audit_v2::text(row, 1, 36)? == h.workspace_id
                        && audit_v2::text(row, 2, 36)? == h.session_id
                        && audit_v2::text(row, 3, 36)? == h.run_id
                        && seq == h.seq
                        && seq == request.after_seq + events.len() as u64 + 1
                        && h.workspace_id == request.workspace_id
                        && h.run_id == request.run_id
                        && h.session_id == run.snapshot.session_id
                        && h.provider_id == run.snapshot.provider_id
                        && (seq != 1 || matches!(event, v2::RunEvent::RunCreated { .. })),
                    "event_history_unavailable",
                    "Committed event identities or sequences are inconsistent.",
                )?;
                events.push(event);
            }
        }
        require_store(
            events.len() as u64 == (through - request.after_seq).min(request.limit),
            "event_history_unavailable",
            "The requested committed event range is incomplete.",
        )?;
        let next = request.after_seq + events.len() as u64;
        let value = v2::EventPage {
            contract_version: 2,
            workspace_id: request.workspace_id.clone(),
            run_id: request.run_id.clone(),
            events,
            last_seq: run.snapshot.last_seq,
            through_seq: through,
            next_after_seq: next,
            has_more: next < through,
        };
        transaction.commit().map_err(database_error)?;
        Ok(value)
    }
}
