//! Serialized v2 owner runtime (C30a): one shared FIFO/slot for every adapter.
//! Fake runs advance one bounded step per tick on the owner; Codex runs execute
//! on the supervisor's single worker, never on the owner. Carry-forward rules
//! (Argon C28/C29) implemented here:
//! - store audit + recovery exactly once per open, before admission or peeks;
//! - dispatch arms only from fresh non-replayed admissions, held in memory;
//! - commit `starting` BEFORE dispatch; commit `running` only on `Started`;
//! - persist cancel BEFORE signalling the worker; cancel intent beats success;
//! - quarantine (or storage failure) stops all dispatch for this process.
use crate::{
    execution::ports::{Clock, ExecutionHealth, IdSource, RunSeed, SessionSeed, WorkspaceSeed},
    execution::validation as common,
    execution_v2::{
        self as v2,
        admission::StartRequest,
        ports::{AdmissionJournal, AppendBatch, JournalReader, JournalWriter, RecoveryReport},
        validation as policy, DomainResult,
    },
    supervisor::{
        DispatchArm, Progress, ProviderFactory, Supervisor, SupervisorEvent, WorkerOutcome,
    },
};
use std::{collections::HashMap, time::Duration};

pub mod context;
#[cfg(test)]
mod tests;

/// Store surface the owner needs beyond the shared journal ports.
pub trait OwnedStore: AdmissionJournal + JournalReader + JournalWriter {
    fn bootstrap(&mut self, seed: WorkspaceSeed) -> DomainResult<v2::WorkspaceSnapshot>;
    fn startup_recover(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> DomainResult<RecoveryReport>;
}
impl OwnedStore for crate::storage::store_v2::V2Store {
    fn bootstrap(&mut self, seed: WorkspaceSeed) -> DomainResult<v2::WorkspaceSnapshot> {
        crate::storage::store_v2::V2Store::bootstrap(self, seed)
    }
    fn startup_recover(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> DomainResult<RecoveryReport> {
        crate::storage::store_v2::V2Store::startup_recover(self, now, ids)
    }
}

const FAKE_TEXT: [&str; 3] = [
    "SIMULATED: response from the local fake provider.\n",
    "SIMULATED: this bounded chunk contains display text only.\n",
    "SIMULATED: fake response complete.\n",
];
const WAIT_TEXT: [&str; 2] = [
    "SIMULATED: waiting scenario started.\n",
    "SIMULATED: awaiting a persisted cancellation request.\n",
];

/// One coalesced step of the volatile progress of the active Codex run.
#[derive(serde::Serialize)]
pub struct ProgressStep {
    pub step: &'static str,
    pub count: u32,
}

/// Volatile progress of the active run: in-memory only, never journaled,
/// empty after the run ends or on restart. Text-free by construction.
#[derive(serde::Serialize)]
pub struct RunProgress {
    pub contract_version: u64,
    pub workspace_id: String,
    pub run_id: Option<String>,
    pub steps: Vec<ProgressStep>,
}

enum Active {
    Fake {
        run_id: String,
        scenario: v2::FakeScenario,
        emitted: u8,
    },
    Codex {
        run_id: String,
        generation: u64,
    },
}

fn unavailable(message: &str) -> crate::execution::Error {
    common::error("storage_unavailable", message)
}
fn v2_error(code: &str, message: &str) -> v2::Error {
    v2::Error {
        code: code.into(),
        user_message: message.into(),
        recoverable: true,
    }
}
fn domain(error: &crate::execution::Error) -> bool {
    matches!(
        error.code.as_str(),
        "validation_failed"
            | "unsupported_contract_version"
            | "provider_execution_disabled"
            | "provider_activity_disabled"
            | "codex_unavailable"
            | "request_id_conflict"
            | "workspace_not_found"
            | "session_not_found"
            | "run_not_found"
            | "state_conflict"
            | "capacity_exceeded"
            | "queue_full"
            | "context_unavailable"
            | "unsupported_layout_version"
            | "layout_revision_conflict"
            | "layout_invalid"
    )
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
/// Sanitized, user-facing text for provider failure codes. Never raw output.
fn failure_message(code: &str) -> &'static str {
    match code {
        "codex_timeout" => "Codex did not finish within its time limit.",
        "codex_invalid_executable_pin" => "The pinned Codex CLI is missing or changed.",
        "codex_nonzero_exit" => "Codex exited with an error. Check its login and configuration.",
        "codex_job_containment_failed" => "Codex left background processes; they were stopped.",
        "codex_dispatch_unarmed" => "This run lost its one-time permission and was not started.",
        "codex_unavailable" => "Codex is not available in this workbench.",
        "codex_forbidden_activity" => {
            "Codex tried to use tools or another activity this tool-free profile does not allow. Try a prompt that needs no lookups."
        }
        "codex_invalid_lifecycle" | "codex_invalid_event" | "codex_incomplete_stream" => {
            "Codex replied in an order or shape this profile does not accept."
        }
        "codex_turn_failed" => "Codex reported that the turn failed.",
        "codex_output_limit" => "Codex output exceeded this profile's size limit.",
        "provider_worker_unavailable" => "The Codex worker could not be started.",
        _ if code.starts_with("codex_protocol") || code.starts_with("protocol") => {
            "Codex returned output this profile does not accept."
        }
        _ => "The Codex run failed.",
    }
}

pub struct RuntimeV2<S: OwnedStore, F: ProviderFactory, C: Clock, I: IdSource> {
    store: S,
    clock: C,
    ids: I,
    workspace_id: String,
    supervisor: Supervisor<F>,
    arms: HashMap<String, DispatchArm>,
    active: Option<Active>,
    progress: Vec<ProgressStep>,
    health: ExecutionHealth,
    codex: v2::CodexAvailability,
    changed: bool,
}

impl<S: OwnedStore, F: ProviderFactory, C: Clock, I: IdSource> RuntimeV2<S, F, C, I> {
    /// `codex` is the read-only availability decided natively at startup
    /// (pinned executable verification); it never invokes a model.
    pub fn new(
        mut store: S,
        factory: F,
        codex: v2::CodexAvailability,
        clock: C,
        mut ids: I,
    ) -> DomainResult<Self> {
        let seed = WorkspaceSeed {
            workspace_id: ids.next_id(),
            created_at: clock.now_utc(),
        };
        let initial = store.bootstrap(seed)?;
        let workspace_id = initial.workspace.workspace_id.clone();
        let now = clock.now_utc();
        let report = store.startup_recover(&now, &mut ids)?;
        if report
            .interrupted_runs
            .iter()
            .any(|r| r.workspace_id != workspace_id || r.state != v2::RunState::Interrupted)
        {
            return Err(unavailable(
                "Startup recovery did not interrupt prior work.",
            ));
        }
        Ok(Self {
            store,
            clock,
            ids,
            workspace_id,
            supervisor: Supervisor::new(factory),
            arms: HashMap::new(),
            active: None,
            progress: Vec::new(),
            health: ExecutionHealth::Ready,
            codex,
            changed: !report.committed_events.is_empty(),
        })
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    pub fn health(&self) -> ExecutionHealth {
        self.health
    }
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }
    fn executing(&self) -> bool {
        self.health == ExecutionHealth::Ready && !self.supervisor.is_quarantined()
    }
    fn codex_availability(&self) -> v2::CodexAvailability {
        if self.supervisor.is_quarantined() {
            v2::CodexAvailability::Quarantined
        } else {
            self.codex
        }
    }
    fn selected(&self, workspace_id: &str) -> DomainResult {
        if workspace_id == self.workspace_id {
            Ok(())
        } else {
            Err(common::error(
                "workspace_not_found",
                "Unknown managed workspace.",
            ))
        }
    }
    /// New work is refused while quarantined (Argon C30 observation 1): no
    /// enqueue can sit behind a stopped slot via raw IPC.
    fn admitting(&self) -> DomainResult {
        self.ready()?;
        if self.supervisor.is_quarantined() {
            return Err(common::error(
                "state_conflict",
                "Execution is stopped until restart because cleanup could not be confirmed.",
            ));
        }
        Ok(())
    }
    fn ready(&self) -> DomainResult {
        if self.health == ExecutionHealth::Ready {
            Ok(())
        } else {
            Err(unavailable(
                "Execution is unavailable; reopen storage explicitly.",
            ))
        }
    }
    fn observe<T>(&mut self, result: DomainResult<T>) -> DomainResult<T> {
        if let Err(error) = &result {
            if !domain(error) {
                self.health = ExecutionHealth::StorageUnavailable;
                self.arms.clear();
                // Stop local work; never promote anything after a store failure.
                if let Some(Active::Codex { run_id, .. }) = &self.active {
                    let run_id = run_id.clone();
                    self.supervisor.cancel(&run_id);
                }
            }
        }
        result
    }

    /// Volatile progress of the active Codex run; never touches the store.
    pub fn progress(&self) -> RunProgress {
        let run_id = match &self.active {
            Some(Active::Codex { run_id, .. }) => Some(run_id.clone()),
            _ => None,
        };
        RunProgress {
            contract_version: 2,
            workspace_id: self.workspace_id.clone(),
            steps: if run_id.is_some() {
                self.progress
                    .iter()
                    .map(|s| ProgressStep {
                        step: s.step,
                        count: s.count,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            run_id,
        }
    }

    fn note_progress(&mut self, progress: Progress) {
        let step = progress.code();
        match self.progress.last_mut() {
            Some(last) if last.step == step => last.count = last.count.saturating_add(1),
            _ => self.progress.push(ProgressStep { step, count: 1 }),
        }
        self.changed = true;
    }

    pub fn snapshot(&mut self) -> DomainResult<v2::WorkspaceSnapshot> {
        let request = v2::WorkspaceRequest {
            contract_version: 2,
            workspace_id: self.workspace_id.clone(),
        };
        let result = self.store.workspace_snapshot(&request);
        let mut snapshot = self.observe(result)?;
        snapshot.execution_available = self.executing();
        snapshot.codex_availability = self.codex_availability();
        let valid = policy::validate_workspace(&snapshot)
            .map_err(|_| unavailable("Stored workspace validation failed."));
        self.observe(valid)?;
        Ok(snapshot)
    }

    pub fn bootstrap_workspace(
        &mut self,
        request: &v2::BootstrapRequest,
    ) -> DomainResult<v2::BootstrapResponse> {
        if request.contract_version != 2 {
            return Err(common::error(
                "unsupported_contract_version",
                "Unsupported execution contract version.",
            ));
        }
        Ok(v2::BootstrapResponse {
            contract_version: 2,
            storage_state: v2::StorageState::Ready,
            upgrade_id: None,
            snapshot: Some(self.snapshot()?),
        })
    }

    pub fn create_session(
        &mut self,
        request: &v2::CreateSessionRequest,
    ) -> DomainResult<v2::CreateSessionResponse> {
        policy::validate_create_session(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let seed = SessionSeed {
            session_id: self.ids.next_id(),
            created_at: self.clock.now_utc(),
        };
        let result = self.store.create_session(request, seed);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        Ok(v2::CreateSessionResponse {
            contract_version: 2,
            session: commit.result,
            replayed: commit.replayed,
        })
    }

    fn run_seed(&mut self) -> RunSeed {
        RunSeed {
            run_id: self.ids.next_id(),
            event_id: self.ids.next_id(),
            created_at: self.clock.now_utc(),
        }
    }

    pub fn start_fake_run(
        &mut self,
        request: &v2::StartFakeRunRequest,
    ) -> DomainResult<v2::StartFakeRunResponse> {
        policy::validate_fake_start(request)?;
        self.selected(&request.workspace_id)?;
        self.admitting()?;
        let seed = self.run_seed();
        let result = self.store.enqueue_run(StartRequest::Fake(request), seed);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        Ok(v2::StartFakeRunResponse {
            contract_version: 2,
            run: commit.result,
            replayed: commit.replayed,
        })
    }

    pub fn start_codex_run(
        &mut self,
        request: &v2::StartCodexRunRequest,
    ) -> DomainResult<v2::StartCodexRunResponse> {
        policy::validate_codex_start(request)?;
        self.selected(&request.workspace_id)?;
        self.admitting()?;
        // The text Codex will receive: the prompt, or (v2) the prompt after the carried
        // turns. Composed BEFORE admission so a failure leaves nothing queued; the store
        // re-checks eligibility of the carried runs inside the admission transaction.
        let stdin = self.stdin_text(request)?;
        let seed = self.run_seed();
        let result = self
            .store
            .enqueue_run(StartRequest::CodexText(request), seed);
        let commit = self.observe(result)?;
        // Replays return the immutable receipt and NEVER mint an arm.
        if !commit.replayed {
            self.changed = true;
            if self.codex_availability() == v2::CodexAvailability::Ready {
                if let Some(arm) = Supervisor::<F>::mint_arm(&commit, &stdin) {
                    self.arms.insert(arm.run_id().to_owned(), arm);
                }
            }
            // Unavailable Codex: durable queued run without an arm; tick fails
            // it as `codex_unavailable` when it reaches the head of the queue.
        }
        Ok(v2::StartCodexRunResponse {
            contract_version: 2,
            run: commit.result,
            replayed: commit.replayed,
        })
    }

    /// Earlier turns for `ids` (a prompt and its final reply each), oldest first. Every run
    /// must be a succeeded Codex run of `session_id`; anything else is `context_unavailable`.
    fn context_turns(
        &mut self,
        session_id: &str,
        ids: &[String],
    ) -> DomainResult<Vec<context::Turn>> {
        let workspace_id = self.workspace_id.clone();
        let mut turns = Vec::with_capacity(ids.len());
        for id in ids {
            let found = self.store.get_run(&workspace_id, id);
            let queued = self.observe(found)?;
            let run = &queued.snapshot;
            if run.session_id != session_id
                || run.state != v2::RunState::Succeeded
                || run.provider_id != v2::ProviderId::Codex
            {
                return Err(common::error(
                    "context_unavailable",
                    "Carried runs must be earlier succeeded Codex runs of this session.",
                ));
            }
            let prompt = queued.input.prompt().to_owned();
            let mut reply = String::new();
            let mut after = 0;
            // A reply is at most 16 KiB in chunks of at most 2 KiB, so one page normally holds it.
            // Never treat a partial history as the whole reply: run out of pages -> refuse.
            let mut complete = false;
            for _ in 0..25 {
                let page = self.store.list_run_events(&v2::EventPageRequest {
                    contract_version: 2,
                    workspace_id: workspace_id.clone(),
                    run_id: id.clone(),
                    after_seq: after,
                    through_seq: None,
                    limit: 200,
                });
                let page = self.observe(page)?;
                for event in &page.events {
                    if let v2::RunEvent::TextDelta { payload, .. } = event {
                        if payload.stream == v2::TextStream::Assistant {
                            reply.push_str(&payload.text);
                        }
                    }
                }
                if !page.has_more {
                    complete = true;
                    break;
                }
                after = page.next_after_seq;
            }
            if !complete {
                return Err(common::error(
                    "context_unavailable",
                    "An earlier reply could not be read completely.",
                ));
            }
            turns.push(context::Turn { prompt, reply });
        }
        Ok(turns)
    }

    /// The exact text a `codex_text_v2` start would send, for the user to read BEFORE
    /// consenting. A read only: nothing is admitted, armed or journaled.
    pub fn preview_codex_context(
        &mut self,
        request: &v2::PreviewCodexContextRequest,
    ) -> DomainResult<v2::CodexContextPreview> {
        policy::validate_preview_context(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let turns = self.context_turns(&request.session_id, &request.context_run_ids)?;
        let composed = context::compose(&turns, &request.prompt).ok_or_else(|| {
            common::error("validation_failed", "The prompt is too long to send with context.")
        })?;
        Ok(v2::CodexContextPreview {
            contract_version: 2,
            workspace_id: request.workspace_id.clone(),
            bytes: composed.text.len() as u64,
            text: composed.text,
            carried: composed.carried as u64,
            trimmed: composed.trimmed,
        })
    }

    /// The exact stdin text for a Codex start (see `start_codex_run`).
    fn stdin_text(&mut self, request: &v2::StartCodexRunRequest) -> DomainResult<String> {
        let Some(ids) = request.context_run_ids.as_deref() else {
            return Ok(request.prompt.clone());
        };
        let turns = self.context_turns(&request.session_id, ids)?;
        context::compose(&turns, &request.prompt)
            .map(|composed| composed.text)
            .ok_or_else(|| common::error("validation_failed", "The prompt is too long to send with context."))
    }

    fn event(
        &mut self,
        current: &v2::RunSnapshot,
        payload: v2::EventPayload,
        source: v2::EventSource,
        offset: u64,
    ) -> v2::RunEvent {
        v2::RunEvent::from_parts(
            v2::EventHeader {
                contract_version: 2,
                event_id: self.ids.next_id(),
                workspace_id: current.workspace_id.clone(),
                session_id: current.session_id.clone(),
                run_id: current.run_id.clone(),
                provider_id: current.provider_id,
                source,
                seq: current.last_seq + 1 + offset,
                occurred_at: self.clock.now_utc(),
            },
            payload,
        )
    }

    /// Build a reducer-checked batch from (payload, source) pairs.
    fn batch(
        &mut self,
        current: &v2::RunSnapshot,
        payloads: Vec<(v2::EventPayload, v2::EventSource)>,
    ) -> DomainResult<AppendBatch> {
        let mut next = current.clone();
        let mut events = Vec::with_capacity(payloads.len());
        for (index, (payload, source)) in payloads.into_iter().enumerate() {
            let event = self.event(current, payload, source, index as u64);
            next = policy::apply_event(&next, &event)?;
            events.push(event);
        }
        let batch = AppendBatch {
            workspace_id: current.workspace_id.clone(),
            run_id: current.run_id.clone(),
            expected_last_seq: current.last_seq,
            expected_state: current.state,
            events,
            next_snapshot: next,
        };
        policy::validate_append(current, &batch)?;
        Ok(batch)
    }

    fn change(
        from: v2::RunState,
        to: v2::RunState,
        error: Option<v2::Error>,
    ) -> (v2::EventPayload, v2::EventSource) {
        (
            v2::EventPayload::RunStateChanged(v2::RunStateChangedPayload { from, to, error }),
            v2::EventSource::System,
        )
    }

    fn commit(&mut self, batch: AppendBatch) -> DomainResult<v2::RunSnapshot> {
        let expected = batch.next_snapshot.clone();
        let result = self.store.append_batch(batch);
        let commit = self.observe(result)?;
        if commit.replayed || commit.result != expected {
            return self.observe(Err(unavailable(
                "Append returned an invalid commit acknowledgement.",
            )));
        }
        self.changed = true;
        Ok(expected)
    }

    fn load(&mut self, run_id: &str) -> DomainResult<v2::RunSnapshot> {
        let workspace_id = self.workspace_id.clone();
        let result = self.store.get_run(&workspace_id, run_id);
        Ok(self.observe(result)?.snapshot)
    }

    pub fn cancel_run(
        &mut self,
        request: &v2::CancelRunRequest,
    ) -> DomainResult<v2::CancelRunResponse> {
        policy::validate_cancel(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let current = self.load(&request.run_id)?;
        let target = match current.state {
            v2::RunState::Queued => Some(v2::RunState::Cancelled),
            v2::RunState::Starting | v2::RunState::Running | v2::RunState::WaitingInput => {
                Some(v2::RunState::CancelRequested)
            }
            _ => None,
        };
        let mutation = match target {
            Some(to) => Some(self.batch(&current, vec![Self::change(current.state, to, None)])?),
            None => None,
        };
        let result = self.store.cancel_run(request, mutation);
        let commit = self.observe(result)?;
        if !commit.replayed && !commit.committed_events.is_empty() {
            self.changed = true;
            // Durable intent is committed: only now drop the arm / signal work.
            self.arms.remove(&request.run_id);
            self.supervisor.cancel(&request.run_id);
        }
        Ok(v2::CancelRunResponse {
            contract_version: 2,
            run: commit.result,
            replayed: commit.replayed,
        })
    }

    pub fn list_run_events(
        &mut self,
        request: &v2::EventPageRequest,
    ) -> DomainResult<v2::EventPage> {
        policy::validate_event_page(request)?;
        self.selected(&request.workspace_id)?;
        let result = self.store.list_run_events(request);
        self.observe(result)
    }

    pub fn save_layout(
        &mut self,
        request: &v2::SaveLayoutRequest,
    ) -> DomainResult<v2::SaveLayoutResponse> {
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let result = self.store.save_layout(request);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        Ok(v2::SaveLayoutResponse {
            contract_version: 2,
            layout: commit.result,
            replayed: commit.replayed,
        })
    }

    /// One bounded owner step. Never waits on model or OS I/O.
    pub fn tick(&mut self) -> DomainResult {
        self.ready()?;
        let result = self.advance();
        self.observe(result)
    }

    fn advance(&mut self) -> DomainResult {
        for event in self.supervisor.poll() {
            self.supervised(event)?;
        }
        if !self.executing() {
            return Ok(());
        }
        match self.active.take() {
            Some(Active::Fake {
                run_id,
                scenario,
                emitted,
            }) => self.fake_step(run_id, scenario, emitted),
            Some(active @ Active::Codex { .. }) => {
                self.active = Some(active);
                Ok(())
            }
            None => self.dispatch_next(),
        }
    }

    fn dispatch_next(&mut self) -> DomainResult {
        let workspace_id = self.workspace_id.clone();
        let result = self.store.next_queued(&workspace_id);
        let Some(queued) = self.observe(result)? else {
            return Ok(());
        };
        let current = queued.snapshot;
        if current.state != v2::RunState::Queued || current.last_seq != 1 {
            return Err(unavailable("Dispatch requires a fresh durable queued run."));
        }
        let starting = self.batch(
            &current,
            vec![Self::change(
                v2::RunState::Queued,
                v2::RunState::Starting,
                None,
            )],
        )?;
        match queued.input.scenario() {
            Some(scenario) => {
                self.commit(starting)?;
                self.active = Some(Active::Fake {
                    run_id: current.run_id,
                    scenario,
                    emitted: 0,
                });
                Ok(())
            }
            None => {
                let arm = self.arms.remove(&current.run_id);
                // Commit `starting` BEFORE the arm is offered to the worker.
                let started = self.commit(starting)?;
                let Some(arm) = arm else {
                    let code = if self.codex_availability() == v2::CodexAvailability::Ready {
                        "codex_dispatch_unarmed"
                    } else {
                        "codex_unavailable"
                    };
                    return self.fail(&started, code);
                };
                match self.supervisor.dispatch(arm) {
                    Ok(generation) => {
                        self.active = Some(Active::Codex {
                            run_id: started.run_id,
                            generation,
                        });
                        Ok(())
                    }
                    Err(_) => self.fail(&started, "provider_worker_unavailable"),
                }
            }
        }
    }

    fn fail(&mut self, current: &v2::RunSnapshot, code: &str) -> DomainResult {
        let error = v2_error(code, failure_message(code));
        let batch = self.batch(
            current,
            vec![Self::change(
                current.state,
                v2::RunState::Failed,
                Some(error),
            )],
        )?;
        self.commit(batch)?;
        Ok(())
    }

    fn fake_step(
        &mut self,
        run_id: String,
        scenario: v2::FakeScenario,
        emitted: u8,
    ) -> DomainResult {
        let current = self.load(&run_id)?;
        if terminal(current.state) {
            return Ok(());
        }
        let text = |body: &str| {
            (
                v2::EventPayload::TextDelta(v2::TextDeltaPayload {
                    stream: v2::TextStream::Assistant,
                    text: body.into(),
                }),
                v2::EventSource::Fixture,
            )
        };
        let (payload, progresses) = match current.state {
            v2::RunState::CancelRequested => (
                Self::change(current.state, v2::RunState::Cancelled, None),
                false,
            ),
            v2::RunState::Starting if scenario == v2::FakeScenario::Failure => (
                Self::change(
                    current.state,
                    v2::RunState::Failed,
                    Some(v2_error(
                        "fake_start_failed",
                        "SIMULATED startup failure; no child was launched.",
                    )),
                ),
                false,
            ),
            v2::RunState::Starting => (
                Self::change(current.state, v2::RunState::Running, None),
                false,
            ),
            v2::RunState::Running => {
                let script = match scenario {
                    v2::FakeScenario::Success => FAKE_TEXT.get(emitted as usize),
                    v2::FakeScenario::WaitForCancel => WAIT_TEXT.get(emitted as usize),
                    v2::FakeScenario::Failure => None,
                };
                match (script, scenario) {
                    (Some(body), _) => (text(body), true),
                    (None, v2::FakeScenario::Success) => (
                        Self::change(current.state, v2::RunState::Succeeded, None),
                        false,
                    ),
                    _ => {
                        self.active = Some(Active::Fake {
                            run_id,
                            scenario,
                            emitted,
                        });
                        return Ok(());
                    }
                }
            }
            _ => return Err(unavailable("Fake run is in an unexpected state.")),
        };
        let batch = self.batch(&current, vec![payload])?;
        let next = self.commit(batch)?;
        if !terminal(next.state) {
            self.active = Some(Active::Fake {
                run_id,
                scenario,
                emitted: emitted + u8::from(progresses),
            });
        }
        Ok(())
    }

    fn supervised(&mut self, event: SupervisorEvent) -> DomainResult {
        let (run_id, generation) = match &event {
            SupervisorEvent::Started { run_id, generation }
            | SupervisorEvent::Progress {
                run_id, generation, ..
            }
            | SupervisorEvent::Finished {
                run_id, generation, ..
            }
            | SupervisorEvent::Quarantined {
                run_id, generation, ..
            } => (run_id.clone(), *generation),
        };
        // Only the active run's current generation may mutate the journal.
        let matches = matches!(&self.active,
            Some(Active::Codex { run_id: r, generation: g }) if *r == run_id && *g == generation);
        if !matches {
            return Err(unavailable("Supervisor reported an unrelated run."));
        }
        if let SupervisorEvent::Progress { progress, .. } = event {
            self.note_progress(progress);
            return Ok(());
        }
        let current = self.load(&run_id)?;
        match event {
            SupervisorEvent::Progress { .. } => Ok(()),
            SupervisorEvent::Started { .. } => {
                self.progress.clear();
                // A committed cancel intent stays: never move back to running.
                if current.state == v2::RunState::Starting {
                    let batch = self.batch(
                        &current,
                        vec![Self::change(current.state, v2::RunState::Running, None)],
                    )?;
                    self.commit(batch)?;
                }
                Ok(())
            }
            SupervisorEvent::Finished { outcome, .. } => {
                self.active = None;
                self.progress.clear();
                self.finish(current, outcome)
            }
            SupervisorEvent::Quarantined { code, .. } => {
                self.active = None;
                self.progress.clear();
                self.arms.clear();
                if terminal(current.state) {
                    return Ok(());
                }
                let to = if current.state == v2::RunState::CancelRequested {
                    v2::RunState::Cancelled
                } else {
                    v2::RunState::Failed
                };
                let error = (to == v2::RunState::Failed).then(|| {
                    v2_error(
                        "codex_cleanup_failed",
                        "Codex cleanup could not be confirmed; execution is stopped until restart.",
                    )
                });
                let _ = code;
                let batch = self.batch(&current, vec![Self::change(current.state, to, error)])?;
                self.commit(batch)?;
                Ok(())
            }
        }
    }

    fn finish(&mut self, current: v2::RunSnapshot, outcome: WorkerOutcome) -> DomainResult {
        if terminal(current.state) {
            return Ok(());
        }
        // Committed cancel intent defeats any later success or failure.
        if current.state == v2::RunState::CancelRequested {
            let batch = self.batch(
                &current,
                vec![Self::change(current.state, v2::RunState::Cancelled, None)],
            )?;
            self.commit(batch)?;
            return Ok(());
        }
        match outcome {
            WorkerOutcome::Completed { message, usage }
                if current.state == v2::RunState::Running =>
            {
                let Ok(chunks) = policy::completion_chunks(&message) else {
                    // Empty/oversized message: fail the run, never leave it running.
                    return self.fail(&current, "codex_protocol_message");
                };
                let mut payloads = Vec::new();
                for chunk in chunks {
                    payloads.push((
                        v2::EventPayload::TextDelta(v2::TextDeltaPayload {
                            stream: v2::TextStream::Assistant,
                            text: chunk.into(),
                        }),
                        v2::EventSource::Provider,
                    ));
                }
                payloads.push((
                    v2::EventPayload::UsageUpdated(v2::Usage {
                        input_tokens: Some(usage.input_tokens),
                        output_tokens: Some(usage.output_tokens),
                        estimated: false,
                        cached_input_tokens: Some(usage.cached_input_tokens),
                        cache_write_input_tokens: usage.cache_write_input_tokens,
                        reasoning_output_tokens: Some(usage.reasoning_output_tokens),
                    }),
                    v2::EventSource::Provider,
                ));
                payloads.push(Self::change(current.state, v2::RunState::Succeeded, None));
                let batch = self.batch(&current, payloads)?;
                self.commit(batch)?;
                Ok(())
            }
            // Completion without an observed in-Job start is not trusted.
            WorkerOutcome::Completed { .. } => self.fail(&current, "codex_protocol_unstarted"),
            WorkerOutcome::Failed { code } => self.fail(&current, code),
        }
    }

    /// Development-phase close behavior (user, 2026-10-21): cancel the active
    /// run cleanly, persist the intent, stop the worker within `timeout`, and
    /// commit the terminal state if teardown finishes in time. Otherwise the
    /// next open interrupts it. Revisit before any release.
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        if let Some(Active::Codex { run_id, .. }) = &self.active {
            let run_id = run_id.clone();
            if self.health == ExecutionHealth::Ready {
                let request = v2::CancelRunRequest {
                    contract_version: 2,
                    workspace_id: self.workspace_id.clone(),
                    run_id: run_id.clone(),
                    request_id: self.ids.next_id(),
                };
                let _ = self.cancel_run(&request);
            } else {
                self.supervisor.cancel(&run_id);
            }
            let deadline = std::time::Instant::now() + timeout;
            while matches!(self.active, Some(Active::Codex { .. }))
                && std::time::Instant::now() < deadline
            {
                for event in self.supervisor.poll() {
                    if self.supervised(event).is_err() {
                        self.active = None;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let remaining = timeout.min(Duration::from_secs(1));
        // Simulated runs own no process; the next open interrupts them (v1 parity).
        self.supervisor.shutdown(remaining) && !matches!(self.active, Some(Active::Codex { .. }))
    }
}
