use super::*;
use crate::execution::ports::{MutationCommit, RecoveryReport};

const NOW: &str = "2026-10-19T19:00:00.000Z";
fn id(n: u64) -> Id {
    format!("00000000-0000-4000-8000-{n:012x}")
}
struct TestClock;
impl Clock for TestClock {
    fn now_utc(&self) -> Timestamp {
        NOW.into()
    }
}
struct TestIds(u64);
impl IdSource for TestIds {
    fn next_id(&mut self) -> Id {
        let value = id(self.0);
        self.0 += 1;
        value
    }
}

type Receipt = (Id, String, RunSnapshot);

// Behavioral test double only. It deliberately does not model SQLite transactions,
// durability, ownership, cumulative store budgets, or crash/failure atomicity.
struct TestStore {
    workspace: WorkspaceSnapshot,
    runs: Vec<QueuedRun>,
    events: Vec<RunEvent>,
    starts: Vec<Receipt>,
    cancels: Vec<Receipt>,
    bootstraps: usize,
    recoveries: usize,
    next_calls: usize,
    enqueue_calls: usize,
    append_failure: Option<&'static str>,
    enqueue_failure: Option<&'static str>,
}

impl TestStore {
    fn new() -> Self {
        Self {
            workspace: WorkspaceSnapshot {
                contract_version: 1,
                workspace: WorkspaceSnapshotRecord {
                    workspace_id: id(1),
                    label: "Managed simulation".into(),
                    created_at: NOW.into(),
                },
                sessions: vec![SessionSnapshot {
                    contract_version: 1,
                    workspace_id: id(1),
                    session_id: id(2),
                    provider_id: ProviderId::Fake,
                    label: "Test session".into(),
                    created_at: NOW.into(),
                }],
                runs: vec![],
                layout: Some(Layout {
                    layout_version: 1,
                    workspace_id: id(1),
                    revision: 0,
                    theme: ThemeName::Dark,
                    active_module: ModuleName::Agents,
                    sidebar_width: 300.0,
                    panels: vec![],
                    containers: vec![],
                }),
                layout_warning: None,
                execution_available: false,
            },
            runs: vec![],
            events: vec![],
            starts: vec![],
            cancels: vec![],
            bootstraps: 0,
            recoveries: 0,
            next_calls: 0,
            enqueue_calls: 0,
            append_failure: None,
            enqueue_failure: None,
        }
    }

    fn view(&self) -> WorkspaceSnapshot {
        let mut view = self.workspace.clone();
        view.runs = self.runs.iter().map(|run| run.snapshot.clone()).collect();
        view
    }

    fn receipt(
        receipts: &[Receipt],
        request_id: &str,
        args: &str,
    ) -> StoreResult<Option<MutationCommit<RunSnapshot>>> {
        if let Some((_, original_args, result)) = receipts.iter().find(|r| r.0 == request_id) {
            if original_args != args {
                return Err(v::error(
                    "request_id_conflict",
                    "Request arguments conflict.",
                ));
            }
            return Ok(Some(MutationCommit {
                result: result.clone(),
                committed_events: vec![],
                replayed: true,
            }));
        }
        Ok(None)
    }
}

impl JournalStore for TestStore {
    fn bootstrap(&mut self, _seed: WorkspaceSeed) -> StoreResult<WorkspaceSnapshot> {
        self.bootstraps += 1;
        Ok(self.view())
    }

    fn startup_recover(
        &mut self,
        now: &str,
        ids: &mut dyn IdSource,
    ) -> StoreResult<RecoveryReport> {
        self.recoveries += 1;
        if self.recoveries > 1 {
            return Ok(RecoveryReport::default());
        }
        let mut report = RecoveryReport::default();
        for run in &mut self.runs {
            let current = &run.snapshot;
            let previous_state = match current.state {
                RunState::Queued => ActiveState::Queued,
                RunState::Starting => ActiveState::Starting,
                RunState::Running => ActiveState::Running,
                RunState::WaitingInput => ActiveState::WaitingInput,
                RunState::CancelRequested => ActiveState::CancelRequested,
                state if v::is_terminal(state) => continue,
                _ => return Err(unavailable("Invalid test recovery state.")),
            };
            let event = RunEvent::from_parts(
                EventHeader {
                    contract_version: 1,
                    event_id: ids.next_id(),
                    workspace_id: current.workspace_id.clone(),
                    session_id: current.session_id.clone(),
                    run_id: current.run_id.clone(),
                    provider_id: ProviderId::Fake,
                    source: EventSource::System,
                    seq: current.last_seq + 1,
                    occurred_at: now.into(),
                },
                EventPayload::RunRecovered(RunRecoveredPayload {
                    previous_state,
                    state: "interrupted".into(),
                }),
            );
            run.snapshot = v::apply_event(current, &event)?;
            report.interrupted_runs.push(run.snapshot.clone());
            report.committed_events.push(event);
        }
        self.events.extend(report.committed_events.clone());
        Ok(report)
    }

    fn create_session(
        &mut self,
        _: &CreateSessionRequest,
        _: SessionSeed,
    ) -> StoreResult<MutationCommit<SessionSnapshot>> {
        Err(v::error(
            "test_unused_operation",
            "Session creation is outside this test double.",
        ))
    }

    fn enqueue_run(
        &mut self,
        request: &StartFakeRunRequest,
        seed: RunSeed,
    ) -> StoreResult<MutationCommit<RunSnapshot>> {
        self.enqueue_calls += 1;
        let args = v::canonical_arguments(request)?;
        if let Some(replay) = Self::receipt(&self.starts, &request.request_id, &args)? {
            return Ok(replay);
        }
        if let Some(code) = self.enqueue_failure.take() {
            return Err(v::error(code, "Injected admission failure."));
        }
        v::validate_start_fake_run(request)?;
        if !self
            .workspace
            .sessions
            .iter()
            .any(|session| session.session_id == request.session_id)
        {
            return Err(v::error("session_not_found", "Unknown test session."));
        }
        let run = RunSnapshot {
            contract_version: 1,
            workspace_id: request.workspace_id.clone(),
            session_id: request.session_id.clone(),
            run_id: seed.run_id.clone(),
            provider_id: ProviderId::Fake,
            state: RunState::Queued,
            last_seq: 1,
            created_at: seed.created_at.clone(),
            updated_at: seed.created_at.clone(),
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                estimated: false,
            },
            terminal_error: None,
            enqueue_order: self.runs.len() as u64 + 1,
        };
        let event = RunEvent::from_parts(
            EventHeader {
                contract_version: 1,
                event_id: seed.event_id,
                workspace_id: run.workspace_id.clone(),
                session_id: run.session_id.clone(),
                run_id: run.run_id.clone(),
                provider_id: ProviderId::Fake,
                source: EventSource::Fixture,
                seq: 1,
                occurred_at: seed.created_at,
            },
            EventPayload::RunCreated(RunCreatedPayload {
                state: "queued".into(),
            }),
        );
        v::validate_event(&event)?;
        v::validate_snapshot(&run)?;
        self.runs.push(QueuedRun {
            snapshot: run.clone(),
            prompt: request.prompt.clone(),
            scenario: request.scenario,
        });
        self.events.push(event.clone());
        self.starts
            .push((request.request_id.clone(), args, run.clone()));
        Ok(MutationCommit {
            result: run,
            committed_events: vec![event],
            replayed: false,
        })
    }

    fn cancel_run(
        &mut self,
        request: &CancelRunRequest,
        mutation: Option<AppendBatch>,
    ) -> StoreResult<MutationCommit<RunSnapshot>> {
        let args = v::canonical_arguments(request)?;
        if let Some(replay) = Self::receipt(&self.cancels, &request.request_id, &args)? {
            return Ok(replay);
        }
        let current = self
            .get_run(&request.workspace_id, &request.run_id)?
            .snapshot;
        let commit = if let Some(batch) = mutation {
            if batch.run_id != request.run_id {
                return Err(v::error(
                    "validation_failed",
                    "Cancellation target mismatch.",
                ));
            }
            self.append_batch(batch)?
        } else {
            if current.state != RunState::CancelRequested && !v::is_terminal(current.state) {
                return Err(v::error(
                    "state_conflict",
                    "Missing test cancellation transition.",
                ));
            }
            MutationCommit {
                result: current,
                committed_events: vec![],
                replayed: false,
            }
        };
        self.cancels
            .push((request.request_id.clone(), args, commit.result.clone()));
        Ok(commit)
    }

    fn append_batch(&mut self, batch: AppendBatch) -> StoreResult<MutationCommit<RunSnapshot>> {
        if let Some(code) = self.append_failure.take() {
            return Err(v::error(code, "Injected append failure."));
        }
        let index = self
            .runs
            .iter()
            .position(|run| run.snapshot.run_id == batch.run_id)
            .ok_or_else(|| v::error("run_not_found", "Unknown test run."))?;
        v::validate_append(&self.runs[index].snapshot, &batch)?;
        let result = batch.next_snapshot;
        self.runs[index].snapshot = result.clone();
        self.events.extend(batch.events.clone());
        Ok(MutationCommit {
            result,
            committed_events: batch.events,
            replayed: false,
        })
    }

    fn workspace_snapshot(&mut self, _: &WorkspaceRequest) -> StoreResult<WorkspaceSnapshot> {
        Ok(self.view())
    }

    fn get_run(&mut self, workspace_id: &str, run_id: &str) -> StoreResult<QueuedRun> {
        self.runs
            .iter()
            .find(|run| run.snapshot.workspace_id == workspace_id && run.snapshot.run_id == run_id)
            .cloned()
            .ok_or_else(|| v::error("run_not_found", "Unknown test run."))
    }

    fn next_queued(&mut self, workspace_id: &str) -> StoreResult<Option<QueuedRun>> {
        self.next_calls += 1;
        if self.runs.iter().any(|run| {
            run.snapshot.workspace_id == workspace_id
                && run.snapshot.state != RunState::Queued
                && !v::is_terminal(run.snapshot.state)
        }) {
            return Ok(None);
        }
        Ok(self
            .runs
            .iter()
            .filter(|run| {
                run.snapshot.workspace_id == workspace_id && run.snapshot.state == RunState::Queued
            })
            .min_by_key(|run| run.snapshot.enqueue_order)
            .cloned())
    }

    fn list_run_events(&mut self, _: &EventPageRequest) -> StoreResult<EventPage> {
        Err(v::error(
            "test_unused_operation",
            "Pagination is outside this test double.",
        ))
    }

    fn save_layout(&mut self, _: &SaveLayoutRequest) -> StoreResult<MutationCommit<Layout>> {
        Err(v::error(
            "test_unused_operation",
            "Layout persistence is outside this test double.",
        ))
    }
}

type TestRuntime = Runtime<TestStore, TestClock, TestIds>;
fn runtime() -> TestRuntime {
    Runtime::new(TestStore::new(), TestClock, TestIds(100)).unwrap()
}
fn start_request(scenario: FakeScenario, request: u64) -> StartFakeRunRequest {
    StartFakeRunRequest {
        contract_version: 1,
        workspace_id: id(1),
        session_id: id(2),
        request_id: id(request),
        provider_id: ProviderId::Fake,
        prompt: "test prompt".into(),
        scenario,
    }
}
fn cancel_request(run_id: &str, request: u64) -> CancelRunRequest {
    CancelRunRequest {
        contract_version: 1,
        workspace_id: id(1),
        run_id: run_id.into(),
        request_id: id(request),
    }
}
fn run_snapshot(state: RunState) -> RunSnapshot {
    RunSnapshot {
        contract_version: 1,
        workspace_id: id(1),
        session_id: id(2),
        run_id: id(3),
        provider_id: ProviderId::Fake,
        state,
        last_seq: 3,
        created_at: NOW.into(),
        updated_at: NOW.into(),
        usage: Usage {
            input_tokens: None,
            output_tokens: None,
            estimated: false,
        },
        terminal_error: if state == RunState::Failed {
            Some(v::error("fake_start_failed", "SIMULATED test failure."))
        } else {
            None
        },
        enqueue_order: 1,
    }
}
fn finish(runtime: &mut TestRuntime, run_id: &str) -> RunSnapshot {
    for _ in 0..16 {
        let run = runtime.store.get_run(&id(1), run_id).unwrap().snapshot;
        if v::is_terminal(run.state) {
            return run;
        }
        let before = runtime.store.events.len();
        runtime.tick().unwrap();
        assert!(runtime.store.events.len() - before <= 1);
    }
    panic!("Bounded fake scenario did not finish");
}

#[test]
fn success_is_step_driven_unknown_usage_and_coalesced() {
    let mut runtime = runtime();
    assert_eq!(runtime.health(), ExecutionHealth::Ready);
    let started = runtime
        .start_fake_run(&start_request(FakeScenario::Success, 20))
        .unwrap();
    assert_eq!(runtime.store.events.len(), 1);
    assert_eq!(runtime.store.next_calls, 0);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Starting);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Running);
    let result = finish(&mut runtime, &started.run.run_id);
    assert_eq!(result.state, RunState::Succeeded);
    assert_eq!(
        result.usage,
        Usage {
            input_tokens: None,
            output_tokens: None,
            estimated: false,
        }
    );
    let texts: Vec<_> = runtime
        .store
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::TextDelta { payload, .. } => Some(&payload.text),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 3);
    assert!(texts.iter().all(|text| text.starts_with("SIMULATED:")));
    let count = runtime.store.events.len();
    runtime.tick().unwrap();
    assert_eq!(runtime.store.events.len(), count);
    assert_eq!(runtime.take_changed().unwrap().workspace_id, id(1));
    assert!(runtime.take_changed().is_none());
}

#[test]
fn failure_never_reaches_running_or_produces_output() {
    let mut runtime = runtime();
    let started = runtime
        .start_fake_run(&start_request(FakeScenario::Failure, 20))
        .unwrap();
    let failed = finish(&mut runtime, &started.run.run_id);
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.terminal_error.unwrap().code, "fake_start_failed");
    assert_eq!(runtime.store.events.len(), 3);
    assert!(!runtime.store.events.iter().any(|event| matches!(
        event,
        RunEvent::TextDelta { .. }
            | RunEvent::RunStateChanged {
                payload: RunStateChangedPayload {
                    to: RunState::Running,
                    ..
                },
                ..
            }
    )));
}

#[test]
fn wait_has_bounded_prefix_and_acknowledges_only_persisted_cancellation() {
    let mut runtime = runtime();
    let started = runtime
        .start_fake_run(&start_request(FakeScenario::WaitForCancel, 20))
        .unwrap();
    for _ in 0..12 {
        runtime.tick().unwrap();
    }
    assert_eq!(runtime.store.events.len(), 5);
    for _ in 0..12 {
        runtime.tick().unwrap();
    }
    assert_eq!(runtime.store.events.len(), 5);
    let request = cancel_request(&started.run.run_id, 30);
    let receipt = runtime.cancel_run(&request).unwrap();
    assert_eq!(receipt.run.state, RunState::CancelRequested);
    assert_eq!(
        runtime.store.runs[0].snapshot.state,
        RunState::CancelRequested
    );
    assert!(matches!(
        runtime.store.events.last().unwrap(),
        RunEvent::RunStateChanged {
            source: EventSource::System,
            payload: RunStateChangedPayload {
                to: RunState::CancelRequested,
                ..
            },
            ..
        }
    ));
    let count = runtime.store.events.len();
    let repeated = runtime
        .cancel_run(&cancel_request(&started.run.run_id, 31))
        .unwrap();
    assert_eq!(repeated.run.state, RunState::CancelRequested);
    assert_eq!(runtime.store.events.len(), count);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Cancelled);
    let count = runtime.store.events.len();
    let replay = runtime.cancel_run(&request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run, receipt.run);
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Cancelled);
    let terminal = runtime
        .cancel_run(&cancel_request(&started.run.run_id, 32))
        .unwrap();
    assert_eq!(terminal.run.state, RunState::Cancelled);
    assert_eq!(runtime.store.events.len(), count);
}

#[test]
fn queued_cancel_is_direct_and_never_dispatches() {
    let mut runtime = runtime();
    let started = runtime
        .start_fake_run(&start_request(FakeScenario::Success, 20))
        .unwrap();
    let cancelled = runtime
        .cancel_run(&cancel_request(&started.run.run_id, 30))
        .unwrap();
    assert_eq!(cancelled.run.state, RunState::Cancelled);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.events.len(), 2);
    assert!(runtime.active.is_none());
}

#[test]
fn completion_cancel_race_obeys_serialized_terminal_winner() {
    for cancel_first in [true, false] {
        let mut runtime = runtime();
        let started = runtime
            .start_fake_run(&start_request(FakeScenario::Success, 20))
            .unwrap();
        // Starting, running, three text chunks, and unknown usage: success is next.
        for _ in 0..6 {
            runtime.tick().unwrap();
        }
        assert_eq!(runtime.active.as_ref().unwrap().emitted, 4);
        if cancel_first {
            runtime
                .cancel_run(&cancel_request(&started.run.run_id, 30))
                .unwrap();
            runtime.tick().unwrap();
            assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Cancelled);
            assert!(!runtime.store.events.iter().any(|event| matches!(
                event,
                RunEvent::RunStateChanged {
                    payload: RunStateChangedPayload {
                        to: RunState::Succeeded,
                        ..
                    },
                    ..
                }
            )));
        } else {
            runtime.tick().unwrap();
            let count = runtime.store.events.len();
            let response = runtime
                .cancel_run(&cancel_request(&started.run.run_id, 30))
                .unwrap();
            assert_eq!(response.run.state, RunState::Succeeded);
            assert_eq!(runtime.store.events.len(), count);
        }
    }
}

#[test]
fn start_receipt_replay_cannot_relaunch_or_overwrite_live_state() {
    let mut runtime = runtime();
    let request = start_request(FakeScenario::Success, 20);
    let original = runtime.start_fake_run(&request).unwrap();
    runtime.tick().unwrap();
    runtime.take_changed();
    let replay = runtime.start_fake_run(&request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run, original.run);
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Starting);
    assert!(runtime.take_changed().is_none());
    finish(&mut runtime, &original.run.run_id);
    runtime.take_changed();
    let count = runtime.store.events.len();
    assert!(runtime.start_fake_run(&request).unwrap().replayed);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs.len(), 1);
    assert_eq!(runtime.store.events.len(), count);
    assert!(runtime.take_changed().is_none());
    let mut conflicting = request;
    conflicting.prompt.push('!');
    assert_eq!(
        runtime.start_fake_run(&conflicting).unwrap_err().code,
        "request_id_conflict"
    );
    assert_eq!(runtime.health(), ExecutionHealth::Ready);
}

#[test]
fn scheduler_uses_durable_fifo_and_one_workspace_slot() {
    let mut runtime = runtime();
    let first = runtime
        .start_fake_run(&start_request(FakeScenario::WaitForCancel, 20))
        .unwrap();
    let second = runtime
        .start_fake_run(&start_request(FakeScenario::Failure, 21))
        .unwrap();
    for _ in 0..10 {
        runtime.tick().unwrap();
    }
    assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Running);
    assert_eq!(runtime.store.runs[1].snapshot.state, RunState::Queued);
    assert!(first.run.enqueue_order < second.run.enqueue_order);
    runtime
        .cancel_run(&cancel_request(&first.run.run_id, 30))
        .unwrap();
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs[1].snapshot.state, RunState::Queued);
    runtime.tick().unwrap();
    assert_eq!(runtime.store.runs[1].snapshot.state, RunState::Starting);
}

#[test]
fn startup_interrupts_every_prior_nonterminal_state_and_never_repeats_recovery() {
    for state in [
        RunState::Queued,
        RunState::Starting,
        RunState::Running,
        RunState::WaitingInput,
        RunState::CancelRequested,
    ] {
        let mut store = TestStore::new();
        let mut old = run_snapshot(state);
        if state == RunState::Queued {
            old.last_seq = 1;
        }
        store.runs.push(QueuedRun {
            snapshot: old,
            prompt: "private old prompt".into(),
            scenario: FakeScenario::Success,
        });
        let mut runtime = Runtime::new(store, TestClock, TestIds(100)).unwrap();
        assert_eq!(runtime.store.runs[0].snapshot.state, RunState::Interrupted);
        assert_eq!(runtime.store.events.len(), 1);
        assert!(matches!(
            runtime.store.events[0],
            RunEvent::RunRecovered { .. }
        ));
        assert!(runtime.take_changed().is_some());
        let snapshot = runtime
            .bootstrap_workspace(&BootstrapRequest {
                contract_version: 1,
            })
            .unwrap();
        assert!(snapshot.execution_available);
        runtime.tick().unwrap();
        assert_eq!(runtime.store.events.len(), 1);
        runtime
            .start_fake_run(&start_request(FakeScenario::Failure, 20))
            .unwrap();
        runtime
            .bootstrap_workspace(&BootstrapRequest {
                contract_version: 1,
            })
            .unwrap();
        runtime.tick().unwrap();
        assert_eq!(runtime.store.runs[1].snapshot.state, RunState::Starting);
        assert_eq!(runtime.store.bootstraps, 1);
        assert_eq!(runtime.store.recoveries, 1);
    }
}

#[test]
fn disabled_providers_and_invalid_workspace_fail_before_admission() {
    let mut runtime = runtime();
    let ids_before = runtime.ids.0;
    for provider in [ProviderId::Codex, ProviderId::Claude] {
        let mut request = start_request(FakeScenario::Success, 20);
        request.provider_id = provider;
        assert_eq!(
            runtime.start_fake_run(&request).unwrap_err().code,
            "provider_execution_disabled"
        );
    }
    let mut request = start_request(FakeScenario::Success, 20);
    request.workspace_id = id(99);
    assert_eq!(
        runtime.start_fake_run(&request).unwrap_err().code,
        "workspace_not_found"
    );
    assert_eq!(runtime.store.enqueue_calls, 0);
    assert_eq!(runtime.store.next_calls, 0);
    assert_eq!(runtime.ids.0, ids_before);
    assert!(runtime.store.events.is_empty());
    assert_eq!(runtime.health(), ExecutionHealth::Ready);
}

#[test]
fn write_failure_stops_production_but_domain_rejections_do_not() {
    let mut runtime = runtime();
    runtime.store.enqueue_failure = Some("queue_full");
    assert_eq!(
        runtime
            .start_fake_run(&start_request(FakeScenario::Success, 20))
            .unwrap_err()
            .code,
        "queue_full"
    );
    assert_eq!(runtime.health(), ExecutionHealth::Ready);
    runtime
        .start_fake_run(&start_request(FakeScenario::Success, 20))
        .unwrap();
    runtime.store.append_failure = Some("state_conflict");
    assert_eq!(runtime.tick().unwrap_err().code, "state_conflict");
    assert_eq!(runtime.health(), ExecutionHealth::Ready);
    runtime.tick().unwrap();
    runtime.take_changed();
    let count = runtime.store.events.len();
    runtime.store.append_failure = Some("storage_busy");
    assert_eq!(runtime.tick().unwrap_err().code, "storage_busy");
    assert_eq!(runtime.health(), ExecutionHealth::StorageUnavailable);
    assert!(runtime.active.is_none());
    assert!(runtime.take_changed().is_none());
    assert_eq!(runtime.tick().unwrap_err().code, "storage_unavailable");
    assert_eq!(
        runtime
            .start_fake_run(&start_request(FakeScenario::Failure, 21))
            .unwrap_err()
            .code,
        "storage_unavailable"
    );
    let snapshot = runtime
        .get_workspace_snapshot(&WorkspaceRequest {
            contract_version: 1,
            workspace_id: id(1),
        })
        .unwrap();
    assert!(!snapshot.execution_available);
    assert_eq!(snapshot.runs[0].state, RunState::Starting);
    assert_eq!(runtime.store.events.len(), count);
}

#[test]
fn decisions_prioritize_cancellation_and_never_advance_terminal_states() {
    for scenario in [
        FakeScenario::Success,
        FakeScenario::Failure,
        FakeScenario::WaitForCancel,
    ] {
        for emitted in [0, 1, 4, u8::MAX] {
            let payload = fake::decide(RunState::CancelRequested, scenario, emitted).unwrap();
            assert!(matches!(
                payload,
                EventPayload::RunStateChanged(RunStateChangedPayload {
                    from: RunState::CancelRequested,
                    to: RunState::Cancelled,
                    error: None,
                })
            ));
        }
        for terminal in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Cancelled,
            RunState::Interrupted,
        ] {
            assert!(fake::decide(terminal, scenario, 0).is_none());
        }
    }
    assert!(fake::decide(RunState::Running, FakeScenario::WaitForCancel, 2).is_none());
    assert!(fake::decide(RunState::WaitingInput, FakeScenario::Success, 0).is_none());
}

fn text_payload(text: &str) -> EventPayload {
    EventPayload::TextDelta(TextDeltaPayload {
        stream: TextStream::Assistant,
        text: text.into(),
    })
}
fn running_budget() -> Budget {
    let mut budget = Budget::fresh();
    budget.ordinary = 3;
    budget
}

#[test]
fn each_output_limit_discards_whole_batch_and_atomically_stops_once() {
    for scope in 0..4 {
        let mut runtime = runtime();
        let current = run_snapshot(RunState::Running);
        let budget = running_budget();
        let mut limits = BudgetLimits::production();
        if scope == 0 || scope == 3 {
            limits.pending_bytes = 1;
        }
        if scope == 1 || scope == 3 {
            limits.text_bytes = 3;
        }
        if scope == 2 || scope == 3 {
            limits.ordinary_events = 3;
        }
        let prepared = runtime
            .prepare(&current, &budget, text_payload("🧪"), limits)
            .unwrap();
        assert!(prepared.truncated);
        assert_eq!(
            prepared.batch.next_snapshot.state,
            RunState::CancelRequested
        );
        v::validate_append(&current, &prepared.batch).unwrap();
        let markers: Vec<_> = prepared
            .batch
            .events
            .iter()
            .filter_map(|event| match event {
                RunEvent::OutputTruncated { payload, .. } => Some(payload),
                _ => None,
            })
            .collect();
        assert_eq!(markers.len(), if scope == 3 { 3 } else { 1 });
        assert!(prepared.batch.events.iter().all(v::is_reserved_event));
        for marker in markers {
            assert_eq!(marker.discarded_events, 1);
            match marker.scope {
                TruncationScope::PersistedTextBytes => assert_eq!(marker.discarded_bytes, Some(4)),
                TruncationScope::PendingBytes => assert!(marker.discarded_bytes.unwrap() > 4),
                TruncationScope::EventCount => assert_eq!(marker.discarded_bytes, None),
            }
        }
        assert_eq!(budget.ordinary, 3);
        assert!(!budget.stopped);
        assert_eq!(prepared.budget.ordinary, 3);
        assert_eq!(prepared.budget.text_bytes, 0);
        assert!(prepared.budget.stopped && prepared.budget.cancel_intent);
        assert_eq!(
            runtime
                .prepare(
                    &prepared.batch.next_snapshot,
                    &prepared.budget,
                    text_payload("later"),
                    limits
                )
                .err()
                .unwrap()
                .code,
            "state_conflict"
        );
        assert!(prepared.batch.events.len() <= 4);
    }
}

#[test]
fn byte_and_event_boundaries_are_inclusive_and_classification_uses_shared_rules() {
    let mut runtime = runtime();
    let current = run_snapshot(RunState::Running);
    let event = runtime
        .event(&current, text_payload("🧪"), EventSource::Fixture)
        .unwrap();
    assert!(!v::is_reserved_event(&event));
    let encoded = serde_json::to_vec(&event).unwrap().len();
    let mut limits = BudgetLimits::production();
    limits.pending_bytes = encoded;
    limits.text_bytes = 4;
    limits.ordinary_events = 4;
    let budget = running_budget();
    let accepted = match budget.check(std::slice::from_ref(&event), limits).unwrap() {
        BudgetCheck::Accept(next) => next,
        BudgetCheck::Discard(_) => panic!("Exact boundary was rejected"),
    };
    assert_eq!(accepted.text_bytes, 4);
    assert_eq!(accepted.ordinary, 4);
    let discard = match accepted
        .check(std::slice::from_ref(&event), limits)
        .unwrap()
    {
        BudgetCheck::Discard(discard) => discard,
        BudgetCheck::Accept(_) => panic!("Cumulative overflow was accepted"),
    };
    assert_eq!(discard.scopes, [false, true, true]);
    assert_eq!(discard.events, 1);
    assert_eq!(discard.text_bytes, 4);
    let cancel = runtime
        .event(
            &current,
            EventPayload::RunStateChanged(RunStateChangedPayload {
                from: RunState::Running,
                to: RunState::CancelRequested,
                error: None,
            }),
            EventSource::System,
        )
        .unwrap();
    assert!(v::is_reserved_event(&cancel));
    let mut full = budget.clone();
    full.ordinary = ORDINARY_EVENTS;
    assert!(matches!(
        full.check(std::slice::from_ref(&cancel), BudgetLimits::production())
            .unwrap(),
        BudgetCheck::Accept(_)
    ));
    full.reserved = RESERVED_EVENTS;
    assert_eq!(
        full.check(std::slice::from_ref(&cancel), BudgetLimits::production())
            .unwrap_err()
            .code,
        "storage_unavailable"
    );
    let mut tiny_control = BudgetLimits::production();
    tiny_control.control_bytes = 1;
    assert_eq!(
        budget.check(&[cancel], tiny_control).unwrap_err().code,
        "storage_unavailable"
    );
}

#[test]
fn text_utf8_and_full_encoded_event_limits_are_enforced() {
    let mut runtime = runtime();
    let current = run_snapshot(RunState::Running);
    let oversized_utf8 = "🧪".repeat(TEXT_DELTA_BYTES / 4 + 1);
    assert!(runtime
        .prepare(
            &current,
            &running_budget(),
            text_payload(&oversized_utf8),
            BudgetLimits::production()
        )
        .is_err());
    // Escaping can exceed the encoded-event limit even at the raw text limit.
    let escaped = "\n".repeat(TEXT_DELTA_BYTES);
    assert!(runtime
        .prepare(
            &current,
            &running_budget(),
            text_payload(&escaped),
            BudgetLimits::production()
        )
        .is_err());
    let valid = "x".repeat(TEXT_DELTA_BYTES);
    let prepared = runtime
        .prepare(
            &current,
            &running_budget(),
            text_payload(&valid),
            BudgetLimits::production(),
        )
        .unwrap();
    assert!(!prepared.truncated);
    assert_eq!(prepared.budget.text_bytes, TEXT_DELTA_BYTES);
    assert!(serde_json::to_vec(&prepared.batch.events[0]).unwrap().len() <= EVENT_BYTES);
}

#[test]
fn truncation_of_queued_lifecycle_cancels_directly_without_ordinary_append() {
    let mut runtime = runtime();
    let mut current = run_snapshot(RunState::Queued);
    current.last_seq = 1;
    let mut limits = BudgetLimits::production();
    limits.ordinary_events = 1;
    let prepared = runtime
        .prepare(
            &current,
            &Budget::fresh(),
            fake::decide(RunState::Queued, FakeScenario::Success, 0).unwrap(),
            limits,
        )
        .unwrap();
    assert_eq!(prepared.batch.next_snapshot.state, RunState::Cancelled);
    assert_eq!(prepared.batch.events.len(), 2);
    assert_eq!(prepared.budget.reserved, 2);
    assert!(prepared.budget.terminal);
    assert!(!prepared.budget.cancel_intent);
    v::validate_append(&current, &prepared.batch).unwrap();
}
