//! Test-only stdio transport for headless DOM integration tests. Not linked into
//! the desktop executable; no Tauri App/Window/WebView, HTTP listener or OS input.
use agent_workbench::execution::{Error, NONTEXT_EVENT_BYTES, PROMPT_BYTES};
use agent_workbench::execution::ports::{Clock, IdSource};
use agent_workbench::execution_v2::{CodexAvailability, ExecutionChanged};
use agent_workbench::integration::{CoordinatorV2, OwnerRuntime, Reply};
use agent_workbench::runtime_v2::RuntimeV2;
use agent_workbench::storage::store_v2::V2Store;
use agent_workbench::supervisor::{Cancel, ProviderFactory, WorkerOutcome, WorkerReport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    future::Future,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

#[path = "headless_bridge/history.rs"]
mod history;

const PREFIX: &str = "AW_HEADLESS\t";
const MAX_REQUEST: u64 = (PROMPT_BYTES * 6 + NONTEXT_EVENT_BYTES + 1024) as u64;
const OWNER_FILE: &str = "headless-owner.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease { root: PathBuf, nonce: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call { id: u64, command: String, request: Value }

fn not_reparse(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() { return Err(io::Error::other("symlink refused")); }
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 { return Err(io::Error::other("reparse point refused")); }
    }
    Ok(())
}
fn canonical_uuid(value: &str) -> bool {
    agent_workbench::execution::validation::validate_id(value).is_ok()
}
fn validate_lease(lease: &Lease) -> io::Result<PathBuf> {
    not_reparse(&lease.root)?;
    let root = fs::canonicalize(&lease.root)?;
    let temporary = fs::canonicalize(std::env::temp_dir())?;
    let name = root.file_name().and_then(|v| v.to_str()).unwrap_or("");
    let valid_name = name.strip_prefix("agent-workbench-headless-").is_some_and(canonical_uuid);
    if root.parent() != Some(temporary.as_path()) || !valid_name || !canonical_uuid(&lease.nonce) {
        return Err(io::Error::other("not an isolated headless lease"));
    }
    let marker = root.join(OWNER_FILE);
    not_reparse(&marker)?;
    if fs::metadata(&marker)?.len() > 4096 { return Err(io::Error::other("invalid ownership marker")); }
    let stored: Lease = serde_json::from_slice(&fs::read(marker)?)?;
    if stored.nonce != lease.nonce || stored.root != root { return Err(io::Error::other("lease mismatch")); }
    not_reparse(&root.join("workspace"))?;
    Ok(root)
}
fn new_lease() -> io::Result<Lease> {
    let root = fs::canonicalize(std::env::temp_dir())?
        .join(format!("agent-workbench-headless-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root)?; // Never adopt a pre-existing path.
    let lease = Lease { root, nonce: uuid::Uuid::new_v4().to_string() };
    let mut marker = OpenOptions::new().write(true).create_new(true).open(lease.root.join(OWNER_FILE))?;
    marker.write_all(&serde_json::to_vec(&lease)?)?;
    fs::create_dir(lease.root.join("workspace"))?;
    Ok(lease)
}
fn history_mode(flag: Option<std::ffi::OsString>, resuming: bool) -> io::Result<bool> {
    match flag {
        None => Ok(false),
        Some(value) if value == "1" && !resuming => Ok(true),
        _ => Err(io::Error::other("history mode requires a fresh owned lease")),
    }
}
fn selected_lease() -> io::Result<Lease> {
    match (std::env::var_os("WORKBENCH_HEADLESS_ROOT"), std::env::var("WORKBENCH_HEADLESS_NONCE")) {
        (None, Err(std::env::VarError::NotPresent)) => new_lease(),
        (Some(root), Ok(nonce)) => {
            let lease = Lease { root: root.into(), nonce };
            validate_lease(&lease)?;
            Ok(lease)
        }
        _ => Err(io::Error::other("partial headless lease")),
    }
}
fn emit(output: &Arc<Mutex<io::Stdout>>, value: Value) -> bool {
    let Ok(mut out) = output.lock() else { return false; };
    // Exactly one escaped JSON object per frame; private text cannot inject lines.
    writeln!(out, "{PREFIX}{value}").and_then(|_| out.flush()).is_ok()
}
/// The headless suite never executes a provider: Codex is reported not
/// configured (so no dispatch arm can exist) and this factory refuses anyway.
struct NoProvider;
impl ProviderFactory for NoProvider {
    fn run(&self, _prompt: &str, _cancel: &Cancel, _started: &mut dyn FnMut()) -> WorkerReport {
        WorkerReport { outcome: WorkerOutcome::Failed { code: "codex_unavailable" }, teardown_proven: true }
    }
}
struct SystemClock;
impl Clock for SystemClock {
    fn now_utc(&self) -> String {
        let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
        now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}
struct Ids;
impl IdSource for Ids { fn next_id(&mut self) -> String { uuid::Uuid::new_v4().to_string() } }
struct WakeThread(thread::Thread);
impl Wake for WakeThread { fn wake(self: Arc<Self>) { self.0.unpark(); } }
fn wait(reply: Reply) -> Result<Value, Error> {
    let mut reply = pin!(reply);
    let waker = Waker::from(Arc::new(WakeThread(thread::current())));
    let mut context = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match reply.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending if Instant::now() < deadline => thread::park_timeout(Duration::from_millis(5)),
            Poll::Pending => return Err(Error { code: "headless_timeout".into(), user_message: "Test bridge reply timed out.".into(), recoverable: true }),
        }
    }
}
fn serve() -> io::Result<()> {
    let seed_history = history_mode(std::env::var_os("WORKBENCH_HEADLESS_SEED_HISTORY"),
        std::env::var_os("WORKBENCH_HEADLESS_ROOT").is_some() || std::env::var_os("WORKBENCH_HEADLESS_NONCE").is_some())?;
    let lease = selected_lease()?;
    let root = validate_lease(&lease)?;
    let output = Arc::new(Mutex::new(io::stdout()));
    let ready = json!({"type":"ready", "lease":lease, "request_limit":MAX_REQUEST});
    if seed_history {
        // Transfer only the fresh lease receipt before seeding so a failed seed
        // can be cleaned by its owner. Commands wait until native bootstrap.
        if !emit(&output, ready.clone()) { return Err(io::Error::other("stdout unavailable")); }
        history::seed(&lease)?;
    }
    let hints = output.clone();
    let workspace = root.join("workspace");
    let coordinator = CoordinatorV2::start(move || {
        let store = V2Store::open(&workspace)?;
        Ok(Box::new(RuntimeV2::new(store, NoProvider, CodexAvailability::NotConfigured, SystemClock, Ids)?) as Box<dyn OwnerRuntime>)
    }, move |payload: ExecutionChanged| {
        emit(&hints, json!({"type":"changed", "payload":payload}))
    });
    let result = (|| {
        if !seed_history && !emit(&output, ready) { return Err(io::Error::other("stdout unavailable")); }
        let mut input = io::BufReader::new(io::stdin());
        loop {
            let mut bytes = Vec::new();
            let count = (&mut input).take(MAX_REQUEST + 1).read_until(b'\n', &mut bytes)?;
            if count == 0 { break; } // Parent EOF is the only shutdown channel.
            if count as u64 > MAX_REQUEST || bytes.last() != Some(&b'\n') {
                return Err(io::Error::other("oversized/incomplete frame"));
            }
            let call: Call = serde_json::from_slice(&bytes)?;
            if call.id == 0 || call.command.len() > 64 { return Err(io::Error::other("invalid call envelope")); }
            // Same production strict decode/admission/runtime/SQLite/output path.
            // No parallel fake engine and no test commands accepted by Coordinator.
            let result = coordinator.submit(&call.command, call.request).and_then(wait);
            let response = match result {
                Ok(value) => json!({"type":"reply", "id":call.id, "ok":value}),
                Err(error) => json!({"type":"reply", "id":call.id, "error":error}),
            };
            if !emit(&output, response) { return Err(io::Error::other("stdout unavailable")); }
        }
        Ok(())
    })();
    if !coordinator.shutdown(Duration::from_secs(10)) { return Err(io::Error::other("headless owner did not stop")); }
    result
}

#[test]
#[ignore = "owned headless runner only; never starts with ordinary cargo test"]
fn serve_headless_bridge() {
    if std::env::var("WORKBENCH_HEADLESS_BRIDGE").as_deref() != Ok("1") { return; }
    // No paths/exception details go into stdout protocol errors.
    if serve().is_err() { panic!("isolated headless bridge failed"); }
}

#[test]
fn history_mode_never_seeds_a_resumed_or_invalid_environment() {
    assert!(!history_mode(None, false).unwrap());
    assert!(history_mode(Some("1".into()), false).unwrap());
    assert!(history_mode(Some("1".into()), true).is_err());
    assert!(history_mode(Some("unknown".into()), false).is_err());
}

#[test]
fn headless_lease_is_fresh_resumable_and_rejects_changed_identity() {
    let lease = new_lease().unwrap();
    assert_eq!(validate_lease(&lease).unwrap(), lease.root);
    let mut changed = lease.clone(); changed.nonce = uuid::Uuid::new_v4().to_string();
    assert!(validate_lease(&changed).is_err());
    // This test created this exact directory; no prefix-based reclamation.
    fs::remove_dir_all(lease.root).unwrap();
}
#[test]
fn headless_lease_never_accepts_general_temp_or_other_named_directory() {
    let root = std::env::temp_dir();
    assert!(validate_lease(&Lease { root, nonce: uuid::Uuid::new_v4().to_string() }).is_err());
    assert!(!canonical_uuid("00000000-0000-1000-8000-000000000001"));
    assert!(!canonical_uuid("NOT-A-UUID"));
}
