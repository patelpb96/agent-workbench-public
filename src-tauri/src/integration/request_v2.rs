//! Strict v2 request decode and response checks for the eight v2 commands.
//! One declarative table supplies decode, preflight and the Tauri wrappers.
use super::coordinator_v2::OwnerRuntime;
use crate::{
    execution::{ports::StoreResult, validation as common},
    execution_v2::{self as v2, validation as policy},
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::io::{self, Write};

const REQUEST_BYTES: usize = v2::PROMPT_BYTES * 6 + v2::NONTEXT_EVENT_BYTES;
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
fn invalid() -> crate::execution::Error {
    super::coordinator::invalid_response()
}
fn require(ok: bool) -> StoreResult<()> {
    if ok {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn wire<T: Serialize>(value: &T) -> StoreResult<Value> {
    serde_json::to_value(value).map_err(|_| invalid())
}
fn decode<T: DeserializeOwned>(value: Value) -> StoreResult<T> {
    // Unknown fields are rejected by the generated DTOs' strict serde config.
    serde_json::from_value(value)
        .map_err(|_| common::error("validation_failed", "The execution request is invalid."))
}
fn bootstrap_shape(r: &v2::BootstrapRequest) -> StoreResult<()> {
    require_version(r.contract_version)
}
fn require_version(version: u64) -> StoreResult<()> {
    if version == 2 {
        Ok(())
    } else {
        Err(common::error(
            "unsupported_contract_version",
            "Unsupported execution contract version.",
        ))
    }
}
fn save_shape(r: &v2::SaveLayoutRequest) -> StoreResult<()> {
    require_version(r.contract_version)?;
    common::validate_id(&r.workspace_id)?;
    common::validate_id(&r.request_id)?;
    common::validate_counter(r.expected_revision)?;
    if r.layout.panels.len() > v2::WORKSPACE_SESSIONS || r.layout.containers.len() > 8 {
        return Err(common::error(
            "validation_failed",
            "Layout capacity exceeded.",
        ));
    }
    Ok(())
}

macro_rules! execution_commands_v2 {
    ($apply:ident) => {
        $apply! {
            (bootstrap_workspace_v2, Bootstrap, v2::BootstrapRequest, bootstrap_shape),
            (create_session_v2, Create, v2::CreateSessionRequest, policy::validate_create_session),
            (start_fake_run_v2, StartFake, v2::StartFakeRunRequest, policy::validate_fake_start),
            (start_codex_run_v2, StartCodex, v2::StartCodexRunRequest, policy::validate_codex_start),
            (cancel_run_v2, Cancel, v2::CancelRunRequest, policy::validate_cancel),
            (get_workspace_snapshot_v2, Snapshot, v2::WorkspaceRequest, policy::validate_workspace_request),
            (get_run_progress_v2, Progress, v2::WorkspaceRequest, policy::validate_workspace_request),
            (preview_codex_context_v2, Preview, v2::PreviewCodexContextRequest, policy::validate_preview_context),
            (list_run_events_v2, Events, v2::EventPageRequest, policy::validate_event_page),
            (save_layout_v2, Layout, v2::SaveLayoutRequest, save_shape),
        }
    };
}
pub(super) use execution_commands_v2;
macro_rules! define_request {
    ($(($name:ident, $variant:ident, $ty:ty, $validator:path),)*) => {
        pub(super) enum RequestV2 { $($variant($ty),)* }
        impl RequestV2 {
            pub fn decode(command: &str, value: Value) -> StoreResult<Self> {
                serde_json::to_writer(ByteBudget(REQUEST_BYTES), &value).map_err(|_| {
                    common::error("validation_failed", "Execution request exceeds its byte budget.")
                })?;
                match command {
                    $(stringify!($name) => {
                        let request: $ty = decode(value)?;
                        $validator(&request)?;
                        Ok(Self::$variant(request))
                    },)*
                    _ => Err(common::error("validation_failed", "Unknown execution command.")),
                }
            }
        }
    }
}
execution_commands_v2!(define_request);

impl RequestV2 {
    pub fn control(&self) -> bool {
        matches!(self, Self::Cancel(_))
    }
    pub fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Create(_)
                | Self::StartFake(_)
                | Self::StartCodex(_)
                | Self::Cancel(_)
                | Self::Layout(_)
        )
    }
    pub fn execute(self, rt: &mut dyn OwnerRuntime) -> StoreResult<Value> {
        let valid_run =
            |run: &v2::RunSnapshot| policy::validate_snapshot(run).map_err(|_| invalid());
        match self {
            Self::Bootstrap(r) => {
                let response = rt.bootstrap(&r)?;
                policy::validate_bootstrap(&response).map_err(|_| invalid())?;
                wire(&response)
            }
            Self::Snapshot(r) => {
                let response = rt.snapshot(&r)?;
                policy::validate_workspace(&response).map_err(|_| invalid())?;
                require(response.workspace.workspace_id == r.workspace_id)?;
                wire(&response)
            }
            Self::Progress(r) => {
                let response = rt.progress(&r)?;
                require(
                    response.workspace_id == r.workspace_id
                        && response.steps.len() <= crate::codex::MAX_EVENTS,
                )?;
                wire(&response)
            }
            Self::Preview(r) => {
                let response = rt.preview_context(&r)?;
                require(
                    response.workspace_id == r.workspace_id
                        && response.text.len() <= v2::CODEX_CONTEXT_BYTES
                        && response.bytes == response.text.len() as u64
                        && response.carried <= r.context_run_ids.len() as u64,
                )?;
                wire(&response)
            }
            Self::Create(r) => {
                let response = rt.create_session(&r)?;
                policy::validate_session(&response.session).map_err(|_| invalid())?;
                require(
                    response.session.workspace_id == r.workspace_id
                        && response.session.label == r.label
                        && response.session.provider_id == r.provider_id,
                )?;
                wire(&response)
            }
            Self::StartFake(r) => {
                let response = rt.start_fake(&r)?;
                valid_run(&response.run)?;
                require(
                    response.run.workspace_id == r.workspace_id
                        && response.run.session_id == r.session_id
                        && response.run.provider_id == v2::ProviderId::Fake
                        && response.run.state == v2::RunState::Queued
                        && response.run.last_seq == 1,
                )?;
                wire(&response)
            }
            Self::StartCodex(r) => {
                let response = rt.start_codex(&r)?;
                valid_run(&response.run)?;
                require(
                    response.run.workspace_id == r.workspace_id
                        && response.run.session_id == r.session_id
                        && response.run.provider_id == v2::ProviderId::Codex
                        && response.run.state == v2::RunState::Queued
                        && response.run.last_seq == 1,
                )?;
                wire(&response)
            }
            Self::Cancel(r) => {
                let response = rt.cancel(&r)?;
                valid_run(&response.run)?;
                require(
                    response.run.workspace_id == r.workspace_id && response.run.run_id == r.run_id,
                )?;
                wire(&response)
            }
            Self::Events(r) => {
                let response = rt.events(&r)?;
                require(
                    response.workspace_id == r.workspace_id
                        && response.run_id == r.run_id
                        && response.events.len() as u64 <= r.limit,
                )?;
                for event in &response.events {
                    policy::validate_event(event).map_err(|_| invalid())?;
                }
                wire(&response)
            }
            Self::Layout(r) => {
                let response = rt.save_layout(&r)?;
                require(response.layout.revision == r.expected_revision + 1)?;
                wire(&response)
            }
        }
    }
}
