//! Supervisor tests with a scripted in-process provider: no child processes,
//! no store. Real Job-contained Codex fixture coverage lives in codex_process.
use super::*;
use std::sync::{mpsc, Mutex};

const NOW: &str = "2026-10-21T00:00:00.000Z";

/// Each run takes one scripted step from a channel, so tests control timing.
enum Step {
    Complete,
    Fail(&'static str),
    Unproven,
    Panic,
    WaitForCancel,
}
struct Scripted {
    steps: Mutex<mpsc::Receiver<Step>>,
    seen_prompts: Mutex<Vec<String>>,
}
impl ProviderFactory for Arc<Scripted> {
    fn run(&self, prompt: &str, cancel: &Cancel, started: &mut dyn FnMut()) -> WorkerReport {
        self.seen_prompts.lock().unwrap().push(prompt.into());
        started();
        started(); // a second call must not produce a second Started event
        let step = self.steps.lock().unwrap().recv().unwrap();
        let proven = |outcome| WorkerReport {
            outcome,
            teardown_proven: true,
        };
        match step {
            Step::Complete => proven(WorkerOutcome::Completed {
                message: "ok".into(),
                usage: ReportedUsage {
                    input_tokens: 1,
                    cached_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_output_tokens: 0,
                    cache_write_input_tokens: None,
                },
            }),
            Step::Fail(code) => proven(WorkerOutcome::Failed { code }),
            Step::Unproven => WorkerReport {
                outcome: WorkerOutcome::Failed {
                    code: "codex_cleanup_failed",
                },
                teardown_proven: false,
            },
            Step::Panic => panic!("scripted provider panic"),
            Step::WaitForCancel => {
                while !cancel.is_signalled() {
                    thread::sleep(Duration::from_millis(1));
                }
                proven(WorkerOutcome::Failed {
                    code: "codex_cancelled",
                })
            }
        }
    }
}
fn supervisor() -> (Supervisor<Arc<Scripted>>, mpsc::Sender<Step>, Arc<Scripted>) {
    let (tx, rx) = mpsc::channel();
    let scripted = Arc::new(Scripted {
        steps: Mutex::new(rx),
        seen_prompts: Mutex::new(Vec::new()),
    });
    (Supervisor::new(scripted.clone()), tx, scripted)
}
fn admission(replayed: bool) -> MutationCommit<v2::RunSnapshot> {
    let id = |n: u64| format!("00000000-0000-4000-8000-{n:012x}");
    let snapshot = v2::RunSnapshot {
        contract_version: 2,
        workspace_id: id(1),
        session_id: id(2),
        run_id: uuid::Uuid::new_v4().to_string(),
        provider_id: v2::ProviderId::Codex,
        profile_id: Some(v2::CodexProfileId::CodexTextV1),
        model_id: Some(v2::CodexModelId::Gpt61Sol),
        state: v2::RunState::Queued,
        last_seq: 1,
        created_at: NOW.into(),
        updated_at: NOW.into(),
        enqueue_order: 1,
        usage: v2::Usage {
            input_tokens: None,
            output_tokens: None,
            estimated: false,
            cached_input_tokens: None,
            cache_write_input_tokens: None,
            reasoning_output_tokens: None,
        },
        terminal_error: None,
    };
    let event = v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(9),
            workspace_id: snapshot.workspace_id.clone(),
            session_id: snapshot.session_id.clone(),
            run_id: snapshot.run_id.clone(),
            provider_id: v2::ProviderId::Codex,
            source: v2::EventSource::System,
            seq: 1,
            occurred_at: NOW.into(),
        },
        v2::EventPayload::RunCreated(v2::RunCreatedPayload {
            state: "queued".into(),
        }),
    );
    MutationCommit {
        result: snapshot,
        committed_events: if replayed { Vec::new() } else { vec![event] },
        replayed,
    }
}
fn wait_events<F: ProviderFactory>(s: &mut Supervisor<F>, count: usize) -> Vec<SupervisorEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    while all.len() < count {
        all.extend(s.poll());
        assert!(Instant::now() < deadline, "timed out waiting for events");
        thread::sleep(Duration::from_millis(1));
    }
    all
}

#[test]
fn arms_come_only_from_fresh_queued_admissions() {
    assert!(Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p").is_some());
    assert!(Supervisor::<Arc<Scripted>>::mint_arm(&admission(true), "p").is_none());
    let mut moved = admission(false);
    moved.result.state = v2::RunState::Starting;
    moved.result.last_seq = 2;
    assert!(Supervisor::<Arc<Scripted>>::mint_arm(&moved, "p").is_none());
    let mut no_event = admission(false);
    no_event.committed_events.clear();
    assert!(Supervisor::<Arc<Scripted>>::mint_arm(&no_event, "p").is_none());
}

#[test]
fn one_slot_runs_to_completion_and_frees_only_after_proven_teardown() {
    let (mut s, steps, scripted) = supervisor();
    let commit = admission(false);
    let arm = Supervisor::<Arc<Scripted>>::mint_arm(&commit, "private prompt").unwrap();
    let generation = s.dispatch(arm).unwrap();
    assert_eq!(
        s.active_run(),
        Some((commit.result.run_id.as_str(), generation))
    );
    // Busy: a second arm is refused AND spent.
    let other = Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "x").unwrap();
    assert_eq!(s.dispatch(other).err(), Some(DispatchRefused::Busy));
    // Owner stays responsive: polling never blocks while the provider waits.
    let started = Instant::now();
    let events = wait_events(&mut s, 1);
    assert!(
        matches!(&events[..], [SupervisorEvent::Started { generation: g, .. }] if *g == generation)
    );
    for _ in 0..50 {
        assert!(s.poll().is_empty());
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    steps.send(Step::Complete).unwrap();
    let events = wait_events(&mut s, 1);
    assert!(matches!(
        &events[..],
        [SupervisorEvent::Finished {
            outcome: WorkerOutcome::Completed { .. },
            ..
        }]
    ));
    assert!(s.is_available());
    assert_eq!(
        scripted.seen_prompts.lock().unwrap().as_slice(),
        ["private prompt"]
    );
    // Next dispatch gets a fresh generation.
    let arm = Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p2").unwrap();
    assert_eq!(s.dispatch(arm).unwrap(), generation + 1);
    steps.send(Step::Fail("codex_timeout")).unwrap();
    let events = wait_events(&mut s, 2);
    assert!(matches!(
        &events[1],
        SupervisorEvent::Finished {
            outcome: WorkerOutcome::Failed {
                code: "codex_timeout"
            },
            ..
        }
    ));
    assert!(s.is_available());
}

#[test]
fn cancel_signals_only_the_matching_active_run() {
    let (mut s, steps, _) = supervisor();
    let commit = admission(false);
    s.dispatch(Supervisor::<Arc<Scripted>>::mint_arm(&commit, "p").unwrap())
        .unwrap();
    steps.send(Step::WaitForCancel).unwrap();
    wait_events(&mut s, 1);
    assert!(!s.cancel("some-other-run"));
    assert!(s.poll().is_empty());
    assert!(s.cancel(&commit.result.run_id));
    let events = wait_events(&mut s, 1);
    assert!(matches!(
        &events[..],
        [SupervisorEvent::Finished {
            outcome: WorkerOutcome::Failed {
                code: "codex_cancelled"
            },
            ..
        }]
    ));
    assert!(!s.cancel(&commit.result.run_id));
}

#[test]
fn unproven_teardown_and_panics_quarantine_the_slot_until_restart() {
    for step in [Step::Unproven, Step::Panic] {
        let (mut s, steps, _) = supervisor();
        s.dispatch(Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p").unwrap())
            .unwrap();
        steps.send(step).unwrap();
        let events = wait_events(&mut s, 2);
        assert!(matches!(&events[1], SupervisorEvent::Quarantined { .. }));
        assert!(s.is_quarantined() && !s.is_available());
        let arm = Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p").unwrap();
        assert_eq!(s.dispatch(arm).err(), Some(DispatchRefused::Quarantined));
        assert!(s.poll().is_empty());
        // Shutdown reports the missing proof rather than claiming a clean stop.
        assert!(!s.shutdown(Duration::from_secs(2)));
    }
}

#[test]
fn shutdown_cancels_and_joins_within_its_bound_or_reports_failure() {
    let (mut s, steps, _) = supervisor();
    assert!(s.shutdown(Duration::from_millis(10)));
    s.dispatch(Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p").unwrap())
        .unwrap();
    steps.send(Step::WaitForCancel).unwrap();
    assert!(s.shutdown(Duration::from_secs(5)));
    assert!(s.is_available());
    // A provider that ignores cancellation: bounded wait fails, handle retained.
    let (mut s, steps, _) = supervisor();
    s.dispatch(Supervisor::<Arc<Scripted>>::mint_arm(&admission(false), "p").unwrap())
        .unwrap();
    let begun = Instant::now();
    assert!(!s.shutdown(Duration::from_millis(100)));
    assert!(begun.elapsed() < Duration::from_secs(2));
    assert!(s.active_run().is_some());
    steps.send(Step::Complete).unwrap();
    assert!(s.shutdown(Duration::from_secs(5)));
}

#[test]
fn supervision_code_has_no_store_ipc_or_queue_authority() {
    for source in [include_str!("mod.rs"), include_str!("codex_factory.rs")] {
        for forbidden in [
            "rusqlite",
            "storage::",
            "tauri",
            "enqueue_run",
            "append_batch",
        ] {
            assert!(
                !source.contains(forbidden),
                "worker gained authority: {forbidden}"
            );
        }
    }
}
