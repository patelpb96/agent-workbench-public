//! v2 journal writes: compare-and-append, cancellation, layout save and startup
//! recovery. Free functions over a writer transaction so the copy rehearsal and
//! the future fresh v2 owner share one implementation. Shared SQL owns sequence/
//! state preconditions, one-active-slot/FIFO, global event identity, cumulative
//! per-run budgets and receipts; adapter/profile semantics stay in typed domain
//! policy. Nothing here opens originals, creates dispatch arms, signals workers,
//! changes availability or claims readiness.
use super::*;
use crate::execution_v2::{
    self as v2,
    ports::{AppendBatch, JournalWriter, MutationCommit, RecoveryReport},
    stored::SnapshotVersion,
    validation as policy,
};
use admission_v2::AdmissionRehearsal;
use audit_v2::Materialized;

fn terminal(state: v2::RunState) -> bool {
    matches!(
        state,
        v2::RunState::Succeeded
            | v2::RunState::Failed
            | v2::RunState::Cancelled
            | v2::RunState::Interrupted
    )
}
fn active(state: v2::RunState) -> bool {
    state != v2::RunState::Queued && !terminal(state)
}
fn conflict(ok: bool, message: &str) -> StoreResult<()> {
    require_store(ok, "state_conflict", message)
}
fn not_found() -> Error {
    error(
        "run_not_found",
        "Run was not found in the selected workspace.",
    )
}
fn commit<T>(result: T, committed_events: Vec<v2::RunEvent>) -> MutationCommit<T> {
    MutationCommit {
        result,
        committed_events,
        replayed: false,
    }
}

/// Called only inside a writer transaction, with `rows` materialized in that
/// same transaction. Every validation precedes the first write.
pub(super) fn append(
    conn: &Connection,
    rows: &Materialized,
    batch: &AppendBatch,
) -> StoreResult<v2::RunSnapshot> {
    validate_id(&batch.workspace_id)?;
    validate_id(&batch.run_id)?;
    require_store(
        rows.snapshot.workspace.workspace_id == batch.workspace_id,
        "workspace_not_found",
        "Workspace was not found.",
    )?;
    let run = rows.runs.get(&batch.run_id).ok_or_else(not_found)?;
    // V1 runs stay version-homogeneous, including archived/recovered history.
    conflict(
        matches!(run.original.original(), SnapshotVersion::V2(_)),
        "Legacy run history cannot acquire v2 append events.",
    )?;
    let current = &run.snapshot;
    conflict(
        batch.expected_last_seq == current.last_seq && batch.expected_state == current.state,
        "Run state or sequence changed before append.",
    )?;
    policy::validate_append(current, batch)?;
    // A terminal final snapshot cannot hide an intermediate slot claim.
    let claims_slot = active(current.state)
        || batch.events.iter().any(|event| {
            matches!(event, v2::RunEvent::RunStateChanged { payload, .. } if active(payload.to))
        });
    if claims_slot {
        conflict(
            !rows
                .snapshot
                .runs
                .iter()
                .any(|r| r.run_id != current.run_id && active(r.state)),
            "Another run owns the execution slot.",
        )?;
    }
    if current.state == v2::RunState::Queued
        && batch.events.iter().any(|event| {
            matches!(event, v2::RunEvent::RunStateChanged { payload, .. }
                if payload.from == v2::RunState::Queued && payload.to == v2::RunState::Starting)
        })
    {
        conflict(
            !rows.snapshot.runs.iter().any(|r| {
                r.state == v2::RunState::Queued && r.enqueue_order < current.enqueue_order
            }),
            "An earlier queued run must be admitted first.",
        )?;
    }
    // Cumulative budgets continue from COMMITTED counters, never caller data.
    let [mut ordinary, mut reserved, mut text, mut mask, mut cancels, mut terminals] = run.counts;
    for event in &batch.events {
        let found: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE event_id=?1)",
                [event.header().event_id],
                |r| r.get(0),
            )
            .map_err(database_error)?;
        conflict(!found, "Event identity is already committed.")?;
        if policy::is_reserved_event(event) {
            reserved += 1;
            match event {
                v2::RunEvent::OutputTruncated { payload, .. } => {
                    let bit = match payload.scope {
                        v2::TruncationScope::PendingBytes => 1,
                        v2::TruncationScope::PersistedTextBytes => 2,
                        v2::TruncationScope::EventCount => 4,
                    };
                    conflict(mask & bit == 0, "Truncation scope was already committed.")?;
                    mask |= bit;
                }
                v2::RunEvent::RunRecovered { .. } => terminals += 1,
                v2::RunEvent::RunStateChanged { payload, .. }
                    if payload.to == v2::RunState::CancelRequested =>
                {
                    cancels += 1
                }
                v2::RunEvent::RunStateChanged { payload, .. } if terminal(payload.to) => {
                    conflict(
                        payload.to != v2::RunState::Succeeded || mask == 0,
                        "A truncated run cannot succeed.",
                    )?;
                    terminals += 1;
                }
                _ => return Err(unavailable()),
            }
        } else {
            conflict(
                mask == 0,
                "Ordinary events cannot follow committed truncation.",
            )?;
            ordinary += 1;
            if let v2::RunEvent::TextDelta { payload, .. } = event {
                text += payload.text.len() as u64;
            }
        }
        require_store(
            ordinary <= ORDINARY_EVENTS as u64 && text <= PERSISTED_TEXT_BYTES as u64,
            "capacity_exceeded",
            "Run output requires an atomic truncation and stop decision.",
        )?;
        require_store(
            reserved <= RESERVED_EVENTS as u64 && cancels <= 1 && terminals <= 1,
            "storage_unavailable",
            "Run control-event capacity is inconsistent.",
        )?;
    }
    for event in &batch.events {
        let h = event.header();
        conn.execute(
            "INSERT INTO events(event_id,workspace_id,session_id,run_id,seq,event_json) VALUES (?1,?2,?3,?4,?5,?6)",
            params![h.event_id, h.workspace_id, h.session_id, h.run_id, h.seq, encode(event)?],
        )
        .map_err(database_error)?;
    }
    let next = &batch.next_snapshot;
    let updated = conn
        .execute(
            "UPDATE runs SET state=?1,last_seq=?2,snapshot_json=?3,ordinary_count=?4,reserved_count=?5,
            text_bytes=?6,truncation_mask=?7,cancel_count=?8,terminal_count=?9
         WHERE workspace_id=?10 AND run_id=?11 AND last_seq=?12 AND state=?13",
            params![
                next.state.as_str(),
                next.last_seq,
                encode(next)?,
                ordinary,
                reserved,
                text,
                mask,
                cancels,
                terminals,
                batch.workspace_id,
                batch.run_id,
                current.last_seq,
                current.state.as_str()
            ],
        )
        .map_err(database_error)?;
    conflict(
        updated == 1,
        "Run changed before its snapshot was committed.",
    )?;
    Ok(next.clone())
}

fn cancel(
    conn: &Connection,
    request: &v2::CancelRunRequest,
    mutation: Option<AppendBatch>,
) -> StoreResult<MutationCommit<v2::RunSnapshot>> {
    policy::validate_cancel(request)?;
    let arguments = canonical_arguments(request)?;
    let rows = read_v2::materialized(conn, &request.workspace_id)?;
    let run = rows.runs.get(&request.run_id).ok_or_else(not_found)?;
    // Receipt lookup precedes state preconditions: exact replay always returns
    // the immutable original result with no events, whatever happened since.
    if let Some(found) = admission_v2::replay::<v2::RunSnapshot>(
        conn,
        &request.workspace_id,
        v2::CommandName::CancelRunV2,
        &request.request_id,
        &arguments,
    )? {
        policy::validate_snapshot(&found.result).map_err(|_| unavailable())?;
        require_store(
            found.result.workspace_id == request.workspace_id
                && found.result.run_id == request.run_id,
            "storage_unavailable",
            "Stored receipt ownership is inconsistent.",
        )?;
        return Ok(found);
    }
    let current = &run.snapshot;
    let target = match current.state {
        v2::RunState::Queued => Some(v2::RunState::Cancelled),
        v2::RunState::Starting | v2::RunState::Running | v2::RunState::WaitingInput => {
            Some(v2::RunState::CancelRequested)
        }
        state if state == v2::RunState::CancelRequested || terminal(state) => None,
        _ => return Err(unavailable()),
    };
    match (target, mutation) {
        // No-op on cancel_requested/terminal (including archived v1 history):
        // ordinary receipt of the current projection; never emergency capacity.
        (None, None) => {
            let class = receipt_class(conn, &request.workspace_id, false)?;
            let result = current.clone();
            admission_v2::receipt(
                conn,
                &request.workspace_id,
                v2::CommandName::CancelRunV2,
                &request.request_id,
                &arguments,
                &result,
                class,
            )?;
            Ok(commit(result, Vec::new()))
        }
        (Some(target), Some(batch)) => {
            conflict(
                batch.workspace_id == request.workspace_id
                    && batch.run_id == request.run_id
                    && batch.events.len() == 1,
                "Cancellation must contain exactly its required transition.",
            )?;
            let correct = matches!(&batch.events[0], v2::RunEvent::RunStateChanged { source, payload, .. }
                if *source == v2::EventSource::System && payload.from == current.state
                    && payload.to == target && payload.error.is_none());
            conflict(
                correct,
                "Cancellation proposed an unrelated event or transition.",
            )?;
            // Only this first state-changing cancellation may use emergency
            // capacity; capacity is checked before the append writes.
            let class = receipt_class(conn, &request.workspace_id, true)?;
            let result = append(conn, &rows, &batch)?;
            admission_v2::receipt(
                conn,
                &request.workspace_id,
                v2::CommandName::CancelRunV2,
                &request.request_id,
                &arguments,
                &result,
                class,
            )?;
            Ok(commit(result, batch.events))
        }
        _ => Err(error(
            "state_conflict",
            "Cancellation proposal does not match the current run state.",
        )),
    }
}

fn save_layout(
    conn: &Connection,
    request: &v2::SaveLayoutRequest,
) -> StoreResult<MutationCommit<v2::Layout>> {
    validate_id(&request.workspace_id)?;
    let rows = read_v2::materialized(conn, &request.workspace_id)?;
    let sessions = &rows.snapshot.sessions;
    policy::validate_save_layout(request, sessions)?;
    let arguments = canonical_arguments(request)?;
    if let Some(found) = admission_v2::replay::<v2::Layout>(
        conn,
        &request.workspace_id,
        v2::CommandName::SaveLayoutV2,
        &request.request_id,
        &arguments,
    )? {
        policy::validate_layout(&found.result, &request.workspace_id, sessions)
            .map_err(|_| unavailable())?;
        return Ok(found);
    }
    // Unsupported/invalid stored layouts stay preserved: no implicit overwrite.
    if let Some(warning) = &rows.snapshot.layout_warning {
        return Err(Error {
            code: warning.code.clone(),
            user_message: warning.user_message.clone(),
            recoverable: warning.recoverable,
        });
    }
    require_store(
        rows.revision == request.expected_revision,
        "layout_revision_conflict",
        "Layout revision has changed.",
    )?;
    require_store(
        rows.revision < JS_MAX_INTEGER,
        "capacity_exceeded",
        "Layout revision capacity has been reached.",
    )?;
    let class = receipt_class(conn, &request.workspace_id, false)?;
    let mut result = request.layout.clone();
    result.revision += 1;
    policy::validate_layout(&result, &request.workspace_id, sessions)?;
    let updated = conn
        .execute(
            "UPDATE workspaces SET layout_json=?1,layout_revision=?2 WHERE workspace_id=?3 AND layout_revision=?4",
            params![encode(&result)?, result.revision, request.workspace_id, request.expected_revision],
        )
        .map_err(database_error)?;
    require_store(
        updated == 1,
        "layout_revision_conflict",
        "Layout revision has changed.",
    )?;
    admission_v2::receipt(
        conn,
        &request.workspace_id,
        v2::CommandName::SaveLayoutV2,
        &request.request_id,
        &arguments,
        &result,
        class,
    )?;
    Ok(commit(result, Vec::new()))
}

/// Interrupt every nonterminal v2 run, INCLUDING queued ones, once at startup,
/// before any admission, peek or dispatch: queued consent/arms never survive a
/// restart (v1 parity), so a surviving queued run could only wedge FIFO or be
/// dispatched without a fresh arm. The owner (not this function) guarantees exactly-once-per-open so
/// later work in the same open store is never interrupted. Legacy v1 runs are
/// never extended; unfinished v1 work makes the store unavailable instead.
pub(super) fn recover(
    conn: &Connection,
    now: &str,
    ids: &mut dyn IdSource,
) -> StoreResult<RecoveryReport> {
    validate_timestamp(now)?;
    let workspace_id = audit_v2::read_materialized(conn, None)?
        .snapshot
        .workspace
        .workspace_id;
    let mut report = RecoveryReport::default();
    loop {
        // Re-materialize per run so each append sees the previous commit's slot.
        let rows = read_v2::materialized(conn, &workspace_id)?;
        let Some(current) = rows
            .snapshot
            .runs
            .iter()
            .find(|r| !terminal(r.state))
            .cloned()
        else {
            break;
        };
        let previous_state: v2::ActiveState =
            serde_json::from_value(serde_json::Value::String(current.state.as_str().into()))
                .map_err(|_| unavailable())?;
        let event = v2::RunEvent::from_parts(
            v2::EventHeader {
                contract_version: 2,
                event_id: ids.next_id(),
                workspace_id: workspace_id.clone(),
                session_id: current.session_id.clone(),
                run_id: current.run_id.clone(),
                provider_id: current.provider_id,
                source: v2::EventSource::System,
                seq: current.last_seq + 1,
                occurred_at: now.into(),
            },
            v2::EventPayload::RunRecovered(v2::RunRecoveredPayload {
                previous_state,
                state: "interrupted".into(),
            }),
        );
        let next_snapshot = policy::apply_event(&current, &event)?;
        let batch = AppendBatch {
            workspace_id: workspace_id.clone(),
            run_id: current.run_id.clone(),
            expected_last_seq: current.last_seq,
            expected_state: current.state,
            events: vec![event],
            next_snapshot,
        };
        report.interrupted_runs.push(append(conn, &rows, &batch)?);
        report.committed_events.extend(batch.events);
    }
    Ok(report)
}

fn write<T>(
    conn: &mut Connection,
    body: impl FnOnce(&Connection) -> StoreResult<MutationCommit<T>>,
) -> StoreResult<MutationCommit<T>> {
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let result = body(&transaction)?;
    // Replays commit nothing; committing their empty transaction is harmless.
    transaction.commit().map_err(database_error)?;
    Ok(result)
}

/// Run `recover` in one writer transaction. Production: the fresh v2 store
/// calls this exactly once per open, before it reports ready.
pub(super) fn recover_in(
    conn: &mut Connection,
    now: &str,
    ids: &mut dyn IdSource,
) -> StoreResult<RecoveryReport> {
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let report = recover(&transaction, now, ids)?;
    transaction.commit().map_err(database_error)?;
    Ok(report)
}

impl AdmissionRehearsal {
    /// Test-only: the copy rehearsal finished recovery at construction.
    #[cfg(test)]
    pub(super) fn recover_for_test(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> StoreResult<RecoveryReport> {
        recover_in(self.journal_connection()?, now, ids)
    }
}

impl<J: JournalConnection> JournalWriter for J {
    fn append_batch(&mut self, batch: AppendBatch) -> StoreResult<MutationCommit<v2::RunSnapshot>> {
        validate_id(&batch.workspace_id)?;
        let saved = write(self.journal_connection()?, |conn| {
            let rows = read_v2::materialized(conn, &batch.workspace_id)?;
            Ok(commit(append(conn, &rows, &batch)?, Vec::new()))
        })?;
        Ok(commit(saved.result, batch.events))
    }
    fn cancel_run(
        &mut self,
        request: &v2::CancelRunRequest,
        mutation: Option<AppendBatch>,
    ) -> StoreResult<MutationCommit<v2::RunSnapshot>> {
        write(self.journal_connection()?, |conn| {
            cancel(conn, request, mutation)
        })
    }
    fn save_layout(
        &mut self,
        request: &v2::SaveLayoutRequest,
    ) -> StoreResult<MutationCommit<v2::Layout>> {
        write(self.journal_connection()?, |conn| {
            save_layout(conn, request)
        })
    }
}
