//! Real runtime + real SQLite; no Tauri/UI, executable, provider or network calls.
use agent_workbench::{
    execution::{ports::*, validation::is_terminal, *},
    runtime::Runtime,
    storage::SqliteStore,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

const NOW: &str = "2026-10-19T19:00:00.000Z";
struct TestClock;
impl Clock for TestClock {
    fn now_utc(&self) -> Timestamp {
        NOW.into()
    }
}
struct RandomIds;
impl IdSource for RandomIds {
    fn next_id(&mut self) -> Id {
        id()
    }
}
fn id() -> Id {
    uuid::Uuid::new_v4().to_string()
}
type Engine = Runtime<SqliteStore, TestClock, RandomIds>;

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("workbench-native-seam-{}", id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn open(directory: &Path) -> Engine {
    Runtime::new(SqliteStore::open(directory).unwrap(), TestClock, RandomIds).unwrap()
}
fn snapshot(engine: &mut Engine) -> WorkspaceSnapshot {
    engine
        .bootstrap_workspace(&BootstrapRequest {
            contract_version: 1,
        })
        .unwrap()
}
fn session(engine: &mut Engine) -> SessionSnapshot {
    let workspace_id = snapshot(engine).workspace.workspace_id;
    engine
        .create_session(&CreateSessionRequest {
            contract_version: 1,
            workspace_id,
            request_id: id(),
            label: "SIMULATED integration session".into(),
        })
        .unwrap()
        .session
}
fn start(session: &SessionSnapshot, scenario: FakeScenario) -> StartFakeRunRequest {
    StartFakeRunRequest {
        contract_version: 1,
        workspace_id: session.workspace_id.clone(),
        session_id: session.session_id.clone(),
        request_id: id(),
        provider_id: ProviderId::Fake,
        prompt: "SIMULATED input; never execute this as a command.".into(),
        scenario,
    }
}
fn run(engine: &mut Engine, run_id: &str) -> RunSnapshot {
    snapshot(engine)
        .runs
        .into_iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
}
fn finish(engine: &mut Engine, run_id: &str) -> RunSnapshot {
    for _ in 0..16 {
        let current = run(engine, run_id);
        if is_terminal(current.state) {
            return current;
        }
        engine.tick().unwrap();
    }
    panic!("bounded fake run did not finish");
}
fn cancel(engine: &mut Engine, run: &RunSnapshot) -> CancelRunRequest {
    let request = CancelRunRequest {
        contract_version: 1,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        request_id: id(),
    };
    engine.cancel_run(&request).unwrap();
    request
}
fn events(engine: &mut Engine, run: &RunSnapshot) -> Vec<RunEvent> {
    engine
        .list_run_events(&EventPageRequest {
            contract_version: 1,
            workspace_id: run.workspace_id.clone(),
            run_id: run.run_id.clone(),
            after_seq: 0,
            through_seq: Some(run.last_seq),
            limit: 200,
        })
        .unwrap()
        .events
}

#[test]
fn fake_lifecycle_replay_provider_guards_and_cancellation_cross_real_ports() {
    let directory = TestDirectory::new();
    let mut engine = open(&directory.0);
    let session = session(&mut engine);
    assert_eq!(engine.health(), ExecutionHealth::Ready);
    let request = start(&session, FakeScenario::Success);
    let accepted = engine.start_fake_run(&request).unwrap();
    assert_eq!(accepted.run.state, RunState::Queued);
    let completed = finish(&mut engine, &accepted.run.run_id);
    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(
        completed.usage,
        Usage {
            input_tokens: None,
            output_tokens: None,
            estimated: false
        }
    );
    let history = events(&mut engine, &completed);
    assert!(history.iter().any(|event| matches!(event, RunEvent::TextDelta { payload, source: EventSource::Fixture, .. } if payload.text.starts_with("SIMULATED:"))));
    assert_eq!(history.len() as u64, completed.last_seq);
    assert!(engine.take_changed().is_some());
    assert!(engine.take_changed().is_none());

    let replay = engine.start_fake_run(&request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run, accepted.run); // immutable old queued receipt, not current success
    assert!(engine.take_changed().is_none());
    for _ in 0..3 {
        engine.tick().unwrap();
    }
    assert_eq!(run(&mut engine, &completed.run_id), completed);
    assert_eq!(events(&mut engine, &completed), history);

    let count = snapshot(&mut engine).runs.len();
    for provider_id in [ProviderId::Codex, ProviderId::Claude] {
        let mut disabled = start(&session, FakeScenario::Success);
        disabled.provider_id = provider_id;
        assert_eq!(
            engine.start_fake_run(&disabled).unwrap_err().code,
            "provider_execution_disabled"
        );
    }
    assert_eq!(snapshot(&mut engine).runs.len(), count);
    assert_eq!(engine.health(), ExecutionHealth::Ready);

    let failure = engine
        .start_fake_run(&start(&session, FakeScenario::Failure))
        .unwrap();
    let failed = finish(&mut engine, &failure.run.run_id);
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.terminal_error.unwrap().code, "fake_start_failed");

    let waiting = engine
        .start_fake_run(&start(&session, FakeScenario::WaitForCancel))
        .unwrap();
    let queued = engine
        .start_fake_run(&start(&session, FakeScenario::Success))
        .unwrap();
    for _ in 0..6 {
        engine.tick().unwrap();
    }
    let waiting = run(&mut engine, &waiting.run.run_id);
    assert_eq!(waiting.state, RunState::Running);
    assert_eq!(run(&mut engine, &queued.run.run_id).state, RunState::Queued);
    cancel(&mut engine, &queued.run);
    assert_eq!(
        run(&mut engine, &queued.run.run_id).state,
        RunState::Cancelled
    );
    let cancellation = cancel(&mut engine, &waiting);
    let stopped = finish(&mut engine, &waiting.run_id);
    assert_eq!(stopped.state, RunState::Cancelled);
    let replay = engine.cancel_run(&cancellation).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run.state, RunState::CancelRequested);
    assert_eq!(run(&mut engine, &waiting.run_id), stopped);
}

#[test]
fn reopen_restores_layout_history_and_interrupts_without_reexecution() {
    let directory = TestDirectory::new();
    let mut engine = open(&directory.0);
    let session = session(&mut engine);
    let initial = snapshot(&mut engine);
    let workspace_id = initial.workspace.workspace_id;
    let mut layout = initial.layout.unwrap();
    let geometry = Geometry {
        x: 48.0,
        y: 64.0,
        width: 460.0,
        height: 350.0,
    };
    layout.panels.push(PanelLayout {
        panel_id: id(),
        session_id: session.session_id.clone(),
        container_id: None,
        stage: PanelStage::Compact,
        geometry: geometry.clone(),
        expanded_geometry: geometry,
    });
    let first = SaveLayoutRequest {
        contract_version: 1,
        workspace_id: workspace_id.clone(),
        request_id: id(),
        expected_revision: 0,
        layout,
    };
    let saved = engine.save_layout(&first).unwrap().layout;
    let mut second = SaveLayoutRequest {
        contract_version: 1,
        workspace_id: workspace_id.clone(),
        request_id: id(),
        expected_revision: 1,
        layout: saved.clone(),
    };
    second.layout.theme = ThemeName::Light;
    second.layout.panels[0].geometry.x = 96.0;
    let latest = engine.save_layout(&second).unwrap().layout;
    assert_eq!(latest.revision, 2);
    assert_eq!(engine.save_layout(&first).unwrap().layout, saved);
    assert_eq!(snapshot(&mut engine).layout.as_ref(), Some(&latest));
    let mut conflict = first.clone();
    conflict.request_id = id();
    assert_eq!(
        engine.save_layout(&conflict).unwrap_err().code,
        "layout_revision_conflict"
    );

    let active = engine
        .start_fake_run(&start(&session, FakeScenario::WaitForCancel))
        .unwrap();
    let pending_request = start(&session, FakeScenario::Success);
    let pending = engine.start_fake_run(&pending_request).unwrap();
    for _ in 0..6 {
        engine.tick().unwrap();
    }
    let active = run(&mut engine, &active.run.run_id);
    assert_eq!(active.state, RunState::Running);
    assert_eq!(
        run(&mut engine, &pending.run.run_id).state,
        RunState::Queued
    );
    drop(engine);

    let mut engine = open(&directory.0);
    let restored = snapshot(&mut engine);
    assert_eq!(restored.workspace.workspace_id, workspace_id);
    assert_eq!(restored.sessions, vec![session]);
    assert_eq!(restored.layout.as_ref(), Some(&latest));
    assert!(restored
        .runs
        .iter()
        .all(|run| run.state == RunState::Interrupted));
    for old in [&active, &pending.run] {
        let interrupted = run(&mut engine, &old.run_id);
        assert_eq!(interrupted.last_seq, old.last_seq + 1);
        let history = events(&mut engine, &interrupted);
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(
                    event,
                    RunEvent::RunRecovered {
                        source: EventSource::System,
                        ..
                    }
                ))
                .count(),
            1
        );
    }
    let reply = engine.start_fake_run(&pending_request).unwrap();
    assert!(reply.replayed);
    assert_eq!(reply.run, pending.run);
    for _ in 0..6 {
        engine.tick().unwrap();
    }
    assert_eq!(snapshot(&mut engine).runs, restored.runs);
    assert_eq!(snapshot(&mut engine).layout, restored.layout);
}
