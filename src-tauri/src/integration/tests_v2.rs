//! v2 owner thread over the real JSON command path, a fresh V2Store and a
//! scripted in-process provider (no child processes, no live Codex).
use super::coordinator_v2::{CoordinatorV2, OwnerRuntime};
use super::Reply;
use crate::{
    execution::ports::{Clock, ExecutionHealth, IdSource, StoreResult},
    execution_v2 as v2,
    runtime_v2::RuntimeV2,
    storage::store_v2::V2Store,
    supervisor::{Cancel, ProviderFactory, ReportedUsage, WorkerOutcome, WorkerReport},
};
use serde_json::{json, Value};
use std::{
    future::Future,
    path::PathBuf,
    pin::pin,
    sync::{mpsc, Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
struct Dir(PathBuf);
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
        id()
    }
}
#[derive(Clone)]
struct Scripted(Arc<Mutex<mpsc::Receiver<&'static str>>>);
impl ProviderFactory for Scripted {
    fn run(&self, _prompt: &str, cancel: &Cancel, started: &mut dyn FnMut()) -> WorkerReport {
        started();
        let step = self.0.lock().unwrap().recv().unwrap_or("cancel");
        let outcome = if step == "cancel" {
            while !cancel.is_signalled() {
                thread::sleep(Duration::from_millis(1));
            }
            WorkerOutcome::Failed {
                code: "codex_cancelled",
            }
        } else {
            WorkerOutcome::Completed {
                message: step.into(),
                usage: ReportedUsage {
                    input_tokens: 3,
                    cached_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_output_tokens: 0,
                    cache_write_input_tokens: None,
                },
            }
        };
        WorkerReport {
            outcome,
            teardown_proven: true,
        }
    }
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
    let deadline = Instant::now() + Duration::from_secs(10);
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
struct Rig {
    c: CoordinatorV2,
    steps: mpsc::Sender<&'static str>,
    hints: Arc<Mutex<Vec<v2::ExecutionChanged>>>,
    dir: Dir,
}
fn rig(path: Option<PathBuf>) -> Rig {
    let dir = Dir(path.unwrap_or_else(|| {
        let p = std::env::temp_dir().join(format!("aw-ipc-v2-{}", id()));
        std::fs::create_dir(&p).unwrap();
        p
    }));
    let (steps, rx) = mpsc::channel();
    let hints = Arc::new(Mutex::new(Vec::new()));
    let store_dir = dir.0.clone();
    let seen = hints.clone();
    let c = CoordinatorV2::spawn(
        move || {
            let rt = RuntimeV2::new(
                V2Store::open(&store_dir)?,
                Scripted(Arc::new(Mutex::new(rx))),
                v2::CodexAvailability::Ready,
                TestClock,
                Ids,
            )?;
            Ok(Box::new(rt) as Box<dyn OwnerRuntime>)
        },
        move |hint| {
            seen.lock().unwrap().push(hint);
            true
        },
        Duration::from_millis(5),
    );
    Rig {
        c,
        steps,
        hints,
        dir,
    }
}
impl Rig {
    fn call(&self, name: &str, request: Value) -> StoreResult<Value> {
        wait(self.c.submit(name, request)?)
    }
    fn workspace(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.c.health() != ExecutionHealth::Ready {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let boot = self
            .call("bootstrap_workspace_v2", json!({"contract_version": 2}))
            .unwrap();
        boot["snapshot"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn run_state(&self, w: &str, run: &str) -> String {
        let snap = self
            .call(
                "get_workspace_snapshot_v2",
                json!({"contract_version": 2, "workspace_id": w}),
            )
            .unwrap();
        snap["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == run)
            .unwrap()["state"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn until(&self, w: &str, run: &str, state: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.run_state(w, run) != state {
            assert!(Instant::now() < deadline, "never reached {state}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn session(&self, w: &str, provider: &str) -> String {
        self.call(
            "create_session_v2",
            json!({"contract_version": 2, "workspace_id": w, "request_id": id(),
                   "label": "S", "provider_id": provider}),
        )
        .unwrap()["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn codex(&self, w: &str, s: &str, request_id: &str) -> StoreResult<Value> {
        self.call(
            "start_codex_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": s,
                   "request_id": request_id, "prompt": "Hello", "profile_id": "codex_text_v1",
                   "confirmed": true}),
        )
    }
}

#[test]
fn json_commands_run_fake_and_codex_through_the_owner_and_hint_changes() {
    let r = rig(None);
    let w = r.workspace();
    let boot = r
        .call("bootstrap_workspace_v2", json!({"contract_version": 2}))
        .unwrap();
    assert_eq!(boot["storage_state"], "ready");
    assert_eq!(boot["snapshot"]["execution_available"], true);
    assert_eq!(boot["snapshot"]["codex_availability"], "ready");
    let f = r.session(&w, "fake");
    let c = r.session(&w, "codex");
    let fake = r
        .call(
            "start_fake_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": f,
                   "request_id": id(), "provider_id": "fake", "prompt": "p", "scenario": "success"}),
        )
        .unwrap();
    let request_id = id();
    let codex = r.codex(&w, &c, &request_id).unwrap();
    assert_eq!(codex["run"]["profile_id"], "codex_text_v1");
    assert_eq!(codex["run"]["model_id"], "gpt-6.1-sol");
    let run = codex["run"]["run_id"].as_str().unwrap().to_owned();
    assert_eq!(r.codex(&w, &c, &request_id).unwrap()["replayed"], true);
    r.until(&w, fake["run"]["run_id"].as_str().unwrap(), "succeeded");
    r.until(&w, &run, "running");
    r.steps.send("Hi from fixture").unwrap();
    r.until(&w, &run, "succeeded");
    let page = r
        .call(
            "list_run_events_v2",
            json!({"contract_version": 2, "workspace_id": w, "run_id": run,
                   "after_seq": 0, "through_seq": null, "limit": 50}),
        )
        .unwrap();
    let text: String = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "text_delta")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "Hi from fixture");
    assert!(r
        .hints
        .lock()
        .unwrap()
        .iter()
        .all(|h| h.contract_version == 2 && h.workspace_id == w));
    assert!(!r.hints.lock().unwrap().is_empty());
    assert!(r.c.shutdown(Duration::from_secs(10)));
}

#[test]
fn invalid_requests_unknown_fields_and_unconfirmed_codex_are_refused() {
    let r = rig(None);
    let w = r.workspace();
    let c = r.session(&w, "codex");
    let refused = [
        (
            "start_codex_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": c,
            "request_id": id(), "prompt": "x", "profile_id": "codex_text_v1", "confirmed": false}),
        ),
        (
            "start_codex_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": c,
            "request_id": id(), "prompt": "x".repeat(1025), "profile_id": "codex_text_v1", "confirmed": true}),
        ),
        (
            "start_codex_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": c,
            "request_id": id(), "prompt": "x", "profile_id": "codex_text_v1", "confirmed": true,
            "executable": "C:/evil.exe"}),
        ),
        (
            "start_fake_run_v2",
            json!({"contract_version": 2, "workspace_id": w, "session_id": c,
            "request_id": id(), "provider_id": "fake", "prompt": "p", "scenario": "success"}),
        ),
        (
            "get_workspace_snapshot_v2",
            json!({"contract_version": 1, "workspace_id": w}),
        ),
        ("start_codex_run", json!({})),
    ];
    for (name, request) in refused {
        assert!(r.call(name, request).is_err(), "{name} accepted");
    }
    let snap = r
        .call(
            "get_workspace_snapshot_v2",
            json!({"contract_version": 2, "workspace_id": w}),
        )
        .unwrap();
    assert!(snap["runs"].as_array().unwrap().is_empty());
}

#[test]
fn owner_shutdown_cancels_the_active_codex_run_and_reopen_shows_cancelled() {
    let path;
    let run;
    {
        let r = rig(None);
        let w = r.workspace();
        let c = r.session(&w, "codex");
        run = r.codex(&w, &c, &id()).unwrap()["run"]["run_id"]
            .as_str()
            .unwrap()
            .to_owned();
        r.until(&w, &run, "running");
        r.steps.send("cancel").unwrap();
        assert!(r.c.shutdown(Duration::from_secs(10)));
        // The mailbox refuses work after shutdown.
        assert!(r
            .c
            .submit("bootstrap_workspace_v2", json!({"contract_version": 2}))
            .is_err());
        path = r.dir.0.clone();
        std::mem::forget(r.dir); // reuse the directory below; removed at the end
    }
    let r = rig(Some(path));
    let w = r.workspace();
    assert_eq!(r.run_state(&w, &run), "cancelled");
    assert!(r.c.shutdown(Duration::from_secs(10)));
}
