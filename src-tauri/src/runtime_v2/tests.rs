//! Owner runtime over a real fresh V2Store with a scripted in-process provider:
//! no child processes; ordering, arms, cancel, quarantine and restart rules.
use super::*;
use crate::storage::store_v2::V2Store;
use crate::supervisor::{Cancel, Progress, ReportedUsage, WorkerReport};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::Instant,
};

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("aw-runtime-v2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct TestClock;
impl Clock for TestClock {
    fn now_utc(&self) -> String {
        "2026-10-21T00:00:00.000Z".into()
    }
}
struct Ids;
impl IdSource for Ids {
    fn next_id(&mut self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

enum Step {
    Complete(&'static str),
    Fail(&'static str),
    Unproven,
    WaitForCancel,
    CompleteEvenIfCancelled,
}
#[derive(Clone)]
struct Scripted(Arc<Mutex<mpsc::Receiver<Step>>>, Arc<Mutex<Vec<String>>>);
impl ProviderFactory for Scripted {
    fn run_reporting(
        &self,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        self.1.lock().unwrap().push(prompt.into());
        started();
        for p in [
            Progress::ThreadStarted,
            Progress::TurnStarted,
            Progress::ReasoningObserved,
            Progress::ReasoningObserved,
        ] {
            progress(p);
        }
        self.1.lock().unwrap().pop();
        self.run(prompt, cancel, &mut || {})
    }
    fn run(&self, prompt: &str, cancel: &Cancel, started: &mut dyn FnMut()) -> WorkerReport {
        self.1.lock().unwrap().push(prompt.into());
        started();
        let usage = ReportedUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 2,
            reasoning_output_tokens: 0,
            cache_write_input_tokens: None,
        };
        let proven = |outcome| WorkerReport {
            outcome,
            teardown_proven: true,
        };
        match self.0.lock().unwrap().recv().unwrap() {
            Step::Complete(message) => proven(WorkerOutcome::Completed {
                message: message.into(),
                usage,
            }),
            Step::Fail(code) => proven(WorkerOutcome::Failed { code }),
            Step::Unproven => WorkerReport {
                outcome: WorkerOutcome::Failed {
                    code: "codex_cleanup_failed",
                },
                teardown_proven: false,
            },
            Step::WaitForCancel => {
                while !cancel.is_signalled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                proven(WorkerOutcome::Failed {
                    code: "codex_cancelled",
                })
            }
            Step::CompleteEvenIfCancelled => {
                while !cancel.is_signalled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                proven(WorkerOutcome::Completed {
                    message: "late".into(),
                    usage,
                })
            }
        }
    }
}

type Rt = RuntimeV2<V2Store, Scripted, TestClock, Ids>;

fn runtime(
    dir: &Dir,
    codex: v2::CodexAvailability,
) -> (Rt, mpsc::Sender<Step>, Arc<Mutex<Vec<String>>>) {
    let (tx, rx) = mpsc::channel();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let factory = Scripted(Arc::new(Mutex::new(rx)), prompts.clone());
    let rt = RuntimeV2::new(
        V2Store::open(&dir.0).unwrap(),
        factory,
        codex,
        TestClock,
        Ids,
    )
    .unwrap();
    (rt, tx, prompts)
}
fn session(rt: &mut Rt, provider: v2::ProviderId) -> String {
    rt.create_session(&v2::CreateSessionRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        label: "Session".into(),
        provider_id: provider,
    })
    .unwrap()
    .session
    .session_id
}
fn codex_request(rt: &Rt, session: &str, prompt: &str) -> v2::StartCodexRunRequest {
    v2::StartCodexRunRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        session_id: session.into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        prompt: prompt.into(),
        profile_id: v2::CodexProfileId::CodexTextV1,
        confirmed: true,
        context_run_ids: None,
    }
}
fn fake_request(rt: &Rt, session: &str, scenario: v2::FakeScenario) -> v2::StartFakeRunRequest {
    v2::StartFakeRunRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        session_id: session.into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        provider_id: v2::ProviderId::Fake,
        prompt: "Simulated".into(),
        scenario,
    }
}
fn state(rt: &mut Rt, run_id: &str) -> v2::RunState {
    rt.snapshot()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.run_id == run_id)
        .unwrap()
        .state
}
fn tick_until(rt: &mut Rt, run_id: &str, want: v2::RunState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while state(rt, run_id) != want {
        rt.tick().unwrap();
        assert!(Instant::now() < deadline, "run never reached {want:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn events(rt: &mut Rt, run_id: &str) -> Vec<v2::RunEvent> {
    rt.list_run_events(&v2::EventPageRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        run_id: run_id.into(),
        after_seq: 0,
        through_seq: None,
        limit: 200,
    })
    .unwrap()
    .events
}

#[test]
fn fake_and_codex_share_one_fifo_and_codex_commits_output_usage_success() {
    let dir = Dir::new();
    let (mut rt, steps, prompts) = runtime(&dir, v2::CodexAvailability::Ready);
    let f = session(&mut rt, v2::ProviderId::Fake);
    let c = session(&mut rt, v2::ProviderId::Codex);
    let fake = rt
        .start_fake_run(&fake_request(&rt, &f, v2::FakeScenario::Success))
        .unwrap()
        .run;
    let request = codex_request(&rt, &c, "Say hi 😀");
    let codex = rt.start_codex_run(&request).unwrap().run;
    // Replay returns the same run and never mints a second arm or run.
    let replay = rt.start_codex_run(&request).unwrap();
    assert!(replay.replayed && replay.run.run_id == codex.run_id);
    // FIFO: the fake run finishes before Codex starts.
    tick_until(&mut rt, &fake.run_id, v2::RunState::Succeeded);
    assert_eq!(state(&mut rt, &codex.run_id), v2::RunState::Queued);
    tick_until(&mut rt, &codex.run_id, v2::RunState::Running);
    assert_eq!(prompts.lock().unwrap().as_slice(), ["Say hi 😀"]);
    steps
        .send(Step::Complete("Hello from the fixture"))
        .unwrap();
    tick_until(&mut rt, &codex.run_id, v2::RunState::Succeeded);
    let snapshot = rt.snapshot().unwrap();
    let done = snapshot
        .runs
        .iter()
        .find(|r| r.run_id == codex.run_id)
        .unwrap();
    assert_eq!(done.usage.input_tokens, Some(10));
    assert!(snapshot.execution_available);
    assert_eq!(snapshot.codex_availability, v2::CodexAvailability::Ready);
    let kinds: Vec<_> = events(&mut rt, &codex.run_id)
        .iter()
        .map(|e| match e {
            v2::RunEvent::RunCreated { .. } => "created",
            v2::RunEvent::RunStateChanged { payload, .. } => payload.to.as_str(),
            v2::RunEvent::TextDelta { .. } => "text",
            v2::RunEvent::UsageUpdated { .. } => "usage",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "created",
            "starting",
            "running",
            "text",
            "usage",
            "succeeded"
        ]
    );
    assert_eq!(prompts.lock().unwrap().len(), 1);
}

#[test]
fn cancel_is_persisted_before_the_worker_stops_and_beats_late_success() {
    let dir = Dir::new();
    let (mut rt, steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
    let c = session(&mut rt, v2::ProviderId::Codex);
    for step in [Step::WaitForCancel, Step::CompleteEvenIfCancelled] {
        let run = rt
            .start_codex_run(&codex_request(&rt, &c, "p"))
            .unwrap()
            .run;
        tick_until(&mut rt, &run.run_id, v2::RunState::Running);
        steps.send(step).unwrap();
        let cancelled = rt
            .cancel_run(&v2::CancelRunRequest {
                contract_version: 2,
                workspace_id: rt.workspace_id().into(),
                run_id: run.run_id.clone(),
                request_id: uuid::Uuid::new_v4().to_string(),
            })
            .unwrap();
        assert_eq!(cancelled.run.state, v2::RunState::CancelRequested);
        tick_until(&mut rt, &run.run_id, v2::RunState::Cancelled);
        assert!(!events(&mut rt, &run.run_id)
            .iter()
            .any(|e| matches!(e, v2::RunEvent::TextDelta { .. })));
    }
    // Queued Codex cancel never reaches the worker.
    let blocker = rt
        .start_codex_run(&codex_request(&rt, &c, "a"))
        .unwrap()
        .run;
    let queued = rt
        .start_codex_run(&codex_request(&rt, &c, "b"))
        .unwrap()
        .run;
    tick_until(&mut rt, &blocker.run_id, v2::RunState::Running);
    rt.cancel_run(&v2::CancelRunRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        run_id: queued.run_id.clone(),
        request_id: uuid::Uuid::new_v4().to_string(),
    })
    .unwrap();
    assert_eq!(state(&mut rt, &queued.run_id), v2::RunState::Cancelled);
    steps.send(Step::Fail("codex_timeout")).unwrap();
    tick_until(&mut rt, &blocker.run_id, v2::RunState::Failed);
    for _ in 0..20 {
        rt.tick().unwrap();
    }
    assert_eq!(state(&mut rt, &queued.run_id), v2::RunState::Cancelled);
}

#[test]
fn unproven_teardown_quarantines_and_stops_all_dispatch() {
    let dir = Dir::new();
    let (mut rt, steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
    let c = session(&mut rt, v2::ProviderId::Codex);
    let f = session(&mut rt, v2::ProviderId::Fake);
    let run = rt
        .start_codex_run(&codex_request(&rt, &c, "p"))
        .unwrap()
        .run;
    let later = rt
        .start_fake_run(&fake_request(&rt, &f, v2::FakeScenario::Success))
        .unwrap()
        .run;
    tick_until(&mut rt, &run.run_id, v2::RunState::Running);
    steps.send(Step::Unproven).unwrap();
    tick_until(&mut rt, &run.run_id, v2::RunState::Failed);
    let snapshot = rt.snapshot().unwrap();
    assert!(!snapshot.execution_available);
    assert_eq!(
        snapshot.codex_availability,
        v2::CodexAvailability::Quarantined
    );
    let failed = snapshot
        .runs
        .iter()
        .find(|r| r.run_id == run.run_id)
        .unwrap();
    assert_eq!(
        failed.terminal_error.as_ref().unwrap().code,
        "codex_cleanup_failed"
    );
    for _ in 0..20 {
        rt.tick().unwrap();
    }
    assert_eq!(state(&mut rt, &later.run_id), v2::RunState::Queued);
    // New enqueues are refused outright while quarantined.
    let refused = rt
        .start_fake_run(&fake_request(&rt, &f, v2::FakeScenario::Success))
        .err()
        .unwrap();
    assert_eq!(refused.code, "state_conflict");
    assert!(rt.start_codex_run(&codex_request(&rt, &c, "p")).is_err());
}

#[test]
fn unavailable_codex_fails_at_the_head_without_blocking_fake_work() {
    let dir = Dir::new();
    let (mut rt, _steps, prompts) = runtime(&dir, v2::CodexAvailability::NotConfigured);
    let c = session(&mut rt, v2::ProviderId::Codex);
    let f = session(&mut rt, v2::ProviderId::Fake);
    let run = rt
        .start_codex_run(&codex_request(&rt, &c, "p"))
        .unwrap()
        .run;
    let fake = rt
        .start_fake_run(&fake_request(&rt, &f, v2::FakeScenario::Success))
        .unwrap()
        .run;
    tick_until(&mut rt, &run.run_id, v2::RunState::Failed);
    let snapshot = rt.snapshot().unwrap();
    let failed = snapshot
        .runs
        .iter()
        .find(|r| r.run_id == run.run_id)
        .unwrap();
    assert_eq!(
        failed.terminal_error.as_ref().unwrap().code,
        "codex_unavailable"
    );
    tick_until(&mut rt, &fake.run_id, v2::RunState::Succeeded);
    assert!(prompts.lock().unwrap().is_empty());
}

#[test]
fn restart_interrupts_queued_codex_work_and_never_rearms_it() {
    let dir = Dir::new();
    let queued_id;
    {
        let (mut rt, _steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
        let f = session(&mut rt, v2::ProviderId::Fake);
        let c = session(&mut rt, v2::ProviderId::Codex);
        let blocker = rt
            .start_fake_run(&fake_request(&rt, &f, v2::FakeScenario::WaitForCancel))
            .unwrap()
            .run;
        queued_id = rt
            .start_codex_run(&codex_request(&rt, &c, "p"))
            .unwrap()
            .run
            .run_id;
        tick_until(&mut rt, &blocker.run_id, v2::RunState::Running);
        assert!(rt.shutdown(Duration::from_millis(100)));
    }
    let (mut rt, _steps, prompts) = runtime(&dir, v2::CodexAvailability::Ready);
    assert_eq!(state(&mut rt, &queued_id), v2::RunState::Interrupted);
    for _ in 0..20 {
        rt.tick().unwrap();
    }
    assert!(prompts.lock().unwrap().is_empty());
}

#[test]
fn shutdown_cancels_the_active_codex_run_and_commits_cancelled() {
    let dir = Dir::new();
    let run_id;
    {
        let (mut rt, steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
        let c = session(&mut rt, v2::ProviderId::Codex);
        run_id = rt
            .start_codex_run(&codex_request(&rt, &c, "p"))
            .unwrap()
            .run
            .run_id;
        tick_until(&mut rt, &run_id, v2::RunState::Running);
        steps.send(Step::WaitForCancel).unwrap();
        assert!(rt.shutdown(Duration::from_secs(5)));
        assert_eq!(state(&mut rt, &run_id), v2::RunState::Cancelled);
    }
    let (mut rt, _steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
    assert_eq!(state(&mut rt, &run_id), v2::RunState::Cancelled);
}

#[test]
fn progress_is_volatile_coalesced_and_cleared_when_the_run_ends() {
    let dir = Dir::new();
    let (mut rt, steps, _) = runtime(&dir, v2::CodexAvailability::Ready);
    let c = session(&mut rt, v2::ProviderId::Codex);
    assert!(rt.progress().run_id.is_none() && rt.progress().steps.is_empty());
    let run = rt
        .start_codex_run(&codex_request(&rt, &c, "hi"))
        .unwrap()
        .run;
    tick_until(&mut rt, &run.run_id, v2::RunState::Running);
    let deadline = Instant::now() + Duration::from_secs(5);
    while rt.progress().steps.len() < 3 && Instant::now() < deadline {
        rt.tick().unwrap();
    }
    let progress = rt.progress();
    assert_eq!(progress.run_id.as_deref(), Some(run.run_id.as_str()));
    let shape: Vec<_> = progress.steps.iter().map(|s| (s.step, s.count)).collect();
    assert_eq!(
        shape,
        [("thread_started", 1), ("turn_started", 1), ("reasoning_observed", 2)]
    );
    let before = events(&mut rt, &run.run_id).len();
    assert!(rt.take_changed());
    steps.send(Step::Complete("done")).unwrap();
    tick_until(&mut rt, &run.run_id, v2::RunState::Succeeded);
    // Progress never reached the journal: only state changes and output.
    assert!(events(&mut rt, &run.run_id).len() >= before);
    let after = rt.progress();
    assert!(after.run_id.is_none() && after.steps.is_empty());
}

#[test]
fn a_v2_follow_up_sends_the_turns_carried_from_the_journal() {
    let dir = Dir::new();
    let (mut rt, steps, prompts) = runtime(&dir, v2::CodexAvailability::Ready);
    let c = session(&mut rt, v2::ProviderId::Codex);
    let other = session(&mut rt, v2::ProviderId::Codex);

    let first = rt
        .start_codex_run(&codex_request(&rt, &c, "What is Rust?"))
        .unwrap()
        .run;
    tick_until(&mut rt, &first.run_id, v2::RunState::Running);
    steps.send(Step::Complete("A systems language.")).unwrap();
    tick_until(&mut rt, &first.run_id, v2::RunState::Succeeded);
    let foreign = rt
        .start_codex_run(&codex_request(&rt, &other, "Unrelated secret"))
        .unwrap()
        .run;
    tick_until(&mut rt, &foreign.run_id, v2::RunState::Running);
    steps.send(Step::Complete("Unrelated answer.")).unwrap();
    tick_until(&mut rt, &foreign.run_id, v2::RunState::Succeeded);

    let mut request = codex_request(&rt, &c, "Shorter?");
    request.profile_id = v2::CodexProfileId::CodexTextV2;
    request.context_run_ids = Some(vec![first.run_id.clone()]);
    let second = rt.start_codex_run(&request).unwrap().run;
    assert_eq!(second.profile_id, Some(v2::CodexProfileId::CodexTextV2));
    // Replay mints no second arm.
    assert!(rt.start_codex_run(&request).unwrap().replayed);
    tick_until(&mut rt, &second.run_id, v2::RunState::Running);
    let seen = prompts.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0], "What is Rust?", "v1 sends the bare prompt");
    let composed = &seen[2];
    assert!(composed.contains("[User] What is Rust?\n[Assistant] A systems language."), "{composed}");
    assert!(composed.ends_with("New message from the user:\nShorter?"), "{composed}");
    assert!(!composed.contains("Unrelated"), "another session's turns are never carried");
    steps.send(Step::Complete("Fast.")).unwrap();
    tick_until(&mut rt, &second.run_id, v2::RunState::Succeeded);

    // Refusals leave nothing queued: wrong session, and a run that is not carried-eligible.
    let before = rt.snapshot().unwrap().runs.len();
    for (session, ids) in [
        (&c, vec![foreign.run_id.clone()]),
        (&c, vec![uuid::Uuid::new_v4().to_string()]),
    ] {
        let mut bad = codex_request(&rt, session, "x");
        bad.profile_id = v2::CodexProfileId::CodexTextV2;
        bad.context_run_ids = Some(ids);
        let error = rt.start_codex_run(&bad).err().unwrap();
        assert!(
            matches!(error.code.as_str(), "context_unavailable" | "run_not_found"),
            "{error:?}"
        );
    }
    assert_eq!(rt.snapshot().unwrap().runs.len(), before);
    assert!(rt.snapshot().unwrap().execution_available, "a refused context is not a storage failure");
}

#[test]
fn the_context_preview_is_exactly_what_a_follow_up_sends_and_changes_nothing() {
    let dir = Dir::new();
    let (mut rt, steps, prompts) = runtime(&dir, v2::CodexAvailability::Ready);
    let c = session(&mut rt, v2::ProviderId::Codex);
    let first = rt
        .start_codex_run(&codex_request(&rt, &c, "What is Rust?"))
        .unwrap()
        .run;
    tick_until(&mut rt, &first.run_id, v2::RunState::Running);
    steps.send(Step::Complete("A systems language.")).unwrap();
    tick_until(&mut rt, &first.run_id, v2::RunState::Succeeded);

    let before = rt.snapshot().unwrap();
    let preview = rt
        .preview_codex_context(&v2::PreviewCodexContextRequest {
            contract_version: 2,
            workspace_id: rt.workspace_id().into(),
            session_id: c.clone(),
            prompt: "Shorter?".into(),
            context_run_ids: vec![first.run_id.clone()],
        })
        .unwrap();
    assert_eq!((preview.carried, preview.trimmed), (1, false));
    assert_eq!(preview.bytes as usize, preview.text.len());
    assert_eq!(rt.snapshot().unwrap().runs.len(), before.runs.len(), "a preview admits nothing");

    // Sending the same follow-up delivers byte-for-byte the text that was previewed.
    let mut request = codex_request(&rt, &c, "Shorter?");
    request.profile_id = v2::CodexProfileId::CodexTextV2;
    request.context_run_ids = Some(vec![first.run_id.clone()]);
    let second = rt.start_codex_run(&request).unwrap().run;
    tick_until(&mut rt, &second.run_id, v2::RunState::Running);
    assert_eq!(prompts.lock().unwrap().last().unwrap(), &preview.text);
    steps.send(Step::Complete("Fast.")).unwrap();
    tick_until(&mut rt, &second.run_id, v2::RunState::Succeeded);

    // A run that is not carry-eligible is refused as a domain error, not a storage failure.
    let bad = rt.preview_codex_context(&v2::PreviewCodexContextRequest {
        contract_version: 2,
        workspace_id: rt.workspace_id().into(),
        session_id: c,
        prompt: "x".into(),
        context_run_ids: vec![uuid::Uuid::new_v4().to_string()],
    });
    assert!(matches!(bad.err().map(|e| e.code), Some(code) if code == "run_not_found" || code == "context_unavailable"));
    assert!(rt.snapshot().unwrap().execution_available);
}
