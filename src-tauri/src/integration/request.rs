use crate::execution::{ports::*, validation as v, *};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{self, Write},
};

// Limits allocations while decoding into typed requests. Six bytes per prompt
// byte covers JSON escaping, plus a bounded envelope/layout allowance.
const REQUEST_BYTES: usize = PROMPT_BYTES * 6 + NONTEXT_EVENT_BYTES;
struct ByteBudget(usize);
impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("request budget"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn require(ok: bool) -> StoreResult<()> {
    if ok {
        Ok(())
    } else {
        Err(super::coordinator::invalid_response())
    }
}
fn wire<T: Serialize>(value: &T) -> StoreResult<Value> {
    serde_json::to_value(value).map_err(|_| super::coordinator::invalid_response())
}
fn valid(result: StoreResult<()>) -> StoreResult<()> {
    result.map_err(|_| super::coordinator::invalid_response())
}
fn save_shape(request: &SaveLayoutRequest) -> StoreResult<()> {
    v::validate_version(request.contract_version)?;
    v::validate_id(&request.workspace_id)?;
    v::validate_id(&request.request_id)?;
    v::validate_counter(request.expected_revision)?;
    if request.layout.panels.len() > WORKSPACE_SESSIONS || request.layout.containers.len() > 8 {
        return Err(v::error("validation_failed", "Layout capacity exceeded."));
    }
    // Full ownership/geometry validation runs on the owner with real sessions.
    Ok(())
}
fn bootstrap_shape(request: &BootstrapRequest) -> StoreResult<()> {
    v::validate_version(request.contract_version)
}

// One declarative table supplies strict decode, semantic preflight, the private
// typed envelope AND all seven Tauri wrappers. Frozen DTOs stay generator-owned.
macro_rules! execution_commands {
    ($apply:ident) => {
        $apply! {
            (bootstrap_workspace, Bootstrap, BootstrapRequest, bootstrap_shape),
            (create_session, Create, CreateSessionRequest, v::validate_create_session),
            (start_fake_run, Start, StartFakeRunRequest, v::validate_start_fake_run),
            (cancel_run, Cancel, CancelRunRequest, v::validate_cancel),
            (get_workspace_snapshot, Snapshot, WorkspaceRequest, v::validate_workspace_request),
            (list_run_events, Events, EventPageRequest, v::validate_event_page),
            (save_layout, Layout, SaveLayoutRequest, save_shape),
        }
    };
}
pub(super) use execution_commands;
macro_rules! define_request {
    ($(($name:ident, $variant:ident, $ty:ty, $validator:path),)*) => {
        pub(super) enum Request { $($variant($ty),)* }
        impl Request {
            pub fn decode(command: &str, value: Value) -> StoreResult<Self> {
                serde_json::to_writer(ByteBudget(REQUEST_BYTES), &value)
                    .map_err(|_| v::error("validation_failed", "Execution request exceeds its byte budget."))?;
                match command {
                    $(stringify!($name) => {
                        let request: $ty = v::decode_request(value)?;
                        $validator(&request)?;
                        Ok(Self::$variant(request))
                    },)*
                    _ => Err(v::error("validation_failed", "Unknown execution command.")),
                }
            }
        }
    }
}
execution_commands!(define_request);

fn workspace(rt: &mut dyn ExecutionRuntime, id: &str) -> StoreResult<WorkspaceSnapshot> {
    let value = rt.get_workspace_snapshot(&WorkspaceRequest {
        contract_version: 1,
        workspace_id: id.into(),
    })?;
    valid(v::validate_workspace_snapshot(&value))?;
    require(value.workspace.workspace_id == id)?;
    Ok(value)
}
fn check_page(request: &EventPageRequest, page: &EventPage, run: &RunSnapshot) -> StoreResult<()> {
    valid(v::validate_version(page.contract_version))?;
    for counter in [page.last_seq, page.through_seq, page.next_after_seq] {
        valid(v::validate_counter(counter))?;
    }
    require(
        page.workspace_id == request.workspace_id
            && page.run_id == request.run_id
            && page.last_seq == run.last_seq
            && page.through_seq == request.through_seq.unwrap_or(page.last_seq)
            && request.after_seq <= page.through_seq
            && page.through_seq <= page.last_seq,
    )?;
    let expected = (request.limit).min(page.through_seq - request.after_seq);
    require(
        page.events.len() as u64 == expected
            && page.next_after_seq == request.after_seq + expected
            && page.has_more == (page.next_after_seq < page.through_seq),
    )?;
    let mut ids = HashSet::new();
    for (index, event) in page.events.iter().enumerate() {
        valid(v::validate_event(event))?;
        let h = event.header();
        require(
            h.workspace_id == request.workspace_id
                && h.run_id == request.run_id
                && h.session_id == run.session_id
                && h.provider_id == run.provider_id
                && h.seq == request.after_seq + index as u64 + 1
                && ids.insert(h.event_id),
        )?;
    }
    Ok(())
}
impl Request {
    pub fn control(&self) -> bool {
        matches!(self, Self::Cancel(_))
    }
    pub fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Create(_) | Self::Start(_) | Self::Cancel(_) | Self::Layout(_)
        )
    }
    pub fn execute(self, rt: &mut dyn ExecutionRuntime, available: bool) -> StoreResult<Value> {
        match self {
            Self::Bootstrap(request) => {
                let mut response = rt.bootstrap_workspace(&request)?;
                valid(v::validate_workspace_snapshot(&response))?;
                response.execution_available &= available && rt.health() == ExecutionHealth::Ready;
                wire(&response)
            }
            Self::Snapshot(request) => {
                let mut response = workspace(rt, &request.workspace_id)?;
                response.execution_available &= available && rt.health() == ExecutionHealth::Ready;
                wire(&response)
            }
            Self::Create(request) => {
                let response = rt.create_session(&request)?;
                valid(v::validate_version(response.contract_version))?;
                valid(v::validate_session(&response.session))?;
                require(
                    response.session.workspace_id == request.workspace_id
                        && response.session.label == request.label,
                )?;
                wire(&response)
            }
            Self::Start(request) => {
                let response = rt.start_fake_run(&request)?;
                valid(v::validate_version(response.contract_version))?;
                valid(v::validate_snapshot(&response.run))?;
                require(
                    response.run.workspace_id == request.workspace_id
                        && response.run.session_id == request.session_id
                        && response.run.state == RunState::Queued
                        && response.run.last_seq == 1,
                )?;
                wire(&response)
            }
            Self::Cancel(request) => {
                let response = rt.cancel_run(&request)?;
                valid(v::validate_version(response.contract_version))?;
                valid(v::validate_snapshot(&response.run))?;
                require(
                    response.run.workspace_id == request.workspace_id
                        && response.run.run_id == request.run_id
                        && (v::is_terminal(response.run.state)
                            || response.run.state == RunState::CancelRequested),
                )?;
                wire(&response)
            }
            Self::Events(request) => {
                // All calls are serialized; no tick/command can interleave between
                // this summary and page. Verify session ownership and high-water.
                let snapshot = workspace(rt, &request.workspace_id)?;
                let run = snapshot
                    .runs
                    .iter()
                    .find(|run| run.run_id == request.run_id)
                    .ok_or_else(|| {
                        v::error(
                            "run_not_found",
                            "Run was not found in the selected workspace.",
                        )
                    })?;
                let response = rt.list_run_events(&request)?;
                check_page(&request, &response, run)?;
                wire(&response)
            }
            Self::Layout(request) => {
                let snapshot = workspace(rt, &request.workspace_id)?;
                v::validate_save_layout(&request, &snapshot.sessions)?;
                let response = rt.save_layout(&request)?;
                valid(v::validate_version(response.contract_version))?;
                valid(v::validate_layout(
                    &response.layout,
                    &request.workspace_id,
                    &snapshot.sessions,
                ))?;
                require(response.layout.revision == request.expected_revision + 1)?;
                wire(&response)
            }
        }
    }
}
