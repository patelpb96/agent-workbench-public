//! Serialized execution coordinator. Integration supplies the mailbox and timer.
use crate::execution::{
    ports::{
        AppendBatch, Clock, ExecutionHealth, ExecutionRuntime, IdSource, JournalStore, RunSeed,
        SessionSeed, StoreResult, WorkspaceSeed,
    },
    validation as v, *,
};

mod fake;
use fake::{Budget, BudgetCheck, BudgetLimits, Discard};

#[cfg(test)]
mod tests;

#[derive(Clone)]
struct Active {
    run_id: Id,
    scenario: FakeScenario,
    emitted: u8,
    budget: Budget,
}

struct Prepared {
    batch: AppendBatch,
    budget: Budget,
    truncated: bool,
}

pub struct Runtime<S: JournalStore, C: Clock, I: IdSource> {
    store: S,
    clock: C,
    ids: I,
    workspace_id: Id,
    active: Option<Active>,
    health: ExecutionHealth,
    changed: bool,
}

fn unavailable(message: &str) -> Error {
    v::error("storage_unavailable", message)
}

// These are unsuccessful user/domain operations, not evidence of a failed store.
fn domain_error(code: &str) -> bool {
    matches!(
        code,
        "validation_failed"
            | "unsupported_contract_version"
            | "provider_execution_disabled"
            | "request_id_conflict"
            | "workspace_not_found"
            | "session_not_found"
            | "run_not_found"
            | "state_conflict"
            | "capacity_exceeded"
            | "queue_full"
            | "unsupported_layout_version"
            | "layout_revision_conflict"
            | "layout_invalid"
    )
}

fn cancel_target(state: RunState) -> Option<RunState> {
    match state {
        RunState::Queued => Some(RunState::Cancelled),
        RunState::Starting | RunState::Running | RunState::WaitingInput => {
            Some(RunState::CancelRequested)
        }
        _ => None,
    }
}

impl<S: JournalStore, C: Clock, I: IdSource> Runtime<S, C, I> {
    pub fn new(mut store: S, clock: C, mut ids: I) -> StoreResult<Self> {
        let seed = WorkspaceSeed {
            workspace_id: ids.next_id(),
            created_at: clock.now_utc(),
        };
        v::validate_id(&seed.workspace_id)?;
        v::validate_timestamp(&seed.created_at)?;
        let initial = store.bootstrap(seed)?;
        v::validate_workspace_snapshot(&initial)
            .map_err(|_| unavailable("Stored workspace validation failed."))?;
        let workspace_id = initial.workspace.workspace_id;
        let now = clock.now_utc();
        v::validate_timestamp(&now)?;
        let recovery = store.startup_recover(&now, &mut ids)?;
        for run in &recovery.interrupted_runs {
            v::validate_snapshot(run)
                .map_err(|_| unavailable("Startup recovery returned an invalid run."))?;
            if run.workspace_id != workspace_id || run.state != RunState::Interrupted {
                return Err(unavailable(
                    "Startup recovery did not interrupt prior work.",
                ));
            }
        }
        for event in &recovery.committed_events {
            v::validate_event(event)
                .map_err(|_| unavailable("Startup recovery returned an invalid event."))?;
            if event.header().workspace_id != workspace_id
                || !matches!(event, RunEvent::RunRecovered { .. })
            {
                return Err(unavailable("Startup recovery returned unrelated events."));
            }
        }
        Ok(Self {
            store,
            clock,
            ids,
            workspace_id,
            active: None,
            health: ExecutionHealth::Ready,
            changed: !recovery.committed_events.is_empty(),
        })
    }

    fn selected(&self, workspace_id: &str) -> StoreResult<()> {
        if workspace_id == self.workspace_id {
            Ok(())
        } else {
            Err(v::error(
                "workspace_not_found",
                "Unknown managed workspace.",
            ))
        }
    }

    fn ready(&self) -> StoreResult<()> {
        if self.health == ExecutionHealth::Ready {
            Ok(())
        } else {
            Err(unavailable(
                "Execution is unavailable; reopen storage explicitly.",
            ))
        }
    }

    fn observe<T>(&mut self, result: StoreResult<T>) -> StoreResult<T> {
        if let Err(error) = &result {
            if !domain_error(&error.code) {
                self.health = ExecutionHealth::StorageUnavailable;
                self.active = None;
            }
        }
        result
    }

    fn snapshot(&mut self) -> StoreResult<WorkspaceSnapshot> {
        let request = WorkspaceRequest {
            contract_version: 1,
            workspace_id: self.workspace_id.clone(),
        };
        let result = self.store.workspace_snapshot(&request);
        let mut snapshot = self.observe(result)?;
        let valid = v::validate_workspace_snapshot(&snapshot)
            .map_err(|_| unavailable("Stored workspace validation failed."));
        self.observe(valid)?;
        if snapshot.workspace.workspace_id != self.workspace_id {
            return self.observe(Err(unavailable("Storage returned another workspace.")));
        }
        snapshot.execution_available = self.health == ExecutionHealth::Ready;
        Ok(snapshot)
    }

    fn check_run(&self, queued: &QueuedRun, run_id: Option<&str>) -> StoreResult<()> {
        v::validate_snapshot(&queued.snapshot)
            .map_err(|_| unavailable("Stored run validation failed."))?;
        if queued.snapshot.workspace_id != self.workspace_id
            || run_id.is_some_and(|id| id != queued.snapshot.run_id)
        {
            return Err(unavailable("Storage returned an unrelated run."));
        }
        Ok(())
    }

    fn load_run(&mut self, run_id: &str) -> StoreResult<QueuedRun> {
        let result = self.store.get_run(&self.workspace_id, run_id);
        let queued = self.observe(result)?;
        let valid = self.check_run(&queued, Some(run_id));
        self.observe(valid)?;
        Ok(queued)
    }

    fn event(
        &mut self,
        current: &RunSnapshot,
        payload: EventPayload,
        source: EventSource,
    ) -> StoreResult<RunEvent> {
        let seq = current
            .last_seq
            .checked_add(1)
            .ok_or_else(|| unavailable("Run sequence capacity exhausted."))?;
        v::validate_counter(seq)?;
        let event = RunEvent::from_parts(
            EventHeader {
                contract_version: 1,
                event_id: self.ids.next_id(),
                workspace_id: current.workspace_id.clone(),
                session_id: current.session_id.clone(),
                run_id: current.run_id.clone(),
                provider_id: ProviderId::Fake,
                source,
                seq,
                occurred_at: self.clock.now_utc(),
            },
            payload,
        );
        v::validate_event(&event)?;
        Ok(event)
    }

    fn batch(current: &RunSnapshot, events: Vec<RunEvent>) -> StoreResult<AppendBatch> {
        let mut next = current.clone();
        for event in &events {
            next = v::apply_event(&next, event)?;
        }
        let batch = AppendBatch {
            workspace_id: current.workspace_id.clone(),
            run_id: current.run_id.clone(),
            expected_last_seq: current.last_seq,
            expected_state: current.state,
            events,
            next_snapshot: next,
        };
        v::validate_append(current, &batch)?;
        Ok(batch)
    }

    fn stop_batch(
        &mut self,
        current: &RunSnapshot,
        budget: &Budget,
        discarded: Discard,
        limits: BudgetLimits,
    ) -> StoreResult<Prepared> {
        // At most three markers and one stop transition; never retain discarded output.
        let mut events = Vec::with_capacity(4);
        let mut cursor = current.clone();
        for (index, scope) in [
            TruncationScope::PendingBytes,
            TruncationScope::PersistedTextBytes,
            TruncationScope::EventCount,
        ]
        .into_iter()
        .enumerate()
        {
            if !discarded.scopes[index] {
                continue;
            }
            let bytes = match scope {
                TruncationScope::PendingBytes => Some(discarded.encoded_bytes as u64),
                TruncationScope::PersistedTextBytes => Some(discarded.text_bytes as u64),
                TruncationScope::EventCount => None,
            };
            let event = self.event(
                &cursor,
                EventPayload::OutputTruncated(OutputTruncatedPayload {
                    scope,
                    discarded_bytes: bytes,
                    discarded_events: discarded.events as u64,
                }),
                EventSource::System,
            )?;
            cursor = v::apply_event(&cursor, &event)?;
            events.push(event);
        }
        if let Some(to) = cancel_target(cursor.state) {
            let event = self.event(
                &cursor,
                EventPayload::RunStateChanged(RunStateChangedPayload {
                    from: cursor.state,
                    to,
                    error: None,
                }),
                EventSource::System,
            )?;
            events.push(event);
        } else if cursor.state != RunState::CancelRequested {
            return Err(unavailable(
                "Output overflow has no valid stopping transition.",
            ));
        }
        let next_budget = match budget.check(&events, limits)? {
            BudgetCheck::Accept(next) => next,
            BudgetCheck::Discard(_) => {
                return Err(unavailable("Reserved stop delivery exceeded its budget."));
            }
        };
        Ok(Prepared {
            batch: Self::batch(current, events)?,
            budget: next_budget,
            truncated: true,
        })
    }

    fn prepare(
        &mut self,
        current: &RunSnapshot,
        budget: &Budget,
        payload: EventPayload,
        limits: BudgetLimits,
    ) -> StoreResult<Prepared> {
        let source = match &payload {
            EventPayload::RunStateChanged(change)
                if matches!(change.to, RunState::CancelRequested | RunState::Cancelled) =>
            {
                EventSource::System
            }
            _ => EventSource::Fixture,
        };
        let event = self.event(current, payload, source)?;
        match budget.check(std::slice::from_ref(&event), limits)? {
            BudgetCheck::Accept(next) => Ok(Prepared {
                batch: Self::batch(current, vec![event])?,
                budget: next,
                truncated: false,
            }),
            BudgetCheck::Discard(discarded) => self.stop_batch(current, budget, discarded, limits),
        }
    }

    fn advance(&mut self) -> StoreResult<()> {
        let (mut active, queued) = if let Some(active) = self.active.clone() {
            let queued = self.load_run(&active.run_id)?;
            if queued.scenario != active.scenario {
                return Err(unavailable(
                    "Stored fake scenario changed during execution.",
                ));
            }
            (active, queued)
        } else {
            let result = self.store.next_queued(&self.workspace_id);
            let Some(queued) = self.observe(result)? else {
                return Ok(());
            };
            self.check_run(&queued, None)?;
            if queued.snapshot.state != RunState::Queued || queued.snapshot.last_seq != 1 {
                return Err(unavailable("Dispatch requires a fresh durable queued run."));
            }
            (
                Active {
                    run_id: queued.snapshot.run_id.clone(),
                    scenario: queued.scenario,
                    emitted: 0,
                    budget: Budget::fresh(),
                },
                queued,
            )
        };
        let current = queued.snapshot;
        if v::is_terminal(current.state) {
            self.active = None;
            return Ok(());
        }
        let Some(payload) = fake::decide(current.state, active.scenario, active.emitted) else {
            return Ok(());
        };
        let progresses = matches!(
            &payload,
            EventPayload::TextDelta(_) | EventPayload::UsageUpdated(_)
        );
        let prepared = self.prepare(
            &current,
            &active.budget,
            payload,
            BudgetLimits::production(),
        )?;
        let expected = prepared.batch.next_snapshot.clone();
        let result = self.store.append_batch(prepared.batch);
        let commit = self.observe(result)?;
        if commit.replayed || commit.result != expected || commit.committed_events.is_empty() {
            return Err(unavailable(
                "Producer append returned an invalid commit acknowledgement.",
            ));
        }
        self.changed = true;
        if v::is_terminal(expected.state) {
            self.active = None;
        } else {
            active.budget = prepared.budget;
            if progresses && !prepared.truncated {
                active.emitted += 1;
            }
            self.active = Some(active);
        }
        Ok(())
    }
}

impl<S: JournalStore, C: Clock, I: IdSource> ExecutionRuntime for Runtime<S, C, I> {
    fn bootstrap_workspace(
        &mut self,
        request: &BootstrapRequest,
    ) -> StoreResult<WorkspaceSnapshot> {
        v::validate_version(request.contract_version)?;
        self.snapshot()
    }

    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
    ) -> StoreResult<CreateSessionResponse> {
        v::validate_create_session(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let seed = SessionSeed {
            session_id: self.ids.next_id(),
            created_at: self.clock.now_utc(),
        };
        v::validate_id(&seed.session_id)?;
        v::validate_timestamp(&seed.created_at)?;
        let result = self.store.create_session(request, seed);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        Ok(CreateSessionResponse {
            contract_version: 1,
            session: commit.result,
            replayed: commit.replayed,
        })
    }

    fn start_fake_run(
        &mut self,
        request: &StartFakeRunRequest,
    ) -> StoreResult<StartFakeRunResponse> {
        v::validate_start_fake_run(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let seed = RunSeed {
            run_id: self.ids.next_id(),
            event_id: self.ids.next_id(),
            created_at: self.clock.now_utc(),
        };
        v::validate_id(&seed.run_id)?;
        v::validate_id(&seed.event_id)?;
        v::validate_timestamp(&seed.created_at)?;
        let result = self.store.enqueue_run(request, seed);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        // No dispatch from the receipt, including its immutable old run snapshot.
        Ok(StartFakeRunResponse {
            contract_version: 1,
            run: commit.result,
            replayed: commit.replayed,
        })
    }

    fn cancel_run(&mut self, request: &CancelRunRequest) -> StoreResult<CancelRunResponse> {
        v::validate_cancel(request)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let current = self.load_run(&request.run_id)?.snapshot;
        let mutation = if let Some(to) = cancel_target(current.state) {
            let event = self.event(
                &current,
                EventPayload::RunStateChanged(RunStateChangedPayload {
                    from: current.state,
                    to,
                    error: None,
                }),
                EventSource::System,
            )?;
            Some(Self::batch(&current, vec![event])?)
        } else {
            None
        };
        // The store checks receipts before the candidate append precondition.
        let result = self.store.cancel_run(request, mutation);
        let commit = self.observe(result)?;
        if !commit.replayed && !commit.committed_events.is_empty() {
            self.changed = true;
            if let Some(active) = self.active.as_ref() {
                if active.run_id == request.run_id {
                    let checked = active
                        .budget
                        .check(&commit.committed_events, BudgetLimits::production());
                    let checked = self.observe(checked)?;
                    let next = match checked {
                        BudgetCheck::Accept(next) => next,
                        BudgetCheck::Discard(_) => {
                            return self.observe(Err(unavailable(
                                "Cancellation exceeded control capacity.",
                            )));
                        }
                    };
                    if let Some(active) = self.active.as_mut() {
                        active.budget = next;
                    }
                }
            }
        }
        Ok(CancelRunResponse {
            contract_version: 1,
            run: commit.result,
            replayed: commit.replayed,
        })
    }

    fn get_workspace_snapshot(
        &mut self,
        request: &WorkspaceRequest,
    ) -> StoreResult<WorkspaceSnapshot> {
        v::validate_workspace_request(request)?;
        self.selected(&request.workspace_id)?;
        self.snapshot()
    }

    fn list_run_events(&mut self, request: &EventPageRequest) -> StoreResult<EventPage> {
        v::validate_event_page(request)?;
        self.selected(&request.workspace_id)?;
        let result = self.store.list_run_events(request);
        self.observe(result)
    }

    fn save_layout(&mut self, request: &SaveLayoutRequest) -> StoreResult<SaveLayoutResponse> {
        v::validate_version(request.contract_version)?;
        v::validate_id(&request.workspace_id)?;
        v::validate_id(&request.request_id)?;
        v::validate_counter(request.expected_revision)?;
        self.selected(&request.workspace_id)?;
        self.ready()?;
        let snapshot = self.snapshot()?;
        v::validate_save_layout(request, &snapshot.sessions)?;
        // Existing-layout/revision preconditions remain behind receipt lookup.
        let result = self.store.save_layout(request);
        let commit = self.observe(result)?;
        self.changed |= !commit.replayed;
        Ok(SaveLayoutResponse {
            contract_version: 1,
            layout: commit.result,
            replayed: commit.replayed,
        })
    }

    fn tick(&mut self) -> StoreResult<()> {
        self.ready()?;
        let result = self.advance();
        self.observe(result)
    }

    fn take_changed(&mut self) -> Option<ExecutionChanged> {
        if std::mem::take(&mut self.changed) {
            Some(ExecutionChanged {
                contract_version: 1,
                workspace_id: self.workspace_id.clone(),
            })
        } else {
            None
        }
    }

    fn health(&self) -> ExecutionHealth {
        self.health
    }
}
