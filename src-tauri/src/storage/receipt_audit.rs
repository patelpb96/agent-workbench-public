//! Private startup receipt audit. No schema, wire contract or replay semantics change.
use super::*;
use std::collections::BTreeMap;

// An allocation guard, not a new public DTO limit. Well above canonical encoding
// of the frozen bounded prompt/layout/error DTOs; inspect borrowed bytes first.
const RECEIPT_JSON_BYTES: usize = 1024 * 1024;

pub(super) struct RunReceipt {
    snapshot: RunSnapshot,
    emergency: bool,
}
pub(super) type PrefixReceipts = BTreeMap<u64, RunReceipt>;
pub(super) type RunReceipts = BTreeMap<String, PrefixReceipts>;

fn text<'a>(row: &'a rusqlite::Row<'_>, column: usize, limit: usize) -> StoreResult<&'a str> {
    match row.get_ref(column).map_err(database_error)? {
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= limit => {
            std::str::from_utf8(bytes).map_err(|_| unavailable())
        }
        _ => Err(unavailable()),
    }
}
fn canonical<T: Serialize>(request: &T, stored: &str) -> StoreResult<()> {
    require_store(
        canonical_arguments(request)? == stored,
        "storage_unavailable",
        "Stored receipt arguments are not canonical.",
    )
}
fn remember(receipts: &mut RunReceipts, snapshot: RunSnapshot, emergency: bool) -> StoreResult<()> {
    checked_stored(validate_snapshot(&snapshot))?;
    let prefixes = receipts.entry(snapshot.run_id.clone()).or_default();
    if let Some(previous) = prefixes.get_mut(&snapshot.last_seq) {
        require_store(
            previous.snapshot == snapshot,
            "storage_unavailable",
            "Stored receipts disagree at the same event prefix.",
        )?;
        previous.emergency |= emergency;
    } else {
        prefixes.insert(
            snapshot.last_seq,
            RunReceipt {
                snapshot,
                emergency,
            },
        );
    }
    Ok(())
}

pub(super) fn audit(conn: &Connection, workspace: &WorkspaceSnapshot) -> StoreResult<RunReceipts> {
    let mut receipts = RunReceipts::new();
    let mut statement = conn.prepare("SELECT workspace_id,command_name,request_id,arguments_json,result_json,emergency FROM receipts").map_err(database_error)?;
    let mut rows = statement.query([]).map_err(database_error)?;
    while let Some(row) = rows.next().map_err(database_error)? {
        let workspace_id = text(row, 0, 36)?;
        let command = text(row, 1, 32)?;
        let request_id = text(row, 2, 36)?;
        let args = text(row, 3, RECEIPT_JSON_BYTES)?;
        let result = text(row, 4, RECEIPT_JSON_BYTES)?;
        let emergency: i64 = row.get(5).map_err(database_error)?;
        checked_stored(validate_id(request_id))?;
        let arguments: serde_json::Value = decode(args)?;
        require_store(
            workspace_id == workspace.workspace.workspace_id
                && arguments.get("workspace_id").and_then(|v| v.as_str()) == Some(workspace_id)
                && arguments.get("request_id").and_then(|v| v.as_str()) == Some(request_id)
                && (emergency == 0 || (emergency == 1 && command == "cancel_run")),
            "storage_unavailable",
            "Stored receipt identity is inconsistent.",
        )?;
        match command {
            "create_session" => {
                let request: CreateSessionRequest =
                    serde_json::from_value(arguments).map_err(|_| unavailable())?;
                checked_stored(validate_create_session(&request))?;
                canonical(&request, args)?;
                let result: SessionSnapshot = decode(result)?;
                checked_stored(validate_session(&result))?;
                require_store(
                    result.label == request.label && workspace.sessions.contains(&result),
                    "storage_unavailable",
                    "Stored session receipt disagrees with its immutable session.",
                )?;
            }
            "start_fake_run" => {
                let request: StartFakeRunRequest =
                    serde_json::from_value(arguments).map_err(|_| unavailable())?;
                checked_stored(validate_start_fake_run(&request))?;
                canonical(&request, args)?;
                let result: RunSnapshot = decode(result)?;
                checked_stored(validate_snapshot(&result))?;
                let run = load_run(conn, workspace_id, &result.run_id)?;
                require_store(
                    result.workspace_id == workspace_id
                        && result.session_id == request.session_id
                        && result.state == RunState::Queued
                        && result.last_seq == 1
                        && run.prompt == request.prompt
                        && run.scenario == request.scenario,
                    "storage_unavailable",
                    "Stored start receipt disagrees with its run or private input.",
                )?;
                remember(&mut receipts, result, false)?;
            }
            "cancel_run" => {
                let request: CancelRunRequest =
                    serde_json::from_value(arguments).map_err(|_| unavailable())?;
                checked_stored(validate_cancel(&request))?;
                canonical(&request, args)?;
                let result: RunSnapshot = decode(result)?;
                require_store(
                    result.workspace_id == workspace_id
                        && result.run_id == request.run_id
                        && (result.state == RunState::CancelRequested || is_terminal(result.state)),
                    "storage_unavailable",
                    "Stored cancel receipt disagrees with its target or outcome.",
                )?;
                remember(&mut receipts, result, emergency == 1)?;
            }
            "save_layout" => {
                let request: SaveLayoutRequest =
                    serde_json::from_value(arguments).map_err(|_| unavailable())?;
                checked_stored(validate_save_layout(&request, &workspace.sessions))?;
                canonical(&request, args)?;
                let result: Layout = decode(result)?;
                checked_stored(validate_layout(&result, workspace_id, &workspace.sessions))?;
                require_store(
                    request.expected_revision < JS_MAX_INTEGER,
                    "storage_unavailable",
                    "Stored layout receipt exceeds revision capacity.",
                )?;
                let mut expected = request.layout;
                expected.revision += 1;
                // The current layout may be intentionally unavailable with a
                // warning (invalid/newer format). Do not turn that into a fatal
                // execution error when its historical receipts remain valid.
                let current_revision: u64 = conn
                    .query_row(
                        "SELECT layout_revision FROM workspaces WHERE workspace_id=?1",
                        [workspace_id],
                        |row| row.get(0),
                    )
                    .map_err(database_error)?;
                require_store(
                    result == expected
                        && result.revision <= current_revision
                        && workspace.layout.as_ref().is_none_or(|current| {
                            current.revision != result.revision || current == &result
                        }),
                    "storage_unavailable",
                    "Stored layout receipt disagrees with its submitted layout or revision.",
                )?;
            }
            _ => return Err(unavailable()),
        }
    }
    Ok(receipts)
}

// Compare during the existing single event-history pass rather than replaying
// a run once per receipt (a full ledger can have thousands of no-op cancels).
pub(super) fn prefix(
    receipts: &mut PrefixReceipts,
    derived: &RunSnapshot,
    event: &RunEvent,
) -> StoreResult<()> {
    if let Some(receipt) = receipts.remove(&derived.last_seq) {
        require_store(
            receipt.snapshot == *derived,
            "storage_unavailable",
            "Stored run receipt disagrees with its committed event prefix.",
        )?;
        if receipt.emergency {
            let first_cancel = matches!(event, RunEvent::RunStateChanged { source: EventSource::System, payload, .. }
                if payload.error.is_none() && ((payload.from == RunState::Queued && payload.to == RunState::Cancelled)
                    || (matches!(payload.from, RunState::Starting | RunState::Running | RunState::WaitingInput) && payload.to == RunState::CancelRequested)));
            require_store(
                first_cancel,
                "storage_unavailable",
                "Emergency receipt does not represent a first cancellation.",
            )?;
        }
    }
    Ok(())
}
