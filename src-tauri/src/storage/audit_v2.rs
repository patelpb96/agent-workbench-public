//! Read-only mixed-version validation on an already-owned SQLite COPY.
//! No opener, migration permission, admission, or production wiring lives here.
use super::*;
use crate::execution_v2::{self as v2, ports::QueuedInput, stored::*, validation as policy};
use std::collections::BTreeMap;

const JSON_BYTES: usize = 1024 * 1024;

pub(super) fn text<'a>(
    row: &'a rusqlite::Row<'_>,
    column: usize,
    limit: usize,
) -> StoreResult<&'a str> {
    match row.get_ref(column).map_err(database_error)? {
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= limit => {
            std::str::from_utf8(bytes).map_err(|_| unavailable())
        }
        _ => Err(unavailable()),
    }
}
fn check(ok: bool) -> StoreResult<()> {
    require_store(
        ok,
        "storage_unavailable",
        "Stored mixed-version execution data is inconsistent.",
    )
}
fn valid<T>(result: StoreResult<T>) -> StoreResult<T> {
    result.map_err(|_| unavailable())
}
fn convert<T: DeserializeOwned>(value: &impl Serialize) -> StoreResult<T> {
    decode(&encode(value)?)
}
fn canonical(request: &impl Serialize, raw: &str) -> StoreResult<()> {
    check(canonical_arguments(request)? == raw)
}

pub(super) struct Run {
    pub original: StoredRunSnapshot,
    pub snapshot: v2::RunSnapshot,
    pub input: QueuedInput,
    pub counts: [u64; 6], // ordinary, reserved, text bytes, mask, cancels, terminals
}
// No Debug/Serialize: holds native private inputs separately from public DTOs.
pub(super) struct Materialized {
    pub snapshot: v2::WorkspaceSnapshot,
    pub runs: BTreeMap<String, Run>,
    sessions: BTreeMap<String, StoredSession>,
    pub revision: u64,
}
fn nullable_text<'a>(
    row: &'a rusqlite::Row<'_>,
    column: usize,
    limit: usize,
) -> StoreResult<Option<&'a str>> {
    match row.get_ref(column).map_err(database_error)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        _ => text(row, column, limit).map(Some),
    }
}
struct Receipt {
    snapshot: StoredRunSnapshot,
    emergency: bool,
    // Only a NEW v2 no-op cancellation of an archived terminal v1 run may
    // compare a projected result. Historical v1 receipts NEVER project.
    projected_legacy_cancel: bool,
}
type Prefixes = BTreeMap<u64, Vec<Receipt>>;
type Receipts = BTreeMap<String, Prefixes>;

/// Caller must hold exclusive native ownership, and use a disposable copy.
/// This is not a replacement for original-family preflight/confirmation gates.
/// Returns only a non-dispatchable read projection. No stored JSON is rewritten.
pub(super) fn audit_copy(conn: &Connection) -> StoreResult<Option<v2::WorkspaceSnapshot>> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(database_error)?;
    check(version == 2 || version == 3)?;
    let expected = Connection::open_in_memory().map_err(database_error)?;
    expected.execute_batch(SCHEMA).map_err(database_error)?;
    let mut expected = expected;
    upgrade_preview::migrate_copy(&mut expected, |_| Ok(()))?;
    if version == 3 {
        upgrade_preview::migrate_with(&mut expected, store_v2::MIGRATION_V3, |_| Ok(()))?;
    }
    check(schema_signature(conn)? == schema_signature(&expected)?)?;
    let integrity: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .map_err(database_error)?
        .query_map([], |r| r.get(0))
        .map_err(database_error)?
        .collect::<Result<_, _>>()
        .map_err(database_error)?;
    check(integrity == ["ok"])?;
    let fk: Option<i64> = conn
        .query_row("SELECT 1 FROM pragma_foreign_key_check LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(database_error)?;
    check(fk.is_none())?;
    let (workspaces, sessions_count, runs_count, ordinary, emergency): (u64, u64, u64, u64, u64) =
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM workspaces), (SELECT COUNT(*) FROM sessions),
        (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM receipts WHERE emergency=0),
        (SELECT COUNT(*) FROM receipts WHERE emergency=1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(database_error)?;
    check(
        workspaces <= 1
            && sessions_count <= WORKSPACE_SESSIONS as u64
            && runs_count <= WORKSPACE_RUNS as u64
            && ordinary <= COMMAND_RECEIPTS as u64
            && emergency <= CANCEL_RECEIPTS as u64,
    )?;
    if workspaces == 0 {
        return Ok(None);
    }
    let rows = read_materialized(conn, None)?;
    let mut receipts = audit_receipts(
        conn,
        &rows.snapshot,
        &rows.sessions,
        &rows.runs,
        rows.revision,
    )?;
    for (id, run) in &rows.runs {
        audit_history(conn, run, &mut receipts.remove(id).unwrap_or_default())?;
    }
    check(receipts.is_empty())?;
    Ok(Some(rows.snapshot))
}

/// Shared bounded materialized-row loader. No schema construction, integrity/FK
/// scan, receipt scan or event replay. Only for an already-audited owned store;
/// this is NOT an opener/admission gate or proof of complete history integrity.
pub(super) fn read_materialized(
    conn: &Connection,
    selected: Option<&str>,
) -> StoreResult<Materialized> {
    conn.query_row(
        "SELECT record_json,layout_json FROM workspaces",
        [],
        |row| {
            // Bound borrowed SQLite text before workspace() allocates/parses it.
            Ok(text(row, 0, 16_384).and_then(|_| text(row, 1, JSON_BYTES).map(|_| ())))
        },
    )
    .map_err(database_error)??;
    let workspace = workspace(conn, selected)?;
    let id = &workspace.record.workspace_id;
    let mut sessions = BTreeMap::new();
    let mut statement = conn
        .prepare("SELECT session_id,workspace_id,snapshot_json FROM sessions ORDER BY session_id LIMIT ?1")
        .map_err(database_error)?;
    let mut rows = statement
        .query([WORKSPACE_SESSIONS + 1])
        .map_err(database_error)?;
    while let Some(row) = rows.next().map_err(database_error)? {
        check(sessions.len() < WORKSPACE_SESSIONS)?;
        let stored = valid(StoredSession::decode(text(row, 2, 16_384)?))?;
        let projected = valid(stored.projected())?;
        check(
            projected.session_id == text(row, 0, 36)?
                && projected.workspace_id == *id
                && text(row, 1, 36)? == id,
        )?;
        sessions.insert(projected.session_id.clone(), stored);
    }
    let projected_sessions = sessions
        .values()
        .map(StoredSession::projected)
        .collect::<StoreResult<Vec<_>>>()?;
    let mut runs = BTreeMap::new();
    // Storage 2 (legacy-preview copies) has no context column; storage 3 does.
    let schema_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(database_error)?;
    let context_column = if schema_version >= 3 { "context_json" } else { "NULL" };
    let mut statement = conn.prepare(&format!("SELECT run_id,workspace_id,session_id,state,last_seq,enqueue_order,snapshot_json,
        prompt,scenario,provider_id,profile_id,ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count,{context_column}
        FROM runs ORDER BY enqueue_order LIMIT ?1")).map_err(database_error)?;
    let mut rows = statement
        .query([WORKSPACE_RUNS + 1])
        .map_err(database_error)?;
    while let Some(row) = rows.next().map_err(database_error)? {
        check(runs.len() < WORKSPACE_RUNS)?;
        let original = valid(StoredRunSnapshot::decode(text(row, 6, 65_536)?))?;
        let snapshot = valid(original.projected())?;
        let scenario = nullable_text(row, 8, 16)?;
        let profile = nullable_text(row, 10, 32)?;
        let prompt = text(row, 7, v2::PROMPT_BYTES)?;
        check(
            snapshot.run_id == text(row, 0, 36)?
                && snapshot.workspace_id == *id
                && text(row, 1, 36)? == id
                && snapshot.session_id == text(row, 2, 36)?
                && snapshot.state.as_str() == text(row, 3, 32)?
                && snapshot.last_seq == row.get::<_, u64>(4).map_err(database_error)?
                && snapshot.enqueue_order == row.get::<_, u64>(5).map_err(database_error)?
                && snapshot.enqueue_order <= workspace.last_enqueue
                && snapshot.provider_id.as_str() == text(row, 9, 16)?
                && snapshot.profile_id.map(|p| p.as_str()) == profile,
        )?;
        let session = sessions.get(&snapshot.session_id).ok_or_else(unavailable)?;
        check(
            !matches!(original.original(), SnapshotVersion::V1(_))
                || matches!(session.original(), SessionVersion::V1(_)),
        )?;
        let session = valid(session.projected())?;
        let context = nullable_text(row, 17, 512)?;
        let input = valid(QueuedInput::from_stored(
            &snapshot, &session, prompt, scenario, profile, context,
        ))?;
        let mut counts = [0; 6];
        for (offset, count) in counts.iter_mut().enumerate() {
            *count = row.get(11 + offset).map_err(database_error)?;
        }
        let [ordinary, reserved, text, mask, cancellations, terminals] = counts;
        check(
            ordinary > 0
                && ordinary <= ORDINARY_EVENTS as u64
                && reserved <= RESERVED_EVENTS as u64
                && text <= PERSISTED_TEXT_BYTES as u64
                && mask <= 7
                && cancellations <= 1
                && terminals <= 1
                && ordinary + reserved == snapshot.last_seq
                && reserved == mask.count_ones() as u64 + cancellations + terminals
                && terminals == u64::from(terminal(snapshot.state))
                && (snapshot.state != v2::RunState::CancelRequested || cancellations == 1)
                && (mask == 0
                    || matches!(
                        snapshot.state,
                        v2::RunState::CancelRequested
                            | v2::RunState::Cancelled
                            | v2::RunState::Failed
                            | v2::RunState::Interrupted
                    )),
        )?;
        runs.insert(
            snapshot.run_id.clone(),
            Run {
                original,
                snapshot,
                input,
                counts,
            },
        );
    }
    // Carried context must name earlier, succeeded Codex runs of the same session.
    for run in runs.values() {
        let mut previous = 0;
        for carried in run.input.context_run_ids() {
            let source = runs.get(carried).ok_or_else(unavailable)?;
            check(
                source.snapshot.session_id == run.snapshot.session_id
                    && source.snapshot.state == v2::RunState::Succeeded
                    && source.snapshot.provider_id == v2::ProviderId::Codex
                    && source.snapshot.enqueue_order > previous
                    && source.snapshot.enqueue_order < run.snapshot.enqueue_order,
            )?;
            previous = source.snapshot.enqueue_order;
        }
    }
    let (layout, layout_warning) = match decode::<v2::Layout>(&workspace.layout_json) {
        Ok(layout)
            if layout.revision == workspace.revision
                && policy::validate_layout(&layout, id, &projected_sessions).is_ok() =>
        {
            (Some(layout), None)
        }
        _ => {
            let value: serde_json::Value =
                serde_json::from_str(&workspace.layout_json).unwrap_or_default();
            let unsupported = value
                .get("layout_version")
                .and_then(|v| v.as_u64())
                .is_some_and(|v| v != 1);
            (None, Some(v2::Error { code: if unsupported { "unsupported_layout_version" } else { "layout_invalid" }.into(),
                user_message: if unsupported {
                    "Stored layout version is unsupported; original data is preserved and saving is disabled."
                } else {
                    "Stored layout is invalid; original data is preserved and saving is disabled."
                }.into(), recoverable: true }))
        }
    };
    let mut projected_runs = runs
        .values()
        .map(|r| r.snapshot.clone())
        .collect::<Vec<_>>();
    projected_runs.sort_by_key(|r| r.enqueue_order);
    let snapshot = v2::WorkspaceSnapshot {
        contract_version: 2,
        workspace: convert(&workspace.record)?,
        sessions: projected_sessions,
        runs: projected_runs,
        layout,
        layout_warning,
        execution_available: false,
        codex_availability: v2::CodexAvailability::NotConfigured,
    };
    valid(policy::validate_workspace(&snapshot))?;
    Ok(Materialized {
        snapshot,
        runs,
        sessions,
        revision: workspace.revision,
    })
}

fn remember(
    receipts: &mut Receipts,
    snapshot: StoredRunSnapshot,
    emergency: bool,
    projected_legacy_cancel: bool,
) -> StoreResult<()> {
    let projected = valid(snapshot.projected())?;
    let prefixes = receipts.entry(projected.run_id).or_default();
    // Exactly one request can perform a run's first cancellation. Replayed or
    // later no-op requests cannot mint another emergency receipt at that prefix.
    check(!emergency || !prefixes.values().flatten().any(|r| r.emergency))?;
    let entries = prefixes.entry(projected.last_seq).or_default();
    // Legacy and new v2 no-op cancels may share a prefix. Retain each version;
    // merge only identical original results, never their projections.
    if let Some(previous) = entries.iter_mut().find(|r| {
        same_original_prefix(&r.snapshot, &snapshot)
            && r.projected_legacy_cancel == projected_legacy_cancel
    }) {
        previous.emergency |= emergency;
    } else {
        entries.push(Receipt {
            snapshot,
            emergency,
            projected_legacy_cancel,
        });
    }
    Ok(())
}

fn audit_receipts(
    conn: &Connection,
    workspace: &v2::WorkspaceSnapshot,
    sessions: &BTreeMap<String, StoredSession>,
    runs: &BTreeMap<String, Run>,
    revision: u64,
) -> StoreResult<Receipts> {
    let mut receipts = Receipts::new();
    let mut statement = conn.prepare("SELECT workspace_id,command_name,request_id,arguments_json,result_json,emergency FROM receipts").map_err(database_error)?;
    let mut rows = statement.query([]).map_err(database_error)?;
    while let Some(row) = rows.next().map_err(database_error)? {
        let id = text(row, 0, 36)?;
        let command = text(row, 1, 32)?;
        let request_id = text(row, 2, 36)?;
        let args = text(row, 3, JSON_BYTES)?;
        let result = text(row, 4, JSON_BYTES)?;
        let emergency: i64 = row.get(5).map_err(database_error)?;
        valid(validate_id(request_id))?;
        let arguments: serde_json::Value = decode(args)?;
        check(
            id == workspace.workspace.workspace_id
                && arguments.get("workspace_id").and_then(|v| v.as_str()) == Some(id)
                && arguments.get("request_id").and_then(|v| v.as_str()) == Some(request_id)
                && (emergency == 0
                    || (emergency == 1 && matches!(command, "cancel_run" | "cancel_run_v2"))),
        )?;
        let version = if command.ends_with("_v2") { 2 } else { 1 };
        check(arguments.get("contract_version").and_then(|v| v.as_u64()) == Some(version))?;
        match command {
            "create_session" | "create_session_v2" => {
                let (label, provider) = if version == 1 {
                    let request: CreateSessionRequest = decode(args)?;
                    valid(validate_create_session(&request))?;
                    canonical(&request, args)?;
                    (request.label, v2::ProviderId::Fake)
                } else {
                    let request: v2::CreateSessionRequest = decode(args)?;
                    valid(policy::validate_create_session(&request))?;
                    canonical(&request, args)?;
                    (request.label, request.provider_id)
                };
                let result = valid(StoredSession::decode(result))?;
                check(matches!(
                    (version, result.original()),
                    (1, SessionVersion::V1(_)) | (2, SessionVersion::V2(_))
                ))?;
                let projected = valid(result.projected())?;
                let current = sessions
                    .get(&projected.session_id)
                    .ok_or_else(unavailable)?;
                check(
                    result.original() == current.original()
                        && projected.workspace_id == id
                        && projected.label == label
                        && projected.provider_id == provider,
                )?;
            }
            "start_fake_run" | "start_fake_run_v2" | "start_codex_run_v2" => {
                let result = valid(StoredRunSnapshot::decode(result))?;
                check(matches!(
                    (version, result.original()),
                    (1, SnapshotVersion::V1(_)) | (2, SnapshotVersion::V2(_))
                ))?;
                let projected = valid(result.projected())?;
                let run = runs.get(&projected.run_id).ok_or_else(unavailable)?;
                let session = sessions
                    .get(&projected.session_id)
                    .ok_or_else(unavailable)?;
                let session = valid(session.projected())?;
                check(
                    projected.workspace_id == id
                        && projected.state == v2::RunState::Queued
                        && projected.last_seq == 1,
                )?;
                match command {
                    "start_fake_run" => {
                        let request: StartFakeRunRequest = decode(args)?;
                        valid(validate_start_fake_run(&request))?;
                        canonical(&request, args)?;
                        check(
                            projected.session_id == request.session_id
                                && run.input.prompt() == request.prompt
                                && run.input.scenario().map(|s| s.as_str())
                                    == Some(request.scenario.as_str())
                                && projected.provider_id == v2::ProviderId::Fake,
                        )?;
                    }
                    "start_fake_run_v2" => {
                        let request: v2::StartFakeRunRequest = decode(args)?;
                        valid(policy::validate_fake_session(&request, &session))?;
                        canonical(&request, args)?;
                        check(
                            run.input.prompt() == request.prompt
                                && run.input.scenario() == Some(request.scenario)
                                && projected.provider_id == v2::ProviderId::Fake,
                        )?;
                    }
                    _ => {
                        let request: v2::StartCodexRunRequest = decode(args)?;
                        valid(policy::validate_codex_session(&request, &session))?;
                        canonical(&request, args)?;
                        check(
                            run.input.prompt() == request.prompt
                                && run.input.scenario().is_none()
                                && run.input.profile() == Some(request.profile_id)
                                && run.input.context_run_ids()
                                    == request.context_run_ids.as_deref().unwrap_or_default()
                                && projected.provider_id == v2::ProviderId::Codex,
                        )?;
                    }
                }
                remember(&mut receipts, result, false, false)?;
            }
            "cancel_run" | "cancel_run_v2" => {
                let run_id = if version == 1 {
                    let request: CancelRunRequest = decode(args)?;
                    valid(validate_cancel(&request))?;
                    canonical(&request, args)?;
                    request.run_id
                } else {
                    let request: v2::CancelRunRequest = decode(args)?;
                    valid(policy::validate_cancel(&request))?;
                    canonical(&request, args)?;
                    request.run_id
                };
                let result = valid(StoredRunSnapshot::decode(result))?;
                check(matches!(
                    (version, result.original()),
                    (1, SnapshotVersion::V1(_)) | (2, SnapshotVersion::V2(_))
                ))?;
                let projected = valid(result.projected())?;
                let run = runs.get(&run_id).ok_or_else(unavailable)?;
                let terminal = terminal(projected.state);
                check(
                    projected.workspace_id == id
                        && projected.run_id == run_id
                        && (projected.state == v2::RunState::CancelRequested || terminal),
                )?;
                let legacy_projection =
                    version == 2 && matches!(run.original.original(), SnapshotVersion::V1(_));
                if legacy_projection {
                    check(terminal && emergency == 0 && same_projected(&result, &run.original)?)?;
                }
                remember(&mut receipts, result, emergency == 1, legacy_projection)?;
            }
            "save_layout" | "save_layout_v2" => {
                let request: v2::SaveLayoutRequest = if version == 1 {
                    let request: SaveLayoutRequest = decode(args)?;
                    let legacy_sessions = sessions
                        .values()
                        .filter_map(|s| match s.original() {
                            SessionVersion::V1(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    valid(validate_save_layout(&request, &legacy_sessions))?;
                    canonical(&request, args)?;
                    let mut json = serde_json::to_value(request).map_err(|_| unavailable())?;
                    json["contract_version"] = 2.into();
                    serde_json::from_value(json).map_err(|_| unavailable())?
                } else {
                    let request: v2::SaveLayoutRequest = decode(args)?;
                    canonical(&request, args)?;
                    request
                };
                valid(policy::validate_save_layout(&request, &workspace.sessions))?;
                let result: v2::Layout = decode(result)?;
                valid(policy::validate_layout(&result, id, &workspace.sessions))?;
                check(request.expected_revision < JS_MAX_INTEGER)?;
                let mut expected = request.layout;
                expected.revision += 1;
                check(
                    result == expected
                        && result.revision <= revision
                        && workspace
                            .layout
                            .as_ref()
                            .is_none_or(|l| l.revision != result.revision || *l == result),
                )?;
            }
            _ => return Err(unavailable()),
        }
    }
    Ok(receipts)
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
fn same_projected(a: &StoredRunSnapshot, b: &StoredRunSnapshot) -> StoreResult<bool> {
    Ok(valid(a.projected())? == valid(b.projected())?)
}
fn initial(run: &Run, event: &v2::RunEvent) -> StoreResult<StoredRunSnapshot> {
    check(matches!(event, v2::RunEvent::RunCreated { .. }) && event.header().seq == 1)?;
    let mut value: serde_json::Value = decode(run.original.original_json())?;
    value["state"] = "queued".into();
    value["last_seq"] = 1.into();
    value["created_at"] = event.header().occurred_at.into();
    value["updated_at"] = event.header().occurred_at.into();
    value["terminal_error"] = serde_json::Value::Null;
    for (key, counter) in value["usage"].as_object_mut().ok_or_else(unavailable)? {
        *counter = if key == "estimated" {
            false.into()
        } else {
            serde_json::Value::Null
        };
    }
    valid(StoredRunSnapshot::decode(&encode(&value)?))
}
fn audit_history(conn: &Connection, run: &Run, receipts: &mut Prefixes) -> StoreResult<()> {
    let mut statement = conn.prepare("SELECT event_id,workspace_id,session_id,run_id,seq,event_json FROM events WHERE run_id=?1 ORDER BY seq").map_err(database_error)?;
    let mut rows = statement
        .query([&run.snapshot.run_id])
        .map_err(database_error)?;
    let mut derived: Option<StoredRunSnapshot> = None;
    let mut counts = [0u64; 6];
    while let Some(row) = rows.next().map_err(database_error)? {
        let original = valid(StoredRunEvent::decode(text(row, 5, EVENT_BYTES)?))?;
        let event = valid(original.projected())?;
        let h = event.header();
        check(
            text(row, 0, 36)? == h.event_id
                && text(row, 1, 36)? == h.workspace_id
                && text(row, 2, 36)? == h.session_id
                && text(row, 3, 36)? == h.run_id
                && row.get::<_, u64>(4).map_err(database_error)? == h.seq
                && h.workspace_id == run.snapshot.workspace_id
                && h.session_id == run.snapshot.session_id
                && h.run_id == run.snapshot.run_id
                && h.provider_id == run.snapshot.provider_id,
        )?;
        let next = match &derived {
            Some(current) => valid(apply_stored(current, &original))?,
            None => {
                check(matches!(
                    (run.original.original(), original.original()),
                    (SnapshotVersion::V1(_), EventVersion::V1(_))
                        | (SnapshotVersion::V2(_), EventVersion::V2(_))
                ))?;
                initial(run, &event)?
            }
        };
        for receipt in receipts.remove(&h.seq).unwrap_or_default() {
            check(if receipt.projected_legacy_cancel {
                same_projected(&receipt.snapshot, &next)?
            } else {
                same_original_prefix(&receipt.snapshot, &next)
            })?;
            if receipt.emergency {
                check(
                    matches!(&event, v2::RunEvent::RunStateChanged { source: v2::EventSource::System, payload, .. }
                    if payload.error.is_none() && ((payload.from == v2::RunState::Queued && payload.to == v2::RunState::Cancelled)
                    || (matches!(payload.from, v2::RunState::Starting | v2::RunState::Running | v2::RunState::WaitingInput)
                        && payload.to == v2::RunState::CancelRequested))),
                )?;
            }
        }
        let reserved = match &event {
            v2::RunEvent::OutputTruncated { payload, .. } => {
                let bit = match payload.scope {
                    v2::TruncationScope::PendingBytes => 1,
                    v2::TruncationScope::PersistedTextBytes => 2,
                    v2::TruncationScope::EventCount => 4,
                };
                check(counts[3] & bit == 0)?;
                counts[3] |= bit;
                true
            }
            v2::RunEvent::RunRecovered { .. } => {
                counts[5] += 1;
                true
            }
            v2::RunEvent::RunStateChanged { payload, .. }
                if payload.to == v2::RunState::CancelRequested =>
            {
                counts[4] += 1;
                true
            }
            v2::RunEvent::RunStateChanged { payload, .. } if terminal(payload.to) => {
                check(payload.to != v2::RunState::Succeeded || counts[3] == 0)?;
                counts[5] += 1;
                true
            }
            _ => false,
        };
        if reserved {
            counts[1] += 1;
        } else {
            check(counts[3] == 0)?;
            counts[0] += 1;
            if let v2::RunEvent::TextDelta { payload, .. } = &event {
                counts[2] += payload.text.len() as u64;
            }
        }
        check(
            counts[0] <= ORDINARY_EVENTS as u64
                && counts[1] <= RESERVED_EVENTS as u64
                && counts[2] <= PERSISTED_TEXT_BYTES as u64
                && counts[4] <= 1
                && counts[5] <= 1,
        )?;
        derived = Some(next);
    }
    let derived = derived.ok_or_else(unavailable)?;
    check(
        same_original_prefix(&derived, &run.original)
            && counts == run.counts
            && receipts.is_empty(),
    )
}
