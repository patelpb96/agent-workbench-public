//! Bounded provider supervision (C29b). One dispatch slot, one owned worker
//! thread, volatile run-bound dispatch arms, and quarantine on any unproven
//! teardown, panic or disconnect. The worker holds no store handle, commits
//! nothing, emits no IPC and cannot enqueue; the serialized owner polls events,
//! validates run identity and generation, and commits through the journal.
//!
//! Ordering obligations stay with the owner (C30): commit `starting` before
//! `dispatch`, commit `running` only on `Started`, persist `cancel_requested`
//! before `cancel`, and never promote another queued run while quarantined.
use crate::execution_v2::{self as v2, ports::MutationCommit};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
mod codex_factory;
#[cfg(windows)]
pub use codex_factory::CodexFactory;

#[cfg(test)]
mod tests;

/// Above the decoder's event bound plus Started and Finished, so a run's
/// messages never fill the channel and block the worker before the owner polls.
const CHANNEL_CAPACITY: usize = 72;

/// Cooperative cancellation shared with the provider; provider-agnostic.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn signal(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_signalled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Reported token counters from a completed turn. Reported is not billing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReportedUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cache_write_input_tokens: Option<u64>,
}

// No Debug/Serialize: completed outcomes carry private assistant output.
pub enum WorkerOutcome {
    Completed {
        message: String,
        usage: ReportedUsage,
    },
    /// Sanitized code only (e.g. `codex_timeout`, `codex_cancelled`).
    Failed { code: &'static str },
}

/// Coarse, text-free lifecycle progress of an active run. Volatile display
/// data only: never persisted, never part of the journal or its contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    ThreadStarted,
    TurnStarted,
    ReasoningObserved,
    MessageComplete,
}

impl Progress {
    pub fn code(self) -> &'static str {
        match self {
            Self::ThreadStarted => "thread_started",
            Self::TurnStarted => "turn_started",
            Self::ReasoningObserved => "reasoning_observed",
            Self::MessageComplete => "message_complete",
        }
    }
}

/// What a provider returns. `teardown_proven` must be true only when the
/// provider has independently verified process-tree quiescence AND removal of
/// its owned workspace; anything else quarantines the slot.
pub struct WorkerReport {
    pub outcome: WorkerOutcome,
    pub teardown_proven: bool,
}

/// Native-only injected provider. Runs on the worker thread. Must call
/// `started` at most once, only after its child is running inside containment.
pub trait ProviderFactory: Send + Sync + 'static {
    fn run(&self, prompt: &str, cancel: &Cancel, started: &mut dyn FnMut()) -> WorkerReport;

    /// As `run`, also reporting best-effort progress (must not block). Providers
    /// without progress keep this default.
    fn run_reporting(
        &self,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        _progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        self.run(prompt, cancel, started)
    }

    /// As `run_reporting`, for the profile the run was admitted under. Providers
    /// with a single profile keep this default.
    fn run_profiled(
        &self,
        _profile: v2::CodexProfileId,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        self.run_reporting(prompt, cancel, started, progress)
    }
}

/// Volatile, run-bound permission to dispatch exactly once. Never persisted,
/// cloned, serialized or reconstructed from storage: minted only from a FRESH
/// (non-replayed) committed admission, consumed by `dispatch` before any pin
/// verification or spawn. A restart or replay can never produce one.
// No Debug/Clone/Serialize: carries the private prompt and is single-use.
pub struct DispatchArm {
    workspace_id: String,
    run_id: String,
    prompt: String,
    profile: v2::CodexProfileId,
}
impl DispatchArm {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchRefused {
    Busy,
    Quarantined,
    /// Worker thread could not be created. No child exists; the arm is spent.
    WorkerUnavailable,
}

// No Debug: Finished carries private output.
pub enum SupervisorEvent {
    /// Child resumed inside containment: the owner may commit `running`.
    Started { run_id: String, generation: u64 },
    /// Volatile lifecycle milestone while the child runs; display only.
    Progress {
        run_id: String,
        generation: u64,
        progress: Progress,
    },
    /// Worker returned with proven teardown; the slot is free again.
    Finished {
        run_id: String,
        generation: u64,
        outcome: WorkerOutcome,
    },
    /// Teardown unproven, panic or disconnect. The slot stays quarantined until
    /// restart; the owner must fail the run and stop all new execution.
    Quarantined {
        run_id: String,
        generation: u64,
        code: &'static str,
    },
}

enum Message {
    Started,
    Progress(Progress),
    Finished(WorkerReport),
}

struct Active {
    run_id: String,
    generation: u64,
    cancel: Cancel,
    receiver: mpsc::Receiver<(u64, Message)>,
    handle: Option<thread::JoinHandle<()>>,
}

enum Slot {
    Idle,
    Active(Active),
    /// Holds any still-running worker handle: never detach an untracked worker.
    Quarantined(Option<thread::JoinHandle<()>>),
}

pub struct Supervisor<F: ProviderFactory> {
    factory: Arc<F>,
    slot: Slot,
    next_generation: u64,
}

impl<F: ProviderFactory> Supervisor<F> {
    pub fn new(factory: F) -> Self {
        Self {
            factory: Arc::new(factory),
            slot: Slot::Idle,
            next_generation: 1,
        }
    }

    /// Mint an arm only from a fresh durable admission still in `queued`.
    /// Replays, reads and recovery never call this successfully.
    pub fn mint_arm(commit: &MutationCommit<v2::RunSnapshot>, prompt: &str) -> Option<DispatchArm> {
        let fresh = !commit.replayed
            && commit.result.state == v2::RunState::Queued
            && commit.result.last_seq == 1
            && matches!(
                commit.committed_events.as_slice(),
                [v2::RunEvent::RunCreated { .. }]
            );
        fresh.then(|| DispatchArm {
            workspace_id: commit.result.workspace_id.clone(),
            run_id: commit.result.run_id.clone(),
            prompt: prompt.to_owned(),
            profile: commit
                .result
                .profile_id
                .unwrap_or(v2::CodexProfileId::CodexTextV1),
        })
    }

    pub fn is_available(&self) -> bool {
        matches!(self.slot, Slot::Idle)
    }
    pub fn is_quarantined(&self) -> bool {
        matches!(self.slot, Slot::Quarantined(_))
    }
    pub fn active_run(&self) -> Option<(&str, u64)> {
        match &self.slot {
            Slot::Active(a) => Some((&a.run_id, a.generation)),
            _ => None,
        }
    }

    /// Consumes the arm unconditionally (even when refused): an arm is spent
    /// once offered, so a refused dispatch can never be retried from it.
    pub fn dispatch(&mut self, arm: DispatchArm) -> Result<u64, DispatchRefused> {
        match self.slot {
            Slot::Idle => {}
            Slot::Active(_) => return Err(DispatchRefused::Busy),
            Slot::Quarantined(_) => return Err(DispatchRefused::Quarantined),
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        let (sender, receiver) = mpsc::sync_channel::<(u64, Message)>(CHANNEL_CAPACITY);
        let cancel = Cancel::default();
        let factory = Arc::clone(&self.factory);
        let worker_cancel = cancel.clone();
        let DispatchArm {
            run_id,
            prompt,
            profile,
            ..
        } = arm;
        let spawned = thread::Builder::new()
            .name("provider-worker".into())
            .spawn(move || {
                let started_sender = sender.clone();
                let progress_sender = sender.clone();
                let mut started_once = false;
                let mut started = || {
                    if !started_once {
                        started_once = true;
                        let _ = started_sender.send((generation, Message::Started));
                    }
                };
                // A panic sends nothing: the owner sees a disconnect and quarantines.
                if let Ok(report) = catch_unwind(AssertUnwindSafe(|| {
                    let mut progress = |p: Progress| {
                        // Best-effort: a full channel drops display data, never blocks.
                        let _ = progress_sender.try_send((generation, Message::Progress(p)));
                    };
                    factory.run_profiled(
                        profile,
                        &prompt,
                        &worker_cancel,
                        &mut started,
                        &mut progress,
                    )
                })) {
                    let _ = sender.send((generation, Message::Finished(report)));
                }
            });
        match spawned {
            Ok(handle) => {
                self.slot = Slot::Active(Active {
                    run_id,
                    generation,
                    cancel,
                    receiver,
                    handle: Some(handle),
                });
                Ok(generation)
            }
            Err(_) => Err(DispatchRefused::WorkerUnavailable),
        }
    }

    /// Signal the active worker if it is running `run_id`. The owner must have
    /// committed the cancel intent first; this only stops local execution.
    pub fn cancel(&mut self, run_id: &str) -> bool {
        match &self.slot {
            Slot::Active(a) if a.run_id == run_id => {
                a.cancel.signal();
                true
            }
            _ => false,
        }
    }

    /// Non-blocking. Never waits on model or OS I/O.
    pub fn poll(&mut self) -> Vec<SupervisorEvent> {
        let mut events = Vec::new();
        loop {
            let Slot::Active(active) = &mut self.slot else {
                return events;
            };
            match active.receiver.try_recv() {
                Ok((generation, _)) if generation != active.generation => {
                    // Impossible with one slot and per-dispatch channels; treat a
                    // foreign generation as corruption, never as a result.
                    let (run_id, generation) = (active.run_id.clone(), active.generation);
                    self.quarantine(&mut events, run_id, generation, "supervisor_generation");
                    return events;
                }
                Ok((_, Message::Started)) => events.push(SupervisorEvent::Started {
                    run_id: active.run_id.clone(),
                    generation: active.generation,
                }),
                Ok((_, Message::Progress(progress))) => events.push(SupervisorEvent::Progress {
                    run_id: active.run_id.clone(),
                    generation: active.generation,
                    progress,
                }),
                Ok((_, Message::Finished(report))) => {
                    let (run_id, generation) = (active.run_id.clone(), active.generation);
                    if report.teardown_proven {
                        if let Some(handle) = active.handle.take() {
                            let _ = handle.join();
                        }
                        self.slot = Slot::Idle;
                        events.push(SupervisorEvent::Finished {
                            run_id,
                            generation,
                            outcome: report.outcome,
                        });
                    } else {
                        self.quarantine(
                            &mut events,
                            run_id,
                            generation,
                            "provider_teardown_unproven",
                        );
                    }
                    return events;
                }
                Err(mpsc::TryRecvError::Empty) => return events,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let (run_id, generation) = (active.run_id.clone(), active.generation);
                    self.quarantine(&mut events, run_id, generation, "provider_worker_lost");
                    return events;
                }
            }
        }
    }

    fn quarantine(
        &mut self,
        events: &mut Vec<SupervisorEvent>,
        run_id: String,
        generation: u64,
        code: &'static str,
    ) {
        let handle = match std::mem::replace(&mut self.slot, Slot::Quarantined(None)) {
            Slot::Active(mut a) => {
                a.cancel.signal();
                a.handle.take()
            }
            Slot::Quarantined(h) => h,
            Slot::Idle => None,
        };
        self.slot = Slot::Quarantined(handle);
        events.push(SupervisorEvent::Quarantined {
            run_id,
            generation,
            code,
        });
    }

    /// Stop admission is the owner's job; this cancels active work and waits up
    /// to `timeout` for the worker to finish. Returns false if a worker is still
    /// running (its handle is retained, never detached) or proof is missing.
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        if let Slot::Active(a) = &self.slot {
            a.cancel.signal();
        }
        let deadline = Instant::now() + timeout;
        loop {
            let finished = match &self.slot {
                Slot::Idle => return true,
                Slot::Quarantined(handle) => handle.as_ref().is_none_or(|h| h.is_finished()),
                Slot::Active(_) => {
                    let _ = self.poll();
                    false
                }
            };
            if finished {
                if let Slot::Quarantined(handle) = &mut self.slot {
                    if let Some(h) = handle.take() {
                        let _ = h.join();
                    }
                }
                return false; // quarantined: teardown was not proven
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
