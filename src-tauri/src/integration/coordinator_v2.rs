//! v2 serialized owner thread (C30a). Same mailbox discipline as v1: bounded
//! nonblocking admission, a priority control queue for cancellations, one
//! service quantum per loop, a cadence tick, and change hints that never carry
//! commands. On stop the owner runs the runtime's bounded clean shutdown
//! (development-phase: cancel the active Codex run and commit `cancelled`).
use super::{
    coordinator::{busy, invalid_response, unavailable},
    reply::{self, Permit, Reply},
    request_v2::RequestV2,
};
use crate::{
    execution::ports::{ExecutionHealth, StoreResult},
    execution::{COMMAND_MAILBOX, CONTROL_MAILBOX},
    execution_v2 as v2,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{atomic::AtomicUsize, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CADENCE: Duration = Duration::from_millis(100);
/// Upper bound for the owner's clean shutdown (Codex teardown proof included).
pub const SHUTDOWN_BOUND: Duration = Duration::from_secs(8);

/// Object-safe owner surface; implemented by `RuntimeV2` for any store/provider.
pub trait OwnerRuntime: Send {
    fn workspace_id(&self) -> String;
    fn health(&self) -> ExecutionHealth;
    fn take_changed(&mut self) -> bool;
    fn tick(&mut self) -> StoreResult<()>;
    fn shutdown(&mut self, timeout: Duration) -> bool;
    fn bootstrap(&mut self, r: &v2::BootstrapRequest) -> StoreResult<v2::BootstrapResponse>;
    fn snapshot(&mut self, r: &v2::WorkspaceRequest) -> StoreResult<v2::WorkspaceSnapshot>;
    fn create_session(
        &mut self,
        r: &v2::CreateSessionRequest,
    ) -> StoreResult<v2::CreateSessionResponse>;
    fn start_fake(&mut self, r: &v2::StartFakeRunRequest) -> StoreResult<v2::StartFakeRunResponse>;
    fn start_codex(
        &mut self,
        r: &v2::StartCodexRunRequest,
    ) -> StoreResult<v2::StartCodexRunResponse>;
    fn cancel(&mut self, r: &v2::CancelRunRequest) -> StoreResult<v2::CancelRunResponse>;
    fn events(&mut self, r: &v2::EventPageRequest) -> StoreResult<v2::EventPage>;
    fn progress(&mut self, r: &v2::WorkspaceRequest) -> StoreResult<crate::runtime_v2::RunProgress>;
    fn preview_context(
        &mut self,
        r: &v2::PreviewCodexContextRequest,
    ) -> StoreResult<v2::CodexContextPreview>;
    fn save_layout(&mut self, r: &v2::SaveLayoutRequest) -> StoreResult<v2::SaveLayoutResponse>;
}

impl<S, F, C, I> OwnerRuntime for crate::runtime_v2::RuntimeV2<S, F, C, I>
where
    S: crate::runtime_v2::OwnedStore + Send,
    F: crate::supervisor::ProviderFactory,
    C: crate::execution::ports::Clock,
    I: crate::execution::ports::IdSource,
{
    fn workspace_id(&self) -> String {
        crate::runtime_v2::RuntimeV2::workspace_id(self).to_owned()
    }
    fn health(&self) -> ExecutionHealth {
        crate::runtime_v2::RuntimeV2::health(self)
    }
    fn take_changed(&mut self) -> bool {
        crate::runtime_v2::RuntimeV2::take_changed(self)
    }
    fn tick(&mut self) -> StoreResult<()> {
        crate::runtime_v2::RuntimeV2::tick(self)
    }
    fn shutdown(&mut self, timeout: Duration) -> bool {
        crate::runtime_v2::RuntimeV2::shutdown(self, timeout)
    }
    fn bootstrap(&mut self, r: &v2::BootstrapRequest) -> StoreResult<v2::BootstrapResponse> {
        self.bootstrap_workspace(r)
    }
    fn snapshot(&mut self, r: &v2::WorkspaceRequest) -> StoreResult<v2::WorkspaceSnapshot> {
        v2::validation::validate_workspace_request(r)?;
        if r.workspace_id != crate::runtime_v2::RuntimeV2::workspace_id(self) {
            return Err(crate::execution::validation::error(
                "workspace_not_found",
                "Unknown managed workspace.",
            ));
        }
        crate::runtime_v2::RuntimeV2::snapshot(self)
    }
    fn create_session(
        &mut self,
        r: &v2::CreateSessionRequest,
    ) -> StoreResult<v2::CreateSessionResponse> {
        crate::runtime_v2::RuntimeV2::create_session(self, r)
    }
    fn start_fake(&mut self, r: &v2::StartFakeRunRequest) -> StoreResult<v2::StartFakeRunResponse> {
        self.start_fake_run(r)
    }
    fn start_codex(
        &mut self,
        r: &v2::StartCodexRunRequest,
    ) -> StoreResult<v2::StartCodexRunResponse> {
        self.start_codex_run(r)
    }
    fn cancel(&mut self, r: &v2::CancelRunRequest) -> StoreResult<v2::CancelRunResponse> {
        self.cancel_run(r)
    }
    fn events(&mut self, r: &v2::EventPageRequest) -> StoreResult<v2::EventPage> {
        self.list_run_events(r)
    }
    fn progress(&mut self, r: &v2::WorkspaceRequest) -> StoreResult<crate::runtime_v2::RunProgress> {
        if r.workspace_id != crate::runtime_v2::RuntimeV2::workspace_id(self) {
            return Err(crate::execution::validation::error(
                "workspace_not_found",
                "Unknown managed workspace.",
            ));
        }
        Ok(crate::runtime_v2::RuntimeV2::progress(self))
    }
    fn preview_context(
        &mut self,
        r: &v2::PreviewCodexContextRequest,
    ) -> StoreResult<v2::CodexContextPreview> {
        self.preview_codex_context(r)
    }
    fn save_layout(&mut self, r: &v2::SaveLayoutRequest) -> StoreResult<v2::SaveLayoutResponse> {
        crate::runtime_v2::RuntimeV2::save_layout(self, r)
    }
}

/// No raw I/O/Serde/panic/provider text crosses the IPC seam.
fn public_error(failure: crate::execution::Error) -> crate::execution::Error {
    let message = match failure.code.as_str() {
        "validation_failed" => "The execution request is invalid.",
        "unsupported_contract_version" => "This execution contract version is unsupported.",
        "provider_execution_disabled" => "This command does not start that provider.",
        "provider_activity_disabled" => "That activity is disabled in this profile.",
        "request_id_conflict" => "This request identity was already used with different arguments.",
        "workspace_not_found" => "The managed workspace was not found.",
        "session_not_found" => "The selected session was not found.",
        "run_not_found" => "The selected run was not found.",
        "state_conflict" => "The execution state changed. Reconcile before retrying.",
        "capacity_exceeded" => "Execution capacity has been reached.",
        "queue_full" => "The run queue is full.",
        "context_unavailable" => "Carried runs must be earlier succeeded Codex runs of this session.",
        "unsupported_storage_version" => {
            "Stored execution data has an unsupported version and is preserved."
        }
        "unsupported_layout_version" => {
            "Stored layout has an unsupported version and is preserved."
        }
        "layout_revision_conflict" => "The layout revision changed. Reconcile before retrying.",
        "layout_invalid" => "Stored layout is invalid and is preserved; saving is disabled.",
        "event_history_unavailable" => "Committed execution history could not be validated.",
        "invalid_execution_response" => return invalid_response(),
        "storage_busy" => return busy(),
        _ => return unavailable(),
    };
    crate::execution::validation::error(&failure.code, message)
}

struct Job {
    request: RequestV2,
    reply: reply::Sender,
}
struct State {
    ordinary: VecDeque<Job>,
    control: VecDeque<Job>,
    stopping: bool,
    exited: bool,
    health: ExecutionHealth,
    startup_failure: Option<crate::execution::Error>,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    ordinary_slots: Arc<AtomicUsize>,
    control_slots: Arc<AtomicUsize>,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn pop(&self, control: bool) -> Option<Job> {
        let mut state = self.lock();
        if state.stopping {
            return None;
        }
        if control {
            state.control.pop_front()
        } else {
            state.ordinary.pop_front()
        }
    }
    fn finish(&self) {
        let (jobs, failure) = {
            let mut state = self.lock();
            state.stopping = true;
            state.exited = true;
            state.health = ExecutionHealth::StorageUnavailable;
            let mut jobs = std::mem::take(&mut state.ordinary);
            jobs.append(&mut state.control);
            (
                jobs,
                state.startup_failure.clone().unwrap_or_else(unavailable),
            )
        };
        for job in jobs {
            job.reply.complete(Err(failure.clone()));
        }
        self.wake.notify_all();
    }
}
struct Owner {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.lock().stopping = true;
        self.shared.wake.notify_all();
    }
}

/// Cloneable mailbox handle; never a runtime, store or database mutex.
#[derive(Clone)]
pub struct CoordinatorV2(Arc<Owner>);
impl CoordinatorV2 {
    pub fn start(
        factory: impl FnOnce() -> StoreResult<Box<dyn OwnerRuntime>> + Send + 'static,
        notify: impl FnMut(v2::ExecutionChanged) -> bool + Send + 'static,
    ) -> Self {
        Self::spawn(factory, notify, CADENCE)
    }
    pub(super) fn spawn(
        factory: impl FnOnce() -> StoreResult<Box<dyn OwnerRuntime>> + Send + 'static,
        notify: impl FnMut(v2::ExecutionChanged) -> bool + Send + 'static,
        cadence: Duration,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ordinary: VecDeque::with_capacity(COMMAND_MAILBOX),
                control: VecDeque::with_capacity(CONTROL_MAILBOX),
                stopping: false,
                exited: false,
                health: ExecutionHealth::Starting,
                startup_failure: None,
            }),
            wake: Condvar::new(),
            ordinary_slots: Arc::new(AtomicUsize::new(0)),
            control_slots: Arc::new(AtomicUsize::new(0)),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("execution-owner-v2".into())
            .spawn(move || {
                // A panic drops the runtime (its supervisor retains worker handles
                // until joined); no terminal record is invented; callers released.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    run(worker_shared.clone(), factory, notify, cadence)
                }));
                worker_shared.finish();
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(_) => {
                shared.finish();
                None
            }
        };
        Self(Arc::new(Owner {
            shared,
            worker: Mutex::new(worker),
        }))
    }
    /// Production owner: fresh v2 journal in `directory` (never the legacy
    /// `simulated-execution` store), pinned Codex provider, and availability
    /// from read-only pinned-executable verification (no model invocation).
    pub fn start_production(
        directory: std::path::PathBuf,
        notify: impl FnMut(v2::ExecutionChanged) -> bool + Send + 'static,
    ) -> Self {
        Self::start(
            move || {
                let codex = if crate::codex::ExecutablePin::pinned_codex()
                    .verify_and_lock()
                    .is_ok()
                {
                    v2::CodexAvailability::Ready
                } else {
                    v2::CodexAvailability::NotConfigured
                };
                let store = crate::storage::store_v2::V2Store::open(&directory)?;
                Ok(Box::new(crate::runtime_v2::RuntimeV2::new(
                    store,
                    crate::supervisor::CodexFactory::pinned(),
                    codex,
                    super::coordinator::ProductionClock,
                    super::coordinator::RandomIds,
                )?) as Box<dyn OwnerRuntime>)
            },
            notify,
        )
    }
    pub fn disabled() -> Self {
        Self::spawn(|| Err(unavailable()), |_| false, CADENCE)
    }

    /// Nonblocking try-admission; saturation returns `storage_busy`.
    pub fn submit(&self, command: &str, request: Value) -> StoreResult<Reply> {
        let request = RequestV2::decode(command, request).map_err(public_error)?;
        let shared = &self.0.shared;
        let mut state = shared.lock();
        if let Some(failure) = &state.startup_failure {
            return Err(failure.clone());
        }
        if state.stopping {
            return Err(unavailable());
        }
        if state.health == ExecutionHealth::StorageUnavailable && request.mutation() {
            return Err(unavailable());
        }
        let control = request.control();
        let permit = Permit::acquire(
            if control {
                &shared.control_slots
            } else {
                &shared.ordinary_slots
            },
            if control {
                CONTROL_MAILBOX
            } else {
                COMMAND_MAILBOX
            },
        )?;
        let (sender, receiver) = reply::pair(permit);
        let queue = if control {
            &mut state.control
        } else {
            &mut state.ordinary
        };
        queue.push_back(Job {
            request,
            reply: sender,
        });
        drop(state);
        shared.wake.notify_one();
        Ok(receiver)
    }
    pub fn health(&self) -> ExecutionHealth {
        self.0.shared.lock().health
    }
    /// Stop admission; the owner runs its bounded clean shutdown, then exits.
    /// Returns false if the owner did not exit within `timeout`.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let shared = &self.0.shared;
        shared.lock().stopping = true;
        shared.wake.notify_all();
        let deadline = Instant::now() + timeout;
        let mut state = shared.lock();
        while !state.exited {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = shared
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(state);
        if let Some(worker) = self
            .0
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = worker.join();
        }
        true
    }
}

fn run(
    shared: Arc<Shared>,
    factory: impl FnOnce() -> StoreResult<Box<dyn OwnerRuntime>>,
    mut notify: impl FnMut(v2::ExecutionChanged) -> bool,
    cadence: Duration,
) {
    let mut rt = match factory() {
        Ok(rt) if rt.health() == ExecutionHealth::Ready => rt,
        Ok(_) => {
            shared.lock().startup_failure = Some(unavailable());
            return;
        }
        Err(failure) => {
            shared.lock().startup_failure = Some(public_error(failure));
            return;
        }
    };
    let workspace_id = rt.workspace_id();
    let mut pending = true;
    let mut next_notify = Instant::now();
    let mut last_health = ExecutionHealth::Starting;
    let mut next_tick = Instant::now() + cadence;
    let mut sample = |rt: &mut dyn OwnerRuntime, pending: &mut bool| {
        if rt.take_changed() {
            *pending = true;
        }
        let health = rt.health();
        if health != last_health {
            *pending = true;
            last_health = health;
        }
        shared.lock().health = health;
    };
    sample(rt.as_mut(), &mut pending);
    loop {
        if shared.lock().stopping {
            break;
        }
        let mut worked = false;
        for _ in 0..CONTROL_MAILBOX {
            let Some(job) = shared.pop(true) else { break };
            let result = job.request.execute(rt.as_mut());
            sample(rt.as_mut(), &mut pending);
            job.reply.complete(result.map_err(public_error));
            worked = true;
        }
        if let Some(job) = shared.pop(false) {
            let result = job.request.execute(rt.as_mut());
            sample(rt.as_mut(), &mut pending);
            job.reply.complete(result.map_err(public_error));
            worked = true;
        }
        if pending && Instant::now() >= next_notify {
            pending = !notify(v2::ExecutionChanged {
                contract_version: 2,
                workspace_id: workspace_id.clone(),
            });
            next_notify = Instant::now() + cadence;
        }
        if Instant::now() >= next_tick {
            if rt.health() == ExecutionHealth::Ready {
                let _ = rt.tick();
            }
            sample(rt.as_mut(), &mut pending);
            next_tick = Instant::now() + cadence;
            worked = true;
        }
        if worked {
            continue;
        }
        let state = shared.lock();
        if state.stopping || !state.ordinary.is_empty() || !state.control.is_empty() {
            continue;
        }
        let mut wait = next_tick.saturating_duration_since(Instant::now());
        if pending {
            wait = wait.min(next_notify.saturating_duration_since(Instant::now()));
        }
        let _ = shared
            .wake
            .wait_timeout(state, wait)
            .unwrap_or_else(|e| e.into_inner());
    }
    // Development-phase clean close: cancel + commit within the bound.
    let _ = rt.shutdown(SHUTDOWN_BOUND);
    if rt.take_changed() {
        let _ = notify(v2::ExecutionChanged {
            contract_version: 2,
            workspace_id,
        });
    }
}
