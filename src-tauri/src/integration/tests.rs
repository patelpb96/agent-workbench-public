use super::{coordinator::test_coordinator, *};
use crate::{
    execution::{ports::*, validation as v, *},
    runtime::Runtime,
    storage::SqliteStore,
};
use serde_json::{json, Value};
use std::{
    fs,
    future::Future,
    path::PathBuf,
    pin::pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

const NOW: &str = "2026-10-20T02:00:00.000Z";
const DEADLINE: Duration = Duration::from_secs(10);
const NO_TICK: Duration = Duration::from_secs(3600);
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
struct Clock;
impl ports::Clock for Clock {
    fn now_utc(&self) -> String {
        NOW.into()
    }
}
struct Ids;
impl IdSource for Ids {
    fn next_id(&mut self) -> String {
        id()
    }
}
type Engine = Runtime<SqliteStore, Clock, Ids>;
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("workbench-ipc-{}", id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Rig {
    coordinator: Coordinator,
    _directory: Directory,
}
impl Rig {
    fn new(cadence: Duration) -> Self {
        let directory = Directory::new();
        let path = directory.0.clone();
        let coordinator = test_coordinator(move || Ok(Box::new(open(&path))), |_| true, cadence);
        Self {
            coordinator,
            _directory: directory,
        }
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.coordinator.shutdown(DEADLINE);
    }
}
fn open(path: &std::path::Path) -> Engine {
    Runtime::new(SqliteStore::open(path).unwrap(), Clock, Ids).unwrap()
}
struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn wait(reply: Reply) -> StoreResult<Value> {
    let mut reply = pin!(reply);
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + DEADLINE;
    loop {
        match reply.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                assert!(Instant::now() < deadline, "reply timed out");
                thread::park_timeout(Duration::from_millis(5));
            }
        }
    }
}
fn call(coordinator: &Coordinator, name: &str, request: Value) -> StoreResult<Value> {
    wait(coordinator.submit(name, request)?)
}
fn snapshot(coordinator: &Coordinator) -> WorkspaceSnapshot {
    serde_json::from_value(
        call(
            coordinator,
            "bootstrap_workspace",
            json!({"contract_version":1}),
        )
        .unwrap(),
    )
    .unwrap()
}
fn session(coordinator: &Coordinator) -> SessionSnapshot {
    let workspace_id = snapshot(coordinator).workspace.workspace_id;
    let value = call(coordinator, "create_session", json!({"contract_version":1,"workspace_id":workspace_id,"request_id":id(),"label":"SIMULATED IPC session"})).unwrap();
    serde_json::from_value::<CreateSessionResponse>(value)
        .unwrap()
        .session
}
fn start(session: &SessionSnapshot, scenario: &str) -> Value {
    json!({"contract_version":1,"workspace_id":session.workspace_id,"session_id":session.session_id,"request_id":id(),"provider_id":"fake","prompt":"SIMULATED: never run a process","scenario":scenario})
}
fn cancellation(session: &SessionSnapshot, run_id: &str) -> Value {
    json!({"contract_version":1,"workspace_id":session.workspace_id,"run_id":run_id,"request_id":id()})
}
fn run(coordinator: &Coordinator, run_id: &str) -> RunSnapshot {
    snapshot(coordinator)
        .runs
        .into_iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
}
fn until(coordinator: &Coordinator, run_id: &str, target: RunState) -> RunSnapshot {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let run = run(coordinator, run_id);
        if run.state == target {
            return run;
        }
        assert!(
            Instant::now() < deadline,
            "did not reach {target:?}: {:?}",
            run.state
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn accepted(coordinator: &Coordinator, request: Value) -> StartFakeRunResponse {
    serde_json::from_value(call(coordinator, "start_fake_run", request).unwrap()).unwrap()
}

#[test]
fn seven_wire_commands_round_trip_replay_and_reopen_without_redispatch() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let coordinator = test_coordinator(
        move || Ok(Box::new(open(&path))),
        |_| true,
        Duration::from_millis(5),
    );
    let session = session(&coordinator);
    let request = start(&session, "success");
    let original = accepted(&coordinator, request.clone());
    let completed = until(&coordinator, &original.run.run_id, RunState::Succeeded);
    let replay = accepted(&coordinator, request.clone());
    assert!(replay.replayed);
    assert_eq!(replay.run, original.run);
    let failure = accepted(&coordinator, start(&session, "failure"));
    until(&coordinator, &failure.run.run_id, RunState::Failed);
    let active = accepted(&coordinator, start(&session, "wait_for_cancel"));
    until(&coordinator, &active.run.run_id, RunState::Running);
    let queued = accepted(&coordinator, start(&session, "success"));
    let cancel_queued = cancellation(&session, &queued.run.run_id);
    let cancelled = call(&coordinator, "cancel_run", cancel_queued.clone()).unwrap();
    assert_eq!(cancelled["run"]["state"], "cancelled");
    assert_eq!(
        call(&coordinator, "cancel_run", cancel_queued).unwrap()["replayed"],
        true
    );
    let intent = call(
        &coordinator,
        "cancel_run",
        cancellation(&session, &active.run.run_id),
    )
    .unwrap();
    assert_eq!(intent["run"]["state"], "cancel_requested");
    until(&coordinator, &active.run.run_id, RunState::Cancelled);
    // A committed terminal result wins a later cancellation.
    assert_eq!(
        call(
            &coordinator,
            "cancel_run",
            cancellation(&session, &completed.run_id)
        )
        .unwrap()["run"]["state"],
        "succeeded"
    );
    let ws_request = json!({"contract_version":1,"workspace_id":session.workspace_id});
    let mut ws = call(&coordinator, "get_workspace_snapshot", ws_request.clone()).unwrap();
    ws["layout"]["theme"] = "contrast".into();
    let save = json!({"contract_version":1,"workspace_id":session.workspace_id,"request_id":id(),"expected_revision":0,"layout":ws["layout"]});
    assert_eq!(
        call(&coordinator, "save_layout", save.clone()).unwrap()["layout"]["revision"],
        1
    );
    let events_request = json!({"contract_version":1,"workspace_id":session.workspace_id,"run_id":completed.run_id,"after_seq":0,"through_seq":completed.last_seq,"limit":200});
    let history = call(&coordinator, "list_run_events", events_request.clone()).unwrap();
    assert_eq!(
        history["events"].as_array().unwrap().len() as u64,
        completed.last_seq
    );
    assert!(!serde_json::to_string(&snapshot(&coordinator))
        .unwrap()
        .contains("never run a process"));
    // Leave active work to prove coordinator shutdown doesn't forge a terminal.
    let interrupted = accepted(&coordinator, start(&session, "wait_for_cancel"));
    until(&coordinator, &interrupted.run.run_id, RunState::Running);
    assert!(coordinator.shutdown(DEADLINE));
    let path = directory.0.clone();
    let reopened = test_coordinator(
        move || Ok(Box::new(open(&path))),
        |_| true,
        Duration::from_millis(5),
    );
    let restored = snapshot(&reopened);
    assert_eq!(restored.workspace.workspace_id, session.workspace_id);
    assert_eq!(restored.layout.as_ref().unwrap().theme, ThemeName::Contrast);
    assert_eq!(
        run(&reopened, &interrupted.run.run_id).state,
        RunState::Interrupted
    );
    assert_eq!(
        call(&reopened, "list_run_events", events_request).unwrap(),
        history
    );
    assert_eq!(
        call(&reopened, "save_layout", save).unwrap()["replayed"],
        true
    );
    assert_eq!(accepted(&reopened, request).run, original.run);
    assert_eq!(run(&reopened, &completed.run_id), completed);
    assert!(reopened.shutdown(DEADLINE));
}

struct Probe {
    engine: Engine,
    log: Arc<Mutex<Vec<&'static str>>>,
    fault: Arc<AtomicUsize>,
    unhealthy: bool,
}
macro_rules! forwarded {
    ($(($method:ident, $request:ty, $response:ty)),* $(,)?) => { $(
        fn $method(&mut self, request: &$request) -> StoreResult<$response> {
            self.log.lock().unwrap().push(stringify!($method));
            self.engine.$method(request)
        }
    )* };
}
impl ExecutionRuntime for Probe {
    forwarded! {
        (bootstrap_workspace, BootstrapRequest, WorkspaceSnapshot),
        (start_fake_run, StartFakeRunRequest, StartFakeRunResponse),
        (cancel_run, CancelRunRequest, CancelRunResponse),
        (get_workspace_snapshot, WorkspaceRequest, WorkspaceSnapshot),
        (save_layout, SaveLayoutRequest, SaveLayoutResponse),
    }
    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
    ) -> StoreResult<CreateSessionResponse> {
        let mut response = self.engine.create_session(request)?;
        if self.fault.load(Ordering::SeqCst) == 3 {
            response.session.label.clear();
        }
        Ok(response)
    }
    fn list_run_events(&mut self, request: &EventPageRequest) -> StoreResult<EventPage> {
        let mut response = self.engine.list_run_events(request)?;
        match self.fault.load(Ordering::SeqCst) {
            2 => {
                response.events.clear();
            }
            5 => {
                if let Some(event) = response.events.first_mut() {
                    let mut value = serde_json::to_value(&*event).unwrap();
                    value["session_id"] = id().into();
                    *event = serde_json::from_value(value).unwrap();
                }
            }
            _ => {}
        }
        Ok(response)
    }
    fn tick(&mut self) -> StoreResult<()> {
        self.log.lock().unwrap().push("tick");
        match self.fault.load(Ordering::SeqCst) {
            1 => {
                self.unhealthy = true;
                Err(v::error(
                    "storage_unavailable",
                    "C:/private/secret-do-not-expose",
                ))
            }
            4 => panic!("owned test runtime panic"),
            _ => self.engine.tick(),
        }
    }
    fn take_changed(&mut self) -> Option<ExecutionChanged> {
        self.engine.take_changed()
    }
    fn health(&self) -> ExecutionHealth {
        if self.unhealthy {
            ExecutionHealth::StorageUnavailable
        } else {
            self.engine.health()
        }
    }
}
fn probe(
    engine: Engine,
    log: Arc<Mutex<Vec<&'static str>>>,
    fault: Arc<AtomicUsize>,
) -> Box<dyn ExecutionRuntime> {
    Box::new(Probe {
        engine,
        log,
        fault,
        unhealthy: false,
    })
}

#[test]
fn bounded_admission_control_priority_and_dropped_callers_do_not_bypass_slots() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let log = Arc::new(Mutex::new(Vec::new()));
    let worker_log = log.clone();
    let (release, gate) = mpsc::sync_channel(0);
    let coordinator = test_coordinator(
        move || {
            let engine = open(&path);
            gate.recv_timeout(DEADLINE).unwrap();
            Ok(probe(engine, worker_log, Arc::new(AtomicUsize::new(0))))
        },
        |_| true,
        NO_TICK,
    );
    let mut ordinary = Vec::new();
    for _ in 0..COMMAND_MAILBOX {
        ordinary.push(
            coordinator
                .submit("bootstrap_workspace", json!({"contract_version":1}))
                .unwrap(),
        );
    }
    assert_eq!(
        coordinator
            .submit("bootstrap_workspace", json!({"contract_version":1}))
            .err()
            .unwrap()
            .code,
        "storage_busy"
    );
    let cancel = json!({"contract_version":1,"workspace_id":id(),"run_id":id(),"request_id":id()});
    let mut controls = Vec::new();
    for _ in 0..CONTROL_MAILBOX {
        controls.push(coordinator.submit("cancel_run", cancel.clone()).unwrap());
    }
    assert_eq!(
        coordinator
            .submit("cancel_run", cancel.clone())
            .err()
            .unwrap()
            .code,
        "storage_busy"
    );
    drop(ordinary.pop()); // Still admitted and pending; its sender retains the slot.
    assert_eq!(
        coordinator
            .submit("bootstrap_workspace", json!({"contract_version":1}))
            .err()
            .unwrap()
            .code,
        "storage_busy"
    );
    release.send(()).unwrap();
    for reply in controls {
        assert_eq!(wait(reply).unwrap_err().code, "workspace_not_found");
    }
    for reply in ordinary {
        wait(reply).unwrap();
    }
    let log = log.lock().unwrap();
    assert_eq!(log[0], "bootstrap_workspace"); // startup owner check, not admission
    assert!(log[1..=CONTROL_MAILBOX]
        .iter()
        .all(|name| *name == "cancel_run"));
    assert_eq!(log[CONTROL_MAILBOX + 1], "bootstrap_workspace");
    assert!(!log.contains(&"tick"));
    drop(log);
    assert!(coordinator.shutdown(DEADLINE));
}

#[test]
fn boundary_rejects_unknown_fields_required_nulls_bad_versions_and_real_providers_before_admission()
{
    let rig = Rig::new(NO_TICK);
    let coordinator = &rig.coordinator;
    let session = session(coordinator);
    let mut commands = vec![
        ("bootstrap_workspace", json!({"contract_version":1})),
        (
            "create_session",
            json!({"contract_version":1,"workspace_id":session.workspace_id,"request_id":id(),"label":"SIMULATED"}),
        ),
        ("start_fake_run", start(&session, "success")),
        ("cancel_run", cancellation(&session, &id())),
        (
            "get_workspace_snapshot",
            json!({"contract_version":1,"workspace_id":session.workspace_id}),
        ),
        (
            "list_run_events",
            json!({"contract_version":1,"workspace_id":session.workspace_id,"run_id":id(),"after_seq":0,"through_seq":null,"limit":200}),
        ),
        (
            "save_layout",
            json!({"contract_version":1,"workspace_id":session.workspace_id,"request_id":id(),"expected_revision":0,"layout":snapshot(coordinator).layout}),
        ),
    ];
    for (name, request) in &commands {
        let mut extra = request.clone();
        extra["private_path"] = "C:/secret".into();
        assert_eq!(
            coordinator.submit(name, extra).err().unwrap().code,
            "validation_failed"
        );
        for key in request.as_object().unwrap().keys() {
            let mut missing = request.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert_eq!(
                coordinator.submit(name, missing).err().unwrap().code,
                "validation_failed",
                "{name}:{key}"
            );
        }
        let mut version = request.clone();
        version["contract_version"] = 2.into();
        assert_eq!(
            coordinator.submit(name, version).err().unwrap().code,
            "unsupported_contract_version"
        );
    }
    for provider in ["codex", "claude"] {
        let request = &mut commands[2].1;
        request["provider_id"] = provider.into();
        assert_eq!(
            coordinator
                .submit("start_fake_run", request.clone())
                .err()
                .unwrap()
                .code,
            "provider_execution_disabled"
        );
    }
    assert!(snapshot(coordinator).runs.is_empty());
    let mut oversized = start(&session, "success");
    oversized["prompt"] = "x".repeat(PROMPT_BYTES * 7).into();
    assert_eq!(
        coordinator
            .submit("start_fake_run", oversized)
            .err()
            .unwrap()
            .code,
        "validation_failed"
    );
    assert_eq!(
        coordinator.submit("unknown", json!({})).err().unwrap().code,
        "validation_failed"
    );
}

#[test]
fn outbound_failures_stop_mutations_and_validate_page_cardinality_and_session_ownership() {
    for fault_mode in [2, 3, 5] {
        let directory = Directory::new();
        let path = directory.0.clone();
        let fault = Arc::new(AtomicUsize::new(0));
        let worker_fault = fault.clone();
        let coordinator = test_coordinator(
            move || {
                Ok(probe(
                    open(&path),
                    Arc::new(Mutex::new(Vec::new())),
                    worker_fault,
                ))
            },
            |_| true,
            NO_TICK,
        );
        let session = session(&coordinator);
        let queued = accepted(&coordinator, start(&session, "success"));
        fault.store(fault_mode, Ordering::SeqCst);
        let failure = if fault_mode == 3 {
            call(&coordinator, "create_session", json!({"contract_version":1,"workspace_id":session.workspace_id,"request_id":id(),"label":"SIMULATED"})).unwrap_err()
        } else {
            call(&coordinator, "list_run_events", json!({"contract_version":1,"workspace_id":session.workspace_id,"run_id":queued.run.run_id,"after_seq":0,"through_seq":null,"limit":200})).unwrap_err()
        };
        assert_eq!(failure.code, "invalid_execution_response");
        assert_eq!(coordinator.health(), ExecutionHealth::StorageUnavailable);
        assert_eq!(
            coordinator
                .submit("start_fake_run", start(&session, "success"))
                .err()
                .unwrap()
                .code,
            "storage_unavailable"
        );
        assert!(!snapshot(&coordinator).execution_available);
        assert!(coordinator.shutdown(DEADLINE));
    }
}

#[test]
fn initialization_failure_panic_and_health_only_tick_failure_release_callers_without_false_completion(
) {
    let failed = test_coordinator(
        || Err(v::error("unsupported_storage_version", "C:/private/secret")),
        |_| true,
        NO_TICK,
    );
    let error = call(
        &failed,
        "bootstrap_workspace",
        json!({"contract_version":1}),
    )
    .unwrap_err();
    assert!(!serde_json::to_string(&error).unwrap().contains("secret"));
    assert!(failed.shutdown(DEADLINE));
    for fault_mode in [1, 4] {
        let directory = Directory::new();
        let path = directory.0.clone();
        let fault = Arc::new(AtomicUsize::new(0));
        let worker_fault = fault.clone();
        let (notify, hints) = mpsc::sync_channel(32);
        let coordinator = test_coordinator(
            move || {
                Ok(probe(
                    open(&path),
                    Arc::new(Mutex::new(Vec::new())),
                    worker_fault,
                ))
            },
            move |hint| notify.try_send(hint).is_ok(),
            Duration::from_millis(10),
        );
        let session = session(&coordinator);
        let queued = accepted(&coordinator, start(&session, "wait_for_cancel"));
        until(&coordinator, &queued.run.run_id, RunState::Running);
        // Remove old hints; new failure doesn't set Runtime::changed.
        while hints.try_recv().is_ok() {}
        fault.store(fault_mode, Ordering::SeqCst);
        let deadline = Instant::now() + DEADLINE;
        while coordinator.health() != ExecutionHealth::StorageUnavailable {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        if fault_mode == 1 {
            hints.recv_timeout(DEADLINE).unwrap();
            let visible = snapshot(&coordinator);
            assert!(!visible.execution_available);
            assert_eq!(visible.runs[0].state, RunState::Running);
        }
        assert_eq!(
            coordinator
                .submit("cancel_run", cancellation(&session, &queued.run.run_id))
                .err()
                .unwrap()
                .code,
            "storage_unavailable"
        );
        assert!(coordinator.shutdown(DEADLINE));
        let mut reopened = open(&directory.0);
        assert_eq!(
            reopened
                .bootstrap_workspace(&BootstrapRequest {
                    contract_version: 1
                })
                .unwrap()
                .runs[0]
                .state,
            RunState::Interrupted
        );
    }
}

#[test]
fn failed_notifications_retry_only_bounded_hint_and_observe_committed_rows() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let notified_path = directory.0.clone();
    let fail = Arc::new(AtomicBool::new(true));
    let notifier_fail = fail.clone();
    let (notify, hints) = mpsc::sync_channel(32);
    let coordinator = test_coordinator(
        move || Ok(Box::new(open(&path))),
        move |hint| {
            assert_eq!(hint.contract_version, 1);
            v::validate_id(&hint.workspace_id).unwrap();
            // Read-only independent connection observes only committed data.
            let read = rusqlite::Connection::open_with_flags(
                notified_path.join("execution.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: u64 = read
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
                .unwrap();
            notify.try_send(count).unwrap();
            !notifier_fail.load(Ordering::SeqCst)
        },
        Duration::from_millis(10),
    );
    snapshot(&coordinator);
    let session = session(&coordinator);
    let deadline = Instant::now() + DEADLINE;
    loop {
        if hints.recv_timeout(DEADLINE).unwrap() == 1 {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    fail.store(false, Ordering::SeqCst);
    assert_eq!(hints.recv_timeout(DEADLINE).unwrap(), 1);
    assert_eq!(snapshot(&coordinator).sessions[0], session);
    assert_eq!(snapshot(&coordinator).sessions.len(), 1);
    assert!(coordinator.shutdown(DEADLINE));
}

#[test]
fn admitted_mutation_commits_after_caller_drop_and_replays_original_receipt() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let (release, gate) = mpsc::sync_channel(0);
    let (identified, identity) = mpsc::sync_channel(1);
    let coordinator = test_coordinator(
        move || {
            let mut engine = open(&path);
            identified
                .send(
                    engine
                        .bootstrap_workspace(&BootstrapRequest {
                            contract_version: 1,
                        })
                        .unwrap()
                        .workspace
                        .workspace_id,
                )
                .unwrap();
            gate.recv_timeout(DEADLINE).unwrap();
            Ok(Box::new(engine))
        },
        |_| true,
        NO_TICK,
    );
    let workspace_id = identity.recv_timeout(DEADLINE).unwrap();
    let request = json!({"contract_version":1,"workspace_id":workspace_id,"request_id":id(),"label":"SIMULATED dropped caller"});
    drop(
        coordinator
            .submit("create_session", request.clone())
            .unwrap(),
    );
    release.send(()).unwrap();
    // Ordinary FIFO barrier: the dropped caller must not cancel accepted work.
    let observed = snapshot(&coordinator);
    assert_eq!(observed.sessions.len(), 1);
    let replay = call(&coordinator, "create_session", request).unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(
        replay["session"]["session_id"],
        observed.sessions[0].session_id
    );
    assert!(coordinator.shutdown(DEADLINE));
}

#[test]
fn reply_slots_bound_unconsumed_results_and_senders_after_caller_drop() {
    let slots = Arc::new(AtomicUsize::new(0));
    let permit = reply::Permit::acquire(&slots, 1).unwrap();
    let (sender, receiver) = reply::pair(permit);
    sender.complete(Ok(json!({"bounded":"reply"})));
    assert_eq!(
        reply::Permit::acquire(&slots, 1).err().unwrap().code,
        "storage_busy"
    );
    assert_eq!(wait(receiver).unwrap()["bounded"], "reply");
    let (sender, receiver) = reply::pair(reply::Permit::acquire(&slots, 1).unwrap());
    drop(receiver);
    assert_eq!(
        reply::Permit::acquire(&slots, 1).err().unwrap().code,
        "storage_busy"
    );
    drop(sender);
    assert_eq!(slots.load(Ordering::SeqCst), 0);
}

#[test]
fn due_tick_cannot_beat_already_admitted_cancellation_of_final_success_step() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let (release, gate) = mpsc::sync_channel(0);
    let (prepared, ready) = mpsc::sync_channel(1);
    let coordinator = test_coordinator(
        move || {
            let mut engine = open(&path);
            let workspace_id = engine
                .bootstrap_workspace(&BootstrapRequest {
                    contract_version: 1,
                })
                .unwrap()
                .workspace
                .workspace_id;
            let session = engine
                .create_session(&CreateSessionRequest {
                    contract_version: 1,
                    workspace_id,
                    request_id: id(),
                    label: "SIMULATED".into(),
                })
                .unwrap()
                .session;
            let request: StartFakeRunRequest =
                serde_json::from_value(start(&session, "success")).unwrap();
            let queued = engine.start_fake_run(&request).unwrap().run;
            // Queued -> starting -> running -> 3 deltas -> usage; success is next.
            for _ in 0..6 {
                engine.tick().unwrap();
            }
            let current = engine
                .get_workspace_snapshot(&WorkspaceRequest {
                    contract_version: 1,
                    workspace_id: session.workspace_id.clone(),
                })
                .unwrap()
                .runs[0]
                .clone();
            assert_eq!(current.state, RunState::Running);
            assert_eq!(current.last_seq, 7);
            prepared.send((session, queued.run_id)).unwrap();
            gate.recv_timeout(DEADLINE).unwrap();
            Ok(Box::new(engine))
        },
        |_| true,
        Duration::from_nanos(1),
    );
    let (session, run_id) = ready.recv_timeout(DEADLINE).unwrap();
    let reply = coordinator
        .submit("cancel_run", cancellation(&session, &run_id))
        .unwrap();
    release.send(()).unwrap();
    assert_eq!(wait(reply).unwrap()["run"]["state"], "cancel_requested");
    let cancelled = until(&coordinator, &run_id, RunState::Cancelled);
    assert_eq!(cancelled.last_seq, 9); // intent, acknowledgement, never success
    assert!(coordinator.shutdown(DEADLINE));
}

#[test]
fn bounded_control_quantum_services_ordinary_work_and_due_ticks() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let log = Arc::new(Mutex::new(Vec::new()));
    let worker_log = log.clone();
    let (release, gate) = mpsc::sync_channel(0);
    let coordinator = test_coordinator(
        move || {
            let engine = open(&path);
            gate.recv_timeout(DEADLINE).unwrap();
            Ok(probe(engine, worker_log, Arc::new(AtomicUsize::new(0))))
        },
        |_| true,
        Duration::from_nanos(1),
    );
    let mut ordinary = Vec::new();
    let mut controls = Vec::new();
    for _ in 0..COMMAND_MAILBOX {
        ordinary.push(
            coordinator
                .submit("bootstrap_workspace", json!({"contract_version":1}))
                .unwrap(),
        );
    }
    for _ in 0..CONTROL_MAILBOX {
        controls.push(coordinator.submit("cancel_run", json!({"contract_version":1,"workspace_id":id(),"run_id":id(),"request_id":id()})).unwrap());
    }
    release.send(()).unwrap();
    for reply in controls {
        assert_eq!(wait(reply).unwrap_err().code, "workspace_not_found");
    }
    for reply in ordinary {
        wait(reply).unwrap();
    }
    assert!(coordinator.shutdown(DEADLINE));
    let log = log.lock().unwrap();
    assert_eq!(log[CONTROL_MAILBOX + 1], "bootstrap_workspace");
    assert_eq!(log[CONTROL_MAILBOX + 2], "tick");
    assert_eq!(
        log.iter().filter(|name| **name == "cancel_run").count(),
        CONTROL_MAILBOX
    );
    assert_eq!(
        log.iter()
            .filter(|name| **name == "bootstrap_workspace")
            .count(),
        COMMAND_MAILBOX + 1
    );
}

#[test]
fn queued_shutdown_is_bounded_and_does_not_execute_admitted_mutations() {
    let directory = Directory::new();
    let path = directory.0.clone();
    let (release, gate) = mpsc::sync_channel(0);
    let (identified, identity) = mpsc::sync_channel(1);
    let coordinator = test_coordinator(
        move || {
            let mut engine = open(&path);
            let workspace = engine
                .bootstrap_workspace(&BootstrapRequest {
                    contract_version: 1,
                })
                .unwrap()
                .workspace
                .workspace_id;
            identified.send(workspace).unwrap();
            gate.recv_timeout(DEADLINE).unwrap();
            Ok(Box::new(engine))
        },
        |_| true,
        NO_TICK,
    );
    let workspace_id = identity.recv_timeout(DEADLINE).unwrap();
    let pending = coordinator
        .submit("create_session", json!({"contract_version":1,"workspace_id":workspace_id,"request_id":id(),"label":"SIMULATED not executed"}))
        .unwrap();
    assert!(!coordinator.shutdown(Duration::ZERO));
    assert_eq!(
        coordinator
            .submit("bootstrap_workspace", json!({"contract_version":1}))
            .err()
            .unwrap()
            .code,
        "storage_unavailable"
    );
    release.send(()).unwrap();
    assert_eq!(wait(pending).unwrap_err().code, "storage_unavailable");
    assert!(coordinator.shutdown(DEADLINE));
    let mut reopened = open(&directory.0);
    assert!(reopened
        .bootstrap_workspace(&BootstrapRequest {
            contract_version: 1
        })
        .unwrap()
        .sessions
        .is_empty());
}
