pub use super::reply::Reply;
use super::{
    reply::{self, Permit},
    request::Request,
};
use crate::{
    execution::{ports::*, validation as v, *},
    runtime::Runtime,
    storage::SqliteStore,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{atomic::AtomicUsize, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

const CADENCE: Duration = Duration::from_millis(100);
const CONTROL_BURST: usize = CONTROL_MAILBOX;
pub(super) fn unavailable() -> Error {
    v::error(
        "storage_unavailable",
        "Execution is unavailable; existing data is preserved.",
    )
}
pub(super) fn busy() -> Error {
    v::error(
        "storage_busy",
        "Execution admission is busy. Retry explicitly with the same request identity.",
    )
}
pub(super) fn invalid_response() -> Error {
    v::error(
        "invalid_execution_response",
        "The native execution response could not be validated.",
    )
}
// No raw I/O/Serde/panic text, paths or provider exceptions cross the IPC seam.
fn public_error(failure: Error) -> Error {
    let message = match failure.code.as_str() {
        "validation_failed" => "The execution request is invalid.",
        "unsupported_contract_version" => "This execution contract version is unsupported.",
        "provider_execution_disabled" => "Only the explicitly simulated fake provider is enabled.",
        "request_id_conflict" => "This request identity was already used with different arguments.",
        "workspace_not_found" => "The managed workspace was not found.",
        "session_not_found" => "The selected session was not found.",
        "run_not_found" => "The selected run was not found.",
        "state_conflict" => "The execution state changed. Reconcile before retrying.",
        "capacity_exceeded" => "Execution capacity has been reached.",
        "queue_full" => "The simulated run queue is full.",
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
    v::error(&failure.code, message)
}
struct Job {
    request: Request,
    reply: reply::Sender,
}
struct State {
    ordinary: VecDeque<Job>,
    control: VecDeque<Job>,
    stopping: bool,
    exited: bool,
    health: ExecutionHealth,
    startup_failure: Option<Error>,
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
    fn stop(&self) {
        self.lock().stopping = true;
        self.wake.notify_all();
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
        let jobs = {
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
        // Wake abandoned/pending callers outside the queue mutex.
        for job in jobs.0 {
            job.reply.complete(Err(jobs.1.clone()));
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
        self.shared.stop();
    }
}
/// Cloneable mailbox handle, never a runtime/store or a database mutex.
#[derive(Clone)]
pub struct Coordinator(Arc<Owner>);
impl Coordinator {
    /// Native-selected app-data only. Open/bootstrap/recovery happen on the owner
    /// thread; an initialization error leaves the shell and existing DB intact.
    pub fn start(
        directory: PathBuf,
        notify: impl FnMut(ExecutionChanged) -> bool + Send + 'static,
    ) -> Self {
        Self::spawn(
            move || {
                Ok(Box::new(Runtime::new(
                    SqliteStore::open(&directory)?,
                    ProductionClock,
                    RandomIds,
                )?) as Box<dyn ExecutionRuntime>)
            },
            notify,
            CADENCE,
        )
    }
    pub fn disabled() -> Self {
        Self::spawn(|| Err(unavailable()), |_| false, CADENCE)
    }
    fn spawn(
        factory: impl FnOnce() -> StoreResult<Box<dyn ExecutionRuntime>> + Send + 'static,
        notify: impl FnMut(ExecutionChanged) -> bool + Send + 'static,
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
            .name("simulated-execution-owner".into())
            .spawn(move || {
                // A panic drops the runtime and its current reply sender; no false
                // terminal record is invented, and every queued caller is released.
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
    /// Nonblocking try-admission; no waiting queue/task is created on saturation.
    /// Slots conservatively include execution and unconsumed reply storage.
    pub fn submit(&self, command: &str, request: Value) -> StoreResult<Reply> {
        let request = Request::decode(command, request).map_err(public_error)?;
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
    /// Stop admission/production; do not complete queued mutations or fabricate
    /// terminal records on shutdown. A selected operation cannot be preempted.
    /// No force-kill; timeout leaves next-start interruption to SQLite/runtime.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let shared = &self.0.shared;
        shared.stop();
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
pub(super) struct ProductionClock;
impl Clock for ProductionClock {
    fn now_utc(&self) -> Timestamp {
        // Pinned Chrono std feature: no clock/time-zone feature expansion needed.
        let now: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
        now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}
pub(super) struct RandomIds;
impl IdSource for RandomIds {
    fn next_id(&mut self) -> Id {
        uuid::Uuid::new_v4().to_string()
    }
}
struct Observation {
    workspace_id: Id,
    last_health: ExecutionHealth,
    failed: bool,
    pending: bool,
    next_notify: Instant,
}
impl Observation {
    fn sample(&mut self, rt: &mut dyn ExecutionRuntime, shared: &Shared) {
        if let Some(hint) = rt.take_changed() {
            if hint.contract_version == 1 && hint.workspace_id == self.workspace_id {
                self.pending = true;
            } else {
                self.failed = true;
            }
        }
        let health = if self.failed {
            ExecutionHealth::StorageUnavailable
        } else {
            rt.health()
        };
        if health != self.last_health {
            self.pending = true;
            self.last_health = health;
        }
        shared.lock().health = health;
    }
    fn flush(&mut self, notify: &mut impl FnMut(ExecutionChanged) -> bool, cadence: Duration) {
        if self.pending && Instant::now() >= self.next_notify {
            self.pending = !notify(ExecutionChanged {
                contract_version: 1,
                workspace_id: self.workspace_id.clone(),
            });
            self.next_notify = Instant::now() + cadence;
        }
    }
    fn execute(&mut self, job: Job, rt: &mut dyn ExecutionRuntime, shared: &Shared) {
        let available = !self.failed && rt.health() == ExecutionHealth::Ready;
        let result = if !available && job.request.mutation() {
            Err(unavailable())
        } else {
            job.request.execute(rt, available)
        };
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.code == "invalid_execution_response")
        {
            self.failed = true;
        }
        if let Ok(value) = &result {
            if value.get("workspace").is_some_and(|w| {
                w.get("workspace_id").and_then(Value::as_str) != Some(self.workspace_id.as_str())
            }) {
                self.failed = true;
                self.sample(rt, shared);
                job.reply.complete(Err(invalid_response()));
                return;
            }
        }
        self.sample(rt, shared);
        job.reply.complete(result.map_err(public_error));
    }
}
fn run(
    shared: Arc<Shared>,
    factory: impl FnOnce() -> StoreResult<Box<dyn ExecutionRuntime>>,
    mut notify: impl FnMut(ExecutionChanged) -> bool,
    cadence: Duration,
) {
    let initialized = (|| {
        let mut rt = factory()?;
        let initial = rt.bootstrap_workspace(&BootstrapRequest {
            contract_version: 1,
        })?;
        v::validate_workspace_snapshot(&initial).map_err(|_| invalid_response())?;
        if rt.health() != ExecutionHealth::Ready || !initial.execution_available {
            return Err(unavailable());
        }
        Ok((rt, initial.workspace.workspace_id))
    })();
    let (mut rt, workspace_id) = match initialized {
        Ok(value) => value,
        Err(failure) => {
            shared.lock().startup_failure = Some(public_error(failure));
            return;
        }
    };
    let mut observed = Observation {
        workspace_id,
        last_health: ExecutionHealth::Starting,
        failed: false,
        pending: true,
        next_notify: Instant::now(),
    };
    observed.sample(rt.as_mut(), &shared);
    let mut next_tick = Instant::now() + cadence;
    loop {
        if shared.lock().stopping {
            break;
        }
        let mut controls = 0;
        let mut worked = false;
        // Bounded service quantum: up to eight controls and one ordinary call,
        // then at most one due tick. Never queue timer messages or catch up.
        while controls < CONTROL_BURST {
            let Some(job) = shared.pop(true) else {
                break;
            };
            observed.execute(job, rt.as_mut(), &shared);
            controls += 1;
            worked = true;
        }
        if let Some(job) = shared.pop(false) {
            observed.execute(job, rt.as_mut(), &shared);
            worked = true;
        }
        // A cancellation arriving during the ordinary operation gets any unused
        // control quantum BEFORE production is selected. Selected work is final.
        while controls < CONTROL_BURST {
            let Some(job) = shared.pop(true) else {
                break;
            };
            observed.execute(job, rt.as_mut(), &shared);
            controls += 1;
            worked = true;
        }
        observed.flush(&mut notify, cadence);
        if shared.lock().stopping {
            break;
        }
        if observed.last_health == ExecutionHealth::Ready && Instant::now() >= next_tick {
            if rt.tick().is_err() {
                observed.failed = true;
            }
            observed.sample(rt.as_mut(), &shared);
            observed.flush(&mut notify, cadence);
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
        let wait = if observed.last_health == ExecutionHealth::Ready {
            next_tick.saturating_duration_since(Instant::now())
        } else {
            cadence
        };
        // Failed notifications retry only this bounded hint, never a command.
        let wait = if observed.pending {
            wait.min(
                observed
                    .next_notify
                    .saturating_duration_since(Instant::now()),
            )
        } else {
            wait
        };
        let _ = shared
            .wake
            .wait_timeout(state, wait)
            .unwrap_or_else(|e| e.into_inner());
    }
}

#[cfg(test)]
pub(super) fn test_coordinator(
    factory: impl FnOnce() -> StoreResult<Box<dyn ExecutionRuntime>> + Send + 'static,
    notify: impl FnMut(ExecutionChanged) -> bool + Send + 'static,
    cadence: Duration,
) -> Coordinator {
    Coordinator::spawn(factory, notify, cadence)
}
