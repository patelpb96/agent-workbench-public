//! Broker: single-instance named-pipe server that owns the session registry, auth,
//! capability checks, agent-input grants and the multi-step admin flow. Terminals
//! live in separate `wb-host` processes that survive this process; recovery on start
//! re-adopts them. Slice 3 keeps the registry in a text file; the journal (SQLite)
//! integration comes with the app wiring.
#![cfg(windows)]
use crate::{
    auth,
    caps::{self, AdminAction, AdminError, AdminFlow, CapSet, Capability, Draft, Gate, GrantTable, Origin},
    frames,
    host::{self, host_pipe_name},
    log,
    proto::{self, b64_encode, Admin, ClientKind, Request, Response, SessionInfo, Shell, PROTOCOL_VERSION},
    sync::LockExt,
    win::*,
};
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, HANDLE};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How many client connections are served at once; further ones are refused at once (each costs a thread).
pub const MAX_CONNECTIONS: usize = 128;
const MAX_INPUT_BYTES: usize = 64 << 10;

pub struct BrokerConfig {
    pub data_dir: PathBuf,
    pub pipe_name: String,
    pub host_exe: PathBuf,
    /// Focus-gate phrase for agent grants (empty means "just press Enter").
    pub gate_phrase: String,
}

#[derive(Debug)]
pub enum StartError {
    AlreadyRunning,
    Io(std::io::Error),
}

const HOST_RPC_TIMEOUT_MS: u64 = 5_000;
/// Ending waits up to 5 s for the root and 5 s for the Job inside the host.
const HOST_END_TIMEOUT_MS: u64 = 15_000;
/// A `Watch` answer carries at most about this many bytes of output (the rest is picked up by the next call).
const WATCH_BUDGET: usize = 512 << 10;
/// While a `Watch` waits on sessions without an output event, it asks their hosts this often.
const WATCH_POLL_MS: u64 = 40;
/// How long an unresponsive session is reported without asking its host again.
const UNRESPONSIVE_RETRY_MS: u64 = 5_000;

/// An open connection to one host's pipe; closes the handle when dropped.
struct HostConn {
    h: SendHandle,
}
impl Drop for HostConn {
    fn drop(&mut self) {
        close_handle(self.h.0);
    }
}
/// What became of one call to a host. The difference matters for a request with an effect that must not happen
/// twice (typing a draft): `NotSent` means nothing reached the host; `Unconfirmed` means the request was written but
/// no usable answer came back, so the host may still act on it.
enum RpcOutcome {
    Reply(Vec<u8>),
    NotSent,
    Unconfirmed,
}
impl RpcOutcome {
    fn reply(self) -> Option<Vec<u8>> {
        match self {
            RpcOutcome::Reply(r) => Some(r),
            RpcOutcome::NotSent | RpcOutcome::Unconfirmed => None,
        }
    }
}

impl HostConn {
    fn rpc(&self, op: u8, body: &[u8]) -> RpcOutcome {
        let mut f = Vec::with_capacity(1 + body.len());
        f.push(op);
        f.extend_from_slice(body);
        let mut io = pipe_io(self.h.0);
        if !host::write_request(&mut io, &f) {
            return RpcOutcome::NotSent;
        }
        // A truncated, oversized or closed reply all mean the same here: no usable answer; the caller drops the connection.
        match host::read_reply(&mut io) {
            Ok(reply) => RpcOutcome::Reply(reply),
            Err(_) => RpcOutcome::Unconfirmed,
        }
    }
}

/// Wake-ups for `Watch`: a counter that is bumped (and its waiters notified) whenever some session may have new
/// output. A waiter reads the counter BEFORE it scans the sessions and sleeps only while it is unchanged, so a
/// wake-up that lands during the scan is never lost.
struct Wake {
    count: Mutex<u64>,
    changed: Condvar,
}
impl Wake {
    fn new() -> Arc<Wake> {
        Arc::new(Wake { count: Mutex::new(0), changed: Condvar::new() })
    }
    fn current(&self) -> u64 {
        *self.count.locked()
    }
    fn bump(&self) {
        let mut n = self.count.locked();
        *n = n.wrapping_add(1);
        drop(n);
        self.changed.notify_all();
    }
    fn wait_while_unchanged(&self, seen: u64, timeout: Duration) {
        let guard = self.count.locked();
        // A poisoned wait is not an error here: the caller simply scans again.
        let _ = self.changed.wait_timeout_while(guard, timeout, |n| *n == seen);
    }
}

/// How a session tells the broker it has new output. `signal` is the host's output event (absent on a host that
/// predates it: such a session is polled instead). `dirty` means "read this session once more"; `seen_total` is the
/// furthest output offset this broker has read, so a watcher whose cursor lags behind is served without a new event.
struct WatchState {
    signal: Option<Signal>,
    dirty: AtomicBool,
    seen_total: AtomicU64,
}
impl WatchState {
    /// Opens the host's event and starts the thread that turns it into `dirty` plus a wake-up. The thread ends by
    /// itself once the session is gone (nothing but the thread holds the state any more).
    fn start(id: &str, nonce: &str, wake: &Arc<Wake>) -> Arc<WatchState> {
        let state = Arc::new(WatchState { signal: Signal::open(&output_signal_name(id, nonce)), dirty: AtomicBool::new(true), seen_total: AtomicU64::new(0) });
        if state.signal.is_some() {
            let (state, wake) = (state.clone(), wake.clone());
            let spawned = thread::Builder::new().name("wb-watch".into()).stack_size(64 << 10).spawn(move || {
                while Arc::strong_count(&state) > 1 {
                    if state.signal.as_ref().is_some_and(|s| s.wait(500)) {
                        state.dirty.store(true, Ordering::SeqCst);
                        wake.bump();
                    }
                }
            });
            // No thread: the session is polled like one without an event.
            if spawned.is_err() {
                return Arc::new(WatchState { signal: None, dirty: AtomicBool::new(true), seen_total: AtomicU64::new(0) });
            }
        }
        state
    }
}

struct Session {
    id: String,
    label: String,
    nonce: String,
    host_pid: u32,
    host_ctime: u64,
    history_mib: u32,
    cols: Mutex<(u16, u16)>,
    conn: Mutex<Option<HostConn>>,
    draft: Mutex<Option<Draft>>,
    /// Absolute deadline (ms) of the host call in flight, 0 when none. Read by the watchdog,
    /// which cancels the blocked I/O so a hung host cannot stall the broker.
    deadline: AtomicU64,
    /// The pipe handle the in-flight call is using.
    raw: AtomicUsize,
    unresponsive: AtomicBool,
    last_try_ms: AtomicU64,
    watch: Arc<WatchState>,
    shell: Shell,
    cwd: Option<String>,
}

/// What identifies a session (everything but its live connection).
struct SessionMeta {
    id: String,
    label: String,
    nonce: String,
    host_pid: u32,
    host_ctime: u64,
    cols: u16,
    rows: u16,
    history_mib: u32,
    /// What the terminal was started with, kept so it can be recreated after a restart.
    shell: Shell,
    cwd: Option<String>,
}

impl Session {
    fn new(meta: SessionMeta, conn: HostConn, wake: &Arc<Wake>) -> Session {
        let SessionMeta { id, label, nonce, host_pid, host_ctime, cols, rows, history_mib, shell, cwd } = meta;
        let watch = WatchState::start(&id, &nonce, wake);
        Session {
            id,
            label,
            nonce,
            host_pid,
            host_ctime,
            history_mib,
            cols: Mutex::new((cols, rows)),
            conn: Mutex::new(Some(conn)),
            draft: Mutex::new(None),
            deadline: AtomicU64::new(0),
            raw: AtomicUsize::new(0),
            unresponsive: AtomicBool::new(false),
            last_try_ms: AtomicU64::new(0),
            watch,
            shell,
            cwd,
        }
    }

    /// One host call under a deadline. A failed or timed-out call drops the connection (it may be
    /// mid-frame) and the next call reconnects with a fresh, higher epoch.
    ///
    /// `conn` is a CONNECTION lock, not a data lock: it is held across the pipe call on purpose, to serialise calls
    /// on one pipe. That is bounded by `deadline`, which the broker's watchdog enforces by cancelling the blocked
    /// I/O. No other lock is held while it is taken. If a panic ever poisons it the connection may be mid-frame, so
    /// the repair drops it and the next call reconnects.
    fn call(&self, shared: &Shared, op: u8, body: &[u8], timeout_ms: u64) -> RpcOutcome {
        let mut guard = self.conn.locked_repairing(|conn| *conn = None);
        if guard.is_none() {
            let Some(h) = connect_pipe(&host_pipe_name(&self.id, &self.nonce), Duration::from_millis(300)) else { return RpcOutcome::NotSent };
            let conn = HostConn { h: SendHandle(h) };
            if !host_hello_timeout(conn.h.0, shared.claim_epoch(), &self.nonce) {
                self.unresponsive.store(true, Ordering::SeqCst);
                self.last_try_ms.store(now_ms(), Ordering::SeqCst);
                return RpcOutcome::NotSent;
            }
            *guard = Some(conn);
        }
        let reply = {
            let Some(conn) = guard.as_ref() else { return RpcOutcome::NotSent };
            self.raw.store(conn.h.0 as usize, Ordering::SeqCst);
            self.deadline.store(now_ms() + timeout_ms, Ordering::SeqCst);
            conn.rpc(op, body)
        };
        self.deadline.store(0, Ordering::SeqCst);
        self.last_try_ms.store(now_ms(), Ordering::SeqCst);
        match &reply {
            RpcOutcome::Reply(_) => self.unresponsive.store(false, Ordering::SeqCst),
            RpcOutcome::NotSent | RpcOutcome::Unconfirmed => {
                *guard = None;
                self.unresponsive.store(true, Ordering::SeqCst);
            }
        }
        reply
    }

    /// `call` for requests where only a reply matters.
    fn rpc(&self, shared: &Shared, op: u8, body: &[u8], timeout_ms: u64) -> Option<Vec<u8>> {
        self.call(shared, op, body, timeout_ms).reply()
    }
}

pub struct Shared {
    cfg: BrokerConfig,
    secret: Vec<u8>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// Sessions whose host could not be re-adopted at a restart. Kept (and written to `sessions.txt`) until dismissed
    /// with End, so a loss stays visible across further restarts. Bounded by `MAX_LOST`.
    lost: Mutex<Vec<LostSession>>,
    grants: Mutex<GrantTable>,
    admin: Mutex<AdminFlow>,
    gate: Mutex<Gate>,
    humans: AtomicUsize,
    stop: AtomicBool,
    /// Last epoch issued to any host; strictly increasing, persisted with every claim.
    epoch: AtomicU64,
    /// Sessions already reported as lost/unresponsive in the audit log (each is reported once per daemon run).
    reported: Mutex<std::collections::HashSet<String>>,
    /// Serialises writes of `sessions.txt` (the registry), so two writers cannot leave an older snapshot behind.
    /// It guards no data, so holding it across the file write blocks only other registry writers.
    registry_write: Mutex<()>,
    /// Serialises "change the confirmation phrase" requests for the same reason (the phrase itself is only locked briefly).
    gate_serial: Mutex<()>,
    /// How many poisoned-lock recoveries the audit log already knows about.
    poison_seen: AtomicU64,
    /// Connections that have been sent a challenge and not yet authenticated, each with its deadline. One watchdog
    /// thread sweeps this list (see `run`), instead of a timer thread per connection.
    pending: Mutex<Vec<PendingHandshake>>,
    /// Connections being served right now (capped at `MAX_CONNECTIONS`).
    connections: AtomicUsize,
    /// Set while the connection limit is being hit, so the audit log says so once per episode.
    at_capacity: AtomicBool,
    /// Per-session output logging, keyed by session id. A record outlives its session (the file is the point: it stays
    /// readable, and `LogStatus` still says where it is), so the table is bounded by `MAX_LOGS`.
    logs: Mutex<BTreeMap<String, LogRecord>>,
    /// Creation order for `logs`, so the oldest record is the one dropped when the table is full.
    log_seq: AtomicU64,
    wake: Arc<Wake>,
}

/// A session's log, as the service knows it: where it writes, how far it got and why it stopped.
struct LogRecord {
    path: PathBuf,
    bytes: u64,
    on: bool,
    stopped_reason: Option<String>,
    /// Creation order in the `logs` table (kept when the record is updated).
    seq: u64,
}

/// How many log records are remembered at once (an ended session's record exists for `LogStatus` only).
const MAX_LOGS: usize = 256;

/// How a stop that the writer decided on is worded for a person. A plain request to stop has no reason.
fn stop_reason_text(reason: log::StopReason) -> Option<String> {
    match reason {
        log::StopReason::Stopped => None,
        log::StopReason::Cap => Some("the 64 MiB limit was reached".to_owned()),
        log::StopReason::WriteError => Some("a write to the log file failed".to_owned()),
        log::StopReason::Ended => Some(LOG_ENDED.to_owned()),
    }
}

/// How a log that ended with its terminal is described.
const LOG_ENDED: &str = "the terminal ended";

/// Which record to drop when the table is full (`(seq, on)` of each): the oldest finished one, and only when every
/// record is still recording, the oldest overall (a running log keeps its record as long as possible).
fn log_to_evict(records: impl Iterator<Item = (u64, bool)> + Clone) -> Option<u64> {
    records.clone().filter(|&(_, on)| !on).map(|(seq, _)| seq).min().or_else(|| records.map(|(seq, _)| seq).min())
}

/// The `Logging` answer for a record (`None`: this session never logged, so nothing is on).
fn log_view(rec: Option<&LogRecord>) -> Response {
    match rec {
        Some(r) => Response::Logging { on: r.on, path: Some(r.path.to_string_lossy().into_owned()), bytes: r.bytes, stopped_reason: r.stopped_reason.clone() },
        None => Response::Logging { on: false, path: None, bytes: 0, stopped_reason: None },
    }
}

/// `unix_ms` as `YYYY-MM-DDTHH:MM:SSZ` (UTC), without a date crate.
pub fn iso_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let (y, doy) = (yoe + era * 400, doe - (365 * yoe + yoe / 4 - yoe / 100));
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

const AUDIT_MAX_BYTES: u64 = 256 * 1024;
/// How many lost sessions are remembered at once (the oldest is dropped beyond this).
const MAX_LOST: usize = 64;

/// A session whose host was not there (or would not answer) when the broker started.
struct LostSession {
    info: SessionInfo,
    host_ctime: u64,
    nonce: String,
    shell: Shell,
    cwd: Option<String>,
}

/// One line of `sessions.txt`: id; label; host pid; host start time; nonce; cols; rows; history MiB; `lost` or `-`; shell;
/// start folder (hex of its UTF-8, empty for none). Older brokers wrote the first 7 to 9 fields; they are still read.
struct RegistryLine<'a> {
    id: &'a str,
    label: &'a str,
    host_pid: u32,
    host_ctime: u64,
    nonce: &'a str,
    cols: u16,
    rows: u16,
    history_mib: u32,
    lost: bool,
    shell: Shell,
    cwd: Option<&'a str>,
}
impl RegistryLine<'_> {
    fn text(&self) -> String {
        let cwd = self.cwd.map(|d| auth::hex(d.as_bytes())).unwrap_or_default();
        format!(
            "{};{};{};{};{};{};{};{};{};{};{}\n",
            self.id,
            self.label.replace(';', ","),
            self.host_pid,
            self.host_ctime,
            self.nonce,
            self.cols,
            self.rows,
            self.history_mib,
            if self.lost { "lost" } else { "-" },
            self.shell.name(),
            cwd
        )
    }
}

fn err(code: &str, message: &str) -> Response {
    Response::Error { code: code.into(), message: message.into() }
}

/// Strictly increasing across broker restarts even if the wall clock steps back: the
/// last epoch is persisted and the new one is at least one more than it.
fn next_epoch(dir: &std::path::Path) -> u64 {
    let last = read_epoch(dir);
    let epoch = now_ms().max(last.saturating_add(1));
    write_epoch(dir, epoch);
    epoch
}

/// The fencing number is kept in two files (`epoch.txt` and `epoch.bak`), each written atomically (temp file and
/// rename), and read back as the LARGER of the two. A torn write, an emptied or deleted file, or a damaged copy can
/// then no longer lower it below what the running hosts have already seen (review finding on epoch.txt).
fn read_epoch(dir: &std::path::Path) -> u64 {
    ["epoch.txt", "epoch.bak"].iter().filter_map(|name| std::fs::read_to_string(dir.join(name)).ok()).filter_map(|s| s.trim().parse::<u64>().ok()).max().unwrap_or(0)
}
fn write_epoch(dir: &std::path::Path, epoch: u64) {
    for name in ["epoch.txt", "epoch.bak"] {
        let (tmp, path) = (dir.join(format!("{name}.tmp")), dir.join(name));
        if std::fs::write(&tmp, epoch.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn host_hello(conn: &HostConn, epoch: u64, nonce: &str) -> bool {
    let mut b = epoch.to_le_bytes().to_vec();
    b.extend_from_slice(nonce.as_bytes());
    matches!(conn.rpc(host::OP_HELLO, &b).reply(), Some(r) if r == b"OK")
}

/// Handshake with a deadline, so a suspended or wedged host cannot hang the caller.
/// The caller keeps ownership of the handle.
fn host_hello_timeout(h: HANDLE, epoch: u64, nonce: &str) -> bool {
    let done = Arc::new(AtomicBool::new(false));
    let (d2, raw) = (done.clone(), h as usize);
    let timer = thread::spawn(move || {
        let end = Instant::now() + Duration::from_millis(HOST_RPC_TIMEOUT_MS);
        while !d2.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= end {
                cancel_io(raw);
                return;
            }
            thread::park_timeout(end - now);
        }
    });
    let conn = HostConn { h: SendHandle(h) };
    let ok = host_hello(&conn, epoch, nonce);
    done.store(true, Ordering::SeqCst);
    timer.thread().unpark();
    // The timer has finished before this returns, so it can never act on the handle after the caller closes it.
    let _ = timer.join();
    std::mem::forget(conn);
    ok
}

/// What to start a session host with.
#[derive(Clone, Copy)]
struct HostSpec<'a> {
    id: &'a str,
    nonce: &'a str,
    cols: u16,
    rows: u16,
    ring_mib: u32,
    shell: Shell,
    cwd: Option<&'a str>,
}

/// A request to create a terminal.
struct NewTerminal<'a> {
    /// Recreate under this id (a restore); `None` picks a fresh one.
    id: Option<String>,
    cols: u16,
    rows: u16,
    label: Option<String>,
    shell: Shell,
    extended: bool,
    cwd: Option<String>,
    via: &'a str,
}

fn clean_label(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).take(64).collect()
}

impl Shared {
    // Lock helpers. Each says how its data is restored if a panic elsewhere poisoned the lock (see `sync`).

    /// The session table. A panic cannot leave half an entry, but a repair still drops any entry filed under the wrong id.
    fn sessions_guard(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Session>>> {
        self.sessions.locked_repairing(|map| map.retain(|id, s| *id == s.id))
    }
    /// Agent grants. After a recovery every grant is revoked (fail closed): a half-applied grant must not survive.
    fn grants_guard(&self) -> MutexGuard<'_, GrantTable> {
        self.grants.locked_repairing(GrantTable::revoke_all)
    }
    /// A pending admin step. After a recovery it is cancelled (fail closed): the human starts again.
    fn admin_guard(&self) -> MutexGuard<'_, AdminFlow> {
        self.admin.locked_repairing(AdminFlow::cancel)
    }

    /// Cut off every connection whose handshake deadline has passed. Entries of connections that authenticated or
    /// already closed are dropped. The cancel is made under the connection's gate, so it can never touch a handle
    /// that has been closed (and possibly reused for something else) in the meantime.
    fn expire_handshakes(&self) {
        let now = Instant::now();
        self.pending.locked().retain(|p| {
            if p.authed.load(Ordering::SeqCst) || !p.gate.is_open() {
                return false;
            }
            if now >= p.deadline {
                p.gate.cancel_io(p.raw);
                return false;
            }
            true
        });
    }

    /// Write any poisoned-lock recoveries since the last call to the audit log (counts only).
    fn report_poison(&self) {
        let now = crate::sync::poison_recoveries();
        let seen = self.poison_seen.swap(now, Ordering::SeqCst);
        if now > seen {
            self.audit(&format!("recovered {} poisoned lock(s) after a panic", now - seen));
        }
    }

    fn claim_epoch(&self) -> u64 {
        let e = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        write_epoch(&self.cfg.data_dir, e);
        e
    }

    fn registry_path(&self) -> PathBuf {
        self.cfg.data_dir.join("sessions.txt")
    }

    fn save_registry(&self) {
        // One writer at a time (this lock guards no data). The text is built under the table lock, which is released
        // before any file is touched, so a slow disk never holds up a request that only needs the table.
        let _writer = self.registry_write.locked();
        let body: String = {
            let sessions = self.sessions_guard();
            sessions
                .values()
                .map(|s| {
                    let (c, r) = *s.cols.locked();
                    RegistryLine { id: &s.id, label: &s.label, host_pid: s.host_pid, host_ctime: s.host_ctime, nonce: &s.nonce, cols: c, rows: r, history_mib: s.history_mib, lost: false, shell: s.shell, cwd: s.cwd.as_deref() }.text()
                })
                .collect()
        };
        // Lost sessions keep their line, marked `lost`, with what is needed to recreate them after a restart.
        let lost: String = self
            .lost
            .locked()
            .iter()
            .map(|l| RegistryLine { id: &l.info.id, label: &l.info.label, host_pid: l.info.host_pid, host_ctime: l.host_ctime, nonce: &l.nonce, cols: l.info.cols, rows: l.info.rows, history_mib: l.info.history_mib, lost: true, shell: l.shell, cwd: l.cwd.as_deref() }.text())
            .collect();
        let body = body + &lost;
        let tmp = self.registry_path().with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, self.registry_path());
        }
    }

    /// Once per open, before any client is served: re-adopt live hosts, mark the rest lost.
    fn recover(&self) {
        let Ok(text) = std::fs::read_to_string(self.registry_path()) else { return };
        for line in text.lines() {
            let p: Vec<&str> = line.split(';').collect();
            // Older registries have 7 fields (no history size) or 8 (live) or 9 (`lost`); the current format has 11: the ninth is
            // `lost` or `-`, then the shell and the start folder (hex, empty for none).
            if !(7..=11).contains(&p.len()) || (p.len() >= 9 && p[8] != "lost" && p[8] != "-") {
                continue;
            }
            let shell = p.get(9).and_then(|v| Shell::from_name(v)).unwrap_or(Shell::Cmd);
            let cwd = p.get(10).filter(|v| !v.is_empty()).and_then(|v| auth::unhex(v)).and_then(|b| String::from_utf8(b).ok()).filter(|d| proto::valid_start_dir(d));
            let (id, label, nonce) = (p[0].to_owned(), p[1].to_owned(), p[4].to_owned());
            let (Ok(pid), Ok(ctime), Ok(cols), Ok(rows)) = (p[2].parse::<u32>(), p[3].parse::<u64>(), p[5].parse::<u16>(), p[6].parse::<u16>()) else { continue };
            let history_mib: u32 = p.get(7).and_then(|v| v.parse().ok()).unwrap_or(1);
            // A loss recorded earlier is kept as it is, until the user dismisses it. If its host is in fact running again
            // (same pid and start time) it is tried like any other, so a transient failure can heal.
            let adopted = alive(pid, ctime)
                .then(|| connect_pipe(&host_pipe_name(&id, &nonce), Duration::from_millis(500)))
                .flatten()
                .and_then(|h| {
                    if host_hello_timeout(h, self.claim_epoch(), &nonce) {
                        Some(HostConn { h: SendHandle(h) })
                    } else {
                        close_handle(h);
                        None
                    }
                });
            match adopted {
                Some(conn) => {
                    let s = Arc::new(Session::new(SessionMeta { id: id.clone(), label, nonce, host_pid: pid, host_ctime: ctime, cols, rows, history_mib, shell, cwd }, conn, &self.wake));
                    self.sessions_guard().insert(id, s.clone());
                    self.adopt_log(&s);
                }
                None => {
                    let mut lost = self.lost.locked();
                    lost.push(LostSession { info: SessionInfo { id, label, state: "lost".into(), persistent: true, cols, rows, host_pid: pid, history_mib }, host_ctime: ctime, nonce, shell, cwd });
                    if lost.len() > MAX_LOST {
                        lost.remove(0);
                    }
                }
            }
        }
        self.save_registry();
    }

    fn info(&self, s: &Session) -> SessionInfo {
        let (cols, rows) = *s.cols.locked();
        // A session already known to be unresponsive is reported without asking its host again
        // until the retry interval passes, so one hung host cannot slow every listing.
        let recently_failed = s.unresponsive.load(Ordering::SeqCst) && now_ms().saturating_sub(s.last_try_ms.load(Ordering::SeqCst)) < UNRESPONSIVE_RETRY_MS;
        let state = if recently_failed {
            "unresponsive"
        } else {
            match s.rpc(self, host::OP_INFO, &[], HOST_RPC_TIMEOUT_MS) {
                Some(b) if String::from_utf8_lossy(&b).contains("exited=1") => "exited",
                Some(_) => "running",
                None if alive(s.host_pid, s.host_ctime) => "unresponsive",
                None => "lost",
            }
        };
        // Each loss is written to the audit log once, so "where did my terminal go" has an answer.
        if matches!(state, "lost" | "exited" | "unresponsive") && self.reported.locked().insert(format!("{}:{state}", s.id)) {
            self.audit(&format!("{state} {} host={}", s.id, s.host_pid));
        }
        SessionInfo { id: s.id.clone(), label: s.label.clone(), state: state.into(), persistent: true, cols, rows, host_pid: s.host_pid, history_mib: s.history_mib }
    }

    /// Append one line to `events.log` in the data folder: when terminals were created, ended or lost and by
    /// whom (gui, cli or agent:NAME). Ids and counts only: never labels, text or paths. Bounded (rotated at 256 KiB).
    fn audit(&self, line: &str) {
        use std::io::Write;
        let path = self.cfg.data_dir.join("events.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > AUDIT_MAX_BYTES) {
            let _ = std::fs::rename(&path, self.cfg.data_dir.join("events.log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{} {line}", iso_utc(now_ms()));
        }
    }

    fn spawn_host(&self, spec: &HostSpec) -> Result<(u32, u64), String> {
        let HostSpec { id, nonce, cols, rows, ring_mib, shell, cwd } = *spec;
        // The start folder travels as one hex token so spaces and quotes in it cannot be misread as the shell command line.
        let cwd_token = cwd.map(|d| format!("cwd:{} ", auth::hex(d.as_bytes()))).unwrap_or_default();
        spawn_host_process(&format!("\"{}\" {id} {nonce} {cols} {rows} {ring_mib} {cwd_token}{}", self.cfg.host_exe.display(), shell.command_line()))
    }

    fn new_terminal(&self, request: NewTerminal<'_>) -> Response {
        let NewTerminal { id: wanted, cols, rows, label, shell, extended, cwd, via } = request;
        if let Some(dir) = &cwd {
            if !proto::valid_start_dir(dir) {
                return err("invalid_folder", "The start folder must be an absolute path such as C:\\Users\\you\\project.");
            }
            if !std::path::Path::new(dir).is_dir() {
                return err("invalid_folder", "The start folder does not exist.");
            }
        }
        let history_mib: u32 = if extended { 32 } else { 1 };
        if !(20..=400).contains(&cols) || !(5..=200).contains(&rows) {
            return err("invalid_size", "Terminal size is out of range.");
        }
        let mut idb = [0u8; 4];
        random_bytes(&mut idb);
        let mut nb = [0u8; 8];
        random_bytes(&mut nb);
        // A restore keeps the old id (so the layout, the floating windows and the agent links that name it stay valid);
        // it is never one that is in use.
        let id = match wanted {
            Some(id) if self.session(&id).is_none() => id,
            Some(_) => return err("session_exists", "A terminal with that id is running."),
            None => auth::hex(&idb),
        };
        let nonce = auth::hex(&nb);
        let (pid, ctime) = match self.spawn_host(&HostSpec { id: &id, nonce: &nonce, cols, rows, ring_mib: history_mib, shell, cwd: cwd.as_deref() }) {
            Ok(x) => x,
            Err(m) => return err("host_launch_failed", &m),
        };
        let Some(h) = connect_pipe(&host_pipe_name(&id, &nonce), Duration::from_secs(5)) else {
            kill_pid(pid);
            return err("host_unreachable", "The session host did not start.");
        };
        if !host_hello_timeout(h, self.claim_epoch(), &nonce) {
            close_handle(h);
            kill_pid(pid);
            return err("host_handshake_failed", "The session host refused the handshake.");
        }
        let conn = HostConn { h: SendHandle(h) };
        let label = clean_label(&label.unwrap_or_else(|| format!("Terminal {id}")));
        let session = Arc::new(Session::new(SessionMeta { id: id.clone(), label, nonce, host_pid: pid, host_ctime: ctime, cols, rows, history_mib, shell, cwd: cwd.clone() }, conn, &self.wake));
        // In the table and the registry BEFORE the first call to the host: the deadline watchdog only looks at the table,
        // so a host that stalls right after its handshake is cancelled (not waited on forever), and a broker that dies now
        // still knows about the host (review finding F9).
        self.sessions_guard().insert(id.clone(), session.clone());
        self.save_registry();
        let info = self.info(&session);
        self.audit(&format!("created {id} host={pid} by={via}"));
        Response::Created { session: info }
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions_guard().get(id).cloned()
    }

    // ---- opt-in output logging -------------------------------------------------

    /// One host call about logging. `None` unless the host answered with a usable LOG reply: a host that did not
    /// answer, or answered with something else, never changes what the service thinks it knows.
    fn log_call(&self, s: &Session, op: u8, body: &[u8]) -> Option<host::LogReply> {
        s.rpc(self, op, body, HOST_RPC_TIMEOUT_MS).as_deref().and_then(host::parse_log_reply)
    }

    /// Record what a host reported for a session's log (the table stays bounded; the newest records win).
    fn remember_log(&self, id: &str, path: PathBuf, r: &host::LogReply) {
        let mut logs = self.logs.locked();
        if !logs.contains_key(id) && logs.len() >= MAX_LOGS {
            if let Some(oldest) = log_to_evict(logs.values().map(|v| (v.seq, v.on))) {
                logs.retain(|_, v| v.seq != oldest);
            }
        }
        let seq = logs.get(id).map_or_else(|| self.log_seq.fetch_add(1, Ordering::SeqCst) + 1, |v| v.seq);
        logs.insert(id.to_owned(), LogRecord { path, bytes: r.bytes, on: r.on, stopped_reason: r.reason.and_then(stop_reason_text), seq });
    }

    /// Mark a log's record as ended. The file stays on disk (that is what a log is for); only the state changes.
    fn finish_log(&self, id: &str, reason: &str) {
        if let Some(rec) = self.logs.locked().get_mut(id).filter(|r| r.on) {
            rec.on = false;
            rec.stopped_reason = Some(reason.to_owned());
        }
    }

    /// Learn what an adopted host is logging (once, when a restarted broker re-adopts it), so stop, end and status
    /// know about a log that a previous broker started. A host that predates logging answers nothing usable.
    fn adopt_log(&self, s: &Session) {
        if let Some(r) = self.log_call(s, host::OP_LOG_STATUS, &[]) {
            if let Some(path) = r.path.clone().filter(|p| !p.is_empty()) {
                self.remember_log(&s.id, PathBuf::from(path), &r);
            }
        }
    }

    /// A host that did not answer a log call: if its process is gone the terminal has ended, so an open record is
    /// closed (nothing can be written any more). True when that is the case.
    fn host_gone(&self, s: &Session) -> bool {
        let gone = !alive(s.host_pid, s.host_ctime);
        if gone {
            self.finish_log(&s.id, LOG_ENDED);
        }
        gone
    }

    /// `LogStart`: begin recording this terminal's output. The file is chosen here, under `<data dir>/logs`, and never
    /// by the client. Starting while a log is being written answers with that file.
    fn start_log(&self, id: &str, via: &str) -> Response {
        let Some(s) = self.session(id) else { return err("no_such_session", "Unknown session.") };
        if let Err(e) = ensure_private_dir(&log::dir(&self.cfg.data_dir)) {
            return err("log_unavailable", &format!("The logs folder could not be created ({e})."));
        }
        // The host is asked first, never the cached record: a log the host stopped by itself (the cap, a write error)
        // must let a new one start, and a log a previous broker started is adopted rather than doubled.
        if let Some(r) = self.log_call(&s, host::OP_LOG_STATUS, &[]) {
            if let (true, Some(path)) = (r.on, r.path.clone().filter(|p| !p.is_empty())) {
                let known = self.logs.locked().get(id).is_some_and(|rec| rec.on);
                self.remember_log(id, PathBuf::from(&path), &r);
                if !known {
                    self.audit(&format!("log start {id} by={via} adopted"));
                }
                return log_view(self.logs.locked().get(id));
            }
            if r.reason == Some(log::StopReason::Ended) {
                return err("terminal_ended", "The terminal has ended; there is nothing more to record.");
            }
        } else if self.host_gone(&s) {
            return err("terminal_ended", "The terminal has ended; there is nothing more to record.");
        }
        let path = log::path(&self.cfg.data_dir, id, now_ms());
        let reply = s.rpc(self, host::OP_LOG_START, path.to_string_lossy().as_bytes(), HOST_RPC_TIMEOUT_MS);
        if reply.as_deref() == Some(b"ERR terminal_ended".as_slice()) {
            return err("terminal_ended", "The terminal has ended; there is nothing more to record.");
        }
        let Some(r) = reply.as_deref().and_then(host::parse_log_reply) else {
            return err("host_error", "The session host did not accept a log; nothing was recorded.");
        };
        if !r.on {
            // The host refused to create the file (for example a protected logs folder).
            return err("log_unavailable", "The log file could not be created; nothing was recorded.");
        }
        self.remember_log(id, path, &r);
        self.audit(&format!("log start {id} by={via}"));
        log_view(self.logs.locked().get(id))
    }

    /// `LogStop`: stop recording. Stopping when nothing is being recorded is fine (the answer says what is on).
    fn stop_log(&self, id: &str, via: &str) -> Response {
        let Some((on, path)) = self.logs.locked().get(id).map(|r| (r.on, r.path.clone())) else {
            return if self.session(id).is_some() { log_view(None) } else { err("no_such_session", "Unknown session.") };
        };
        if !on {
            return log_view(self.logs.locked().get(id));
        }
        match self.session(id) {
            Some(s) => match self.log_call(&s, host::OP_LOG_STOP, &[]) {
                Some(r) => {
                    self.remember_log(id, path, &r);
                    self.audit(&format!("log stop {id} by={via}"));
                    log_view(self.logs.locked().get(id))
                }
                // A dead host writes nothing more: the record is closed. A live one that did not answer may still be.
                None if self.host_gone(&s) => {
                    self.audit(&format!("log stop {id} by={via}"));
                    log_view(self.logs.locked().get(id))
                }
                None => err("host_error", "The session host did not confirm the stop; it may still be recording."),
            },
            // The terminal is already gone: nothing is being written any more, so the record is simply closed.
            None => {
                self.finish_log(id, LOG_ENDED);
                self.audit(&format!("log stop {id} by={via}"));
                log_view(self.logs.locked().get(id))
            }
        }
    }

    /// `LogStatus`: where the log is and how far it got, also for a terminal that has ended.
    fn status_log(&self, id: &str) -> Response {
        let Some(s) = self.session(id) else {
            let known = self.logs.locked().contains_key(id);
            return if known { log_view(self.logs.locked().get(id)) } else { err("no_such_session", "Unknown session.") };
        };
        // Ask the host, which owns the count (nothing else can know it exactly). A host that does not answer leaves the
        // last known state in place rather than failing the query.
        match self.log_call(&s, host::OP_LOG_STATUS, &[]) {
            Some(r) => {
                let path = r.path.clone().filter(|p| !p.is_empty()).map(PathBuf::from).or_else(|| self.logs.locked().get(id).map(|v| v.path.clone()));
                match path {
                    Some(path) => {
                        self.remember_log(id, path, &r);
                        log_view(self.logs.locked().get(id))
                    }
                    // The host has no log for it: this session never logged.
                    None => log_view(None),
                }
            }
            None => {
                self.host_gone(&s);
                let known = self.logs.locked().contains_key(id);
                if known {
                    log_view(self.logs.locked().get(id))
                } else {
                    err("host_error", "The session host did not answer.")
                }
            }
        }
    }

    /// Stop a session's logging because the session is ending, before the host is told to end: the count is then the
    /// whole log, and the record keeps the file and its size for `LogStatus` afterwards.
    fn end_log(&self, s: &Session) {
        if !self.logs.locked().get(&s.id).is_some_and(|r| r.on) {
            return;
        }
        let final_bytes = self.log_call(s, host::OP_LOG_STOP, &[]).map(|r| r.bytes);
        {
            let mut logs = self.logs.locked();
            let target = logs.get_mut(&s.id);
            if let Some(rec) = target {
                rec.bytes = final_bytes.unwrap_or(rec.bytes);
            }
        }
        self.finish_log(&s.id, LOG_ENDED);
        self.audit(&format!("log stop {} by=end", s.id));
    }

    /// `Watch`: wait until some cursor's session has output beyond it, then answer with that output. Sessions whose
    /// host signals output are read only when signalled (or when a cursor lags); sessions without a signal (hosts
    /// started before it existed) are read every `WATCH_POLL_MS` while the call waits.
    fn watch(&self, cursors: &[proto::WatchCursor], wait_ms: u64) -> Response {
        if cursors.is_empty() || cursors.len() > proto::MAX_WATCH_CURSORS {
            return err("invalid_watch", "A watch needs between 1 and 64 cursors.");
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(proto::MAX_WATCH_WAIT_MS));
        loop {
            let seen = self.wake.current();
            let (mut items, mut gone, mut polling, mut budget) = (Vec::new(), Vec::new(), false, WATCH_BUDGET);
            // One take of each session's `dirty` per pass, so two cursors on one session both see it.
            let mut taken: BTreeMap<&str, bool> = BTreeMap::new();
            for c in cursors {
                let Some(s) = self.session(&c.session) else {
                    gone.push(c.session.clone());
                    continue;
                };
                if budget == 0 {
                    break;
                }
                let ready = if s.watch.signal.is_some() {
                    let dirty = *taken.entry(c.session.as_str()).or_insert_with(|| s.watch.dirty.swap(false, Ordering::SeqCst));
                    dirty || c.from < s.watch.seen_total.load(Ordering::SeqCst)
                } else {
                    polling = true;
                    true
                };
                // A session already known to be unresponsive is not asked again straight away.
                let recently_failed = s.unresponsive.load(Ordering::SeqCst) && now_ms().saturating_sub(s.last_try_ms.load(Ordering::SeqCst)) < UNRESPONSIVE_RETRY_MS;
                if !ready || recently_failed {
                    continue;
                }
                let reply = s.rpc(self, host::OP_READ, &c.from.to_le_bytes(), HOST_RPC_TIMEOUT_MS);
                let Some((next, skipped, data)) = reply.as_deref().and_then(frames::parse_read_reply) else { continue };
                s.watch.seen_total.fetch_max(next, Ordering::SeqCst);
                if data.len() >= frames::READ_CHUNK {
                    s.watch.dirty.store(true, Ordering::SeqCst); // a full chunk: there is probably more
                }
                if !data.is_empty() || skipped > 0 {
                    budget = budget.saturating_sub(data.len().max(1));
                    items.push(proto::WatchItem { session: c.session.clone(), from: c.from, data: b64_encode(data), next, skipped });
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !items.is_empty() || !gone.is_empty() || remaining.is_zero() || self.stop.load(Ordering::SeqCst) {
                return Response::Watch { items, gone };
            }
            let nap = if polling { remaining.min(Duration::from_millis(WATCH_POLL_MS)) } else { remaining };
            self.wake.wait_while_unchanged(seen, nap);
        }
    }

    /// Ending a lost session dismisses it. Its host should be gone; if it is in fact still running (it could not be
    /// re-adopted) it is stopped, never just forgotten, and if it cannot be stopped the entry stays.
    fn dismiss_lost(&self, id: &str, via: &str) -> Response {
        let removed = {
            let mut lost = self.lost.locked();
            lost.iter().position(|l| l.info.id == id).map(|i| lost.remove(i))
        };
        let Some(entry) = removed else { return err("no_such_session", "Unknown session.") };
        if alive(entry.info.host_pid, entry.host_ctime) {
            kill_pid(entry.info.host_pid);
            if alive(entry.info.host_pid, entry.host_ctime) {
                self.lost.locked().push(entry);
                return err("teardown_unproven", "The session host is still running and could not be stopped; it is still listed so you can try again.");
            }
        }
        self.save_registry();
        self.audit(&format!("dismissed {id} by={via}"));
        Response::Ok
    }

    /// Recreate a terminal that did not survive a restart, under its old id and with what it was started with. Only what the
    /// terminal was (label, size, shell, start folder, history size) comes back, never what was running in it. A start folder
    /// that has gone since falls back to the user's folder.
    fn restore_session(&self, id: &str, shell: Option<Shell>, via: &str) -> Response {
        let taken = {
            let mut lost = self.lost.locked();
            lost.iter().position(|l| l.info.id == id).map(|i| lost.remove(i))
        };
        let Some(entry) = taken else { return err("no_such_session", "No lost terminal has that id.") };
        if alive(entry.info.host_pid, entry.host_ctime) {
            self.lost.locked().push(entry);
            return err("still_running", "That terminal's host is still running; end it first.");
        }
        let cwd = entry.cwd.clone().filter(|d| std::path::Path::new(d).is_dir());
        let outcome = self.new_terminal(NewTerminal {
            id: Some(id.to_owned()),
            cols: entry.info.cols,
            rows: entry.info.rows,
            label: Some(entry.info.label.clone()),
            shell: shell.unwrap_or(entry.shell),
            extended: entry.info.history_mib >= 32,
            cwd,
            via,
        });
        match outcome {
            Response::Created { .. } => {
                self.audit(&format!("restored {id} by={via}"));
                outcome
            }
            other => {
                // Nothing was created: the entry stays, so a later attempt (or a dismissal) is still possible.
                self.lost.locked().push(entry);
                self.save_registry();
                other
            }
        }
    }

    fn end_session(&self, id: &str, via: &str) -> Response {
        let Some(s) = self.session(id) else { return self.dismiss_lost(id, via) };
        // Recording stops with the terminal; the record keeps the file and its size for `LogStatus` afterwards.
        self.end_log(&s);
        let reply = s.rpc(self, host::OP_END, &[], HOST_END_TIMEOUT_MS);
        let proved = matches!(&reply, Some(r) if r.starts_with(b"ENDED active=0 root_signaled=1"));
        // No usable answer: the host is either already gone (nothing left to end) or wedged. A wedged host is stopped by
        // force (its Job then takes the shell tree with it) instead of being forgotten while it keeps running.
        let how = match (&reply, proved) {
            (_, true) => "proof",
            (Some(_), false) => "unproven",
            (None, _) if !alive(s.host_pid, s.host_ctime) => "already_gone",
            (None, _) => {
                kill_pid(s.host_pid);
                "forced"
            }
        };
        if reply.is_none() && alive(s.host_pid, s.host_ctime) {
            // Still running: keep it listed so the user can retry, and say so.
            self.audit(&format!("end {id} by={via} failed: host {} could not be stopped", s.host_pid));
            return err("teardown_unproven", "The session host did not answer and could not be stopped; it is still listed so you can try again.");
        }
        self.sessions_guard().remove(id);
        self.grants_guard().revoke_session(id);
        self.save_registry();
        self.audit(&format!("ended {id} by={via} proof={proved} how={how}"));
        match how {
            "proof" | "already_gone" | "forced" => Response::Ok,
            _ => err("teardown_unproven", "The session ended without proof of a clean teardown."),
        }
    }

    // ---- in-process agent API (agent backends run inside the broker) -------

    /// Agent-origin input: allowed only with a live grant, and the grant lapses while
    /// no human client is attached unless it was created with keep-after-detach.
    pub fn agent_input(&self, agent: &str, session: &str, text: &str) -> Result<(), &'static str> {
        let humans = self.humans.load(Ordering::SeqCst) > 0;
        let allowed = caps::input_allowed(CapSet::USER, &Origin::Agent(agent.to_owned()), &self.grants_guard(), session, now_ms(), humans);
        if !allowed {
            return Err("no_grant");
        }
        if text.len() > MAX_INPUT_BYTES {
            return Err("too_long");
        }
        let s = self.session(session).ok_or("no_such_session")?;
        let ok = matches!(s.rpc(self, host::OP_INPUT, text.as_bytes(), HOST_RPC_TIMEOUT_MS), Some(r) if r == b"OK");
        ok.then_some(()).ok_or("host_error")
    }

    /// Without a grant an agent can only stage a draft; a human accepts it.
    pub fn agent_stage(&self, agent: &str, session: &str, text: &str) -> Result<(), &'static str> {
        let draft = caps::stage_draft(agent, text).map_err(|_| "invalid_draft")?;
        let s = self.session(session).ok_or("no_such_session")?;
        *s.draft.locked() = Some(draft);
        Ok(())
    }

    pub fn session_count(&self) -> usize {
        self.sessions_guard().len()
    }
    pub fn lost_sessions(&self) -> Vec<SessionInfo> {
        self.lost.locked().iter().map(|l| l.info.clone()).collect()
    }
    pub fn human_clients(&self) -> usize {
        self.humans.load(Ordering::SeqCst)
    }
    pub fn grants_len(&self) -> usize {
        self.grants_guard().len()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        if let Some(h) = connect_pipe(&self.cfg.pipe_name, Duration::from_millis(500)) {
            close_handle(h);
        }
    }

    // ---- request dispatch --------------------------------------------------

    fn handle(&self, caps_: CapSet, origin: &Origin, via: &str, req: Request) -> Response {
        let need = |c: Capability| caps_.has(c);
        // Agent connections may read a session's output only while they hold a live grant for it.
        let agent_may_read = |session: &str| match origin {
            Origin::Human => true,
            Origin::Agent(a) => self.grants_guard().permits(a, session, now_ms(), self.humans.load(Ordering::SeqCst) > 0),
        };
        match req {
            Request::Hello { .. } => err("already_authenticated", "Already authenticated."),
            Request::List {} if need(Capability::Observe) => {
                let sessions: Vec<Arc<Session>> = self.sessions_guard().values().cloned().collect();
                let mut items: Vec<SessionInfo> = sessions.iter().map(|s| self.info(s)).collect();
                items.extend(self.lost.locked().iter().map(|l| l.info.clone()));
                Response::Sessions { items }
            }
            Request::NewTerminal { cols, rows, label, shell, extended_history, cwd } if need(Capability::Spawn) => self.new_terminal(NewTerminal { id: None, cols, rows, label, shell: shell.unwrap_or(Shell::Cmd), extended: extended_history.unwrap_or(false), cwd, via }),
            Request::Restore { session, shell } if need(Capability::Spawn) => self.restore_session(&session, shell, via),
            Request::Read { session, from } if need(Capability::Observe) => {
                if !agent_may_read(&session) {
                    return err("no_grant", "This agent has no live grant for that session.");
                }
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                let reply = s.rpc(self, host::OP_READ, &from.to_le_bytes(), HOST_RPC_TIMEOUT_MS);
                match reply.as_deref().and_then(frames::parse_read_reply) {
                    Some((next, skipped, data)) => Response::Output { data: b64_encode(data), next, skipped },
                    None => err("host_error", "The session host did not answer."),
                }
            }
            // Human connections only (agents lack the input capability): output for many sessions, pushed as it appears.
            Request::Watch { cursors, wait_ms } if need(Capability::Input) && matches!(origin, Origin::Human) => self.watch(&cursors, wait_ms),
            // Origin is always Human on a client connection; agents cannot arrive here.
            Request::Input { session, text } if need(Capability::Input) => {
                if text.len() > MAX_INPUT_BYTES {
                    return err("too_long", "Input is too large.");
                }
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                let reply = s.rpc(self, host::OP_INPUT, text.as_bytes(), HOST_RPC_TIMEOUT_MS);
                match reply {
                    Some(r) if r == b"OK" => Response::Ok,
                    _ => err("host_error", "The session host did not accept the input."),
                }
            }
            Request::Resize { session, cols, rows } if need(Capability::Input) => {
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                if !(20..=400).contains(&cols) || !(5..=200).contains(&rows) {
                    return err("invalid_size", "Terminal size is out of range.");
                }
                // Both are within 20..=400 / 5..=200 here, so they always fit; a failure means a logic error, not input.
                let (Ok(cols_i16), Ok(rows_i16)) = (i16::try_from(cols), i16::try_from(rows)) else { return err("invalid_size", "Terminal size is out of range.") };
                let mut body = cols_i16.to_le_bytes().to_vec();
                body.extend_from_slice(&rows_i16.to_le_bytes());
                let reply = s.rpc(self, host::OP_RESIZE, &body, HOST_RPC_TIMEOUT_MS);
                match reply {
                    Some(r) if r == b"OK" => {
                        *s.cols.locked() = (cols, rows);
                        // Recorded, so a restart reports the size the terminal actually has.
                        self.save_registry();
                        Response::Ok
                    }
                    _ => err("host_error", "Resize failed."),
                }
            }
            Request::End { session } if need(Capability::Spawn) => self.end_session(&session, via),
            // Human connections only: recording a terminal's output to a file is a person's decision, and a grant does
            // not make an agent able to make it (an agent lacks the input capability these three need).
            Request::LogStart { session } if need(Capability::Input) && matches!(origin, Origin::Human) => self.start_log(&session, via),
            Request::LogStop { session } if need(Capability::Input) && matches!(origin, Origin::Human) => self.stop_log(&session, via),
            Request::LogStatus { session } if need(Capability::Input) && matches!(origin, Origin::Human) => self.status_log(&session),
            Request::Propose { session, text } if need(Capability::Propose) => match origin {
                Origin::Agent(a) => match self.agent_stage(a, &session, &text) {
                    Ok(()) => Response::Ok,
                    Err("no_such_session") => err("no_such_session", "Unknown session."),
                    Err(_) => err("invalid_draft", "That text cannot be staged (control characters or too long)."),
                },
                Origin::Human => err("forbidden", "Only agent connections propose drafts."),
            },
            Request::AgentInput { session, text } if need(Capability::Propose) => match origin {
                Origin::Agent(a) => match self.agent_input(a, &session, &text) {
                    Ok(()) => Response::Ok,
                    Err("no_grant") => err("no_grant", "This agent has no live grant for that session."),
                    Err("no_such_session") => err("no_such_session", "Unknown session."),
                    Err(other) => err(other, "The input was not delivered."),
                },
                Origin::Human => err("forbidden", "Only agent connections use agent input."),
            },
            Request::GetDraft { session } if need(Capability::Input) => {
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                let draft = s.draft.locked().clone().map(|d| proto::DraftView { agent: d.agent, text: d.text, multiline: d.multiline });
                Response::Draft { draft }
            }
            Request::DiscardDraft { session } if need(Capability::Input) => {
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                *s.draft.locked() = None;
                Response::Ok
            }
            Request::Grants { session } if need(Capability::Input) => {
                let items = self.grants_guard().list_session(&session, now_ms()).into_iter().map(|(agent, remaining_ms, keep_after_detach)| proto::GrantView { agent, remaining_ms, keep_after_detach }).collect();
                Response::Grants { items }
            }
            Request::SetGate { current, new } if need(Capability::Input) => {
                // One change at a time; the phrase's own lock is taken only for the check and the swap, never across the file write.
                let _one_change = self.gate_serial.locked();
                if !self.gate.locked().verify(&current) {
                    return err("gate_failed", "The current confirmation phrase did not match.");
                }
                let mut salt = [0u8; 16];
                random_bytes(&mut salt);
                let Some(next) = Gate::new(&new, salt) else { return err("invalid_phrase", "Use letters, digits and ! @ # $ % ^ & * ( ) only, up to 256 characters.") };
                let (s, h) = next.parts();
                if std::fs::write(self.cfg.data_dir.join("gate.txt"), format!("{}:{}", auth::hex(&s), auth::hex(&h))).is_err() {
                    return err("gate_not_saved", "The new phrase could not be saved.");
                }
                *self.gate.locked() = next;
                Response::Ok
            }
            Request::AcceptDraft { session } if need(Capability::Input) => {
                let Some(s) = self.session(&session) else { return err("no_such_session", "Unknown session.") };
                // Taking the draft first keeps accepting single-use even if two clients press Accept at once.
                let Some(draft) = s.draft.locked().take() else { return err("no_draft", "There is no staged draft.") };
                match s.call(self, host::OP_INPUT, &caps::accept_bytes(&draft), HOST_RPC_TIMEOUT_MS) {
                    RpcOutcome::Reply(r) if r == b"OK" => Response::Ok,
                    RpcOutcome::Reply(_) => err("host_error", "The session host did not accept the draft."),
                    RpcOutcome::NotSent => {
                        // Nothing reached the host, so nothing was typed: put the draft back for a retry, unless the
                        // agent has staged a newer one in the meantime.
                        let mut slot = s.draft.locked();
                        if slot.is_none() {
                            *slot = Some(draft);
                        }
                        err("host_error", "The session host could not be reached. Nothing was typed and the draft is still staged.")
                    }
                    // The request was written but never confirmed: the host may still type it. It is NOT kept, because
                    // accepting it again could type it twice.
                    RpcOutcome::Unconfirmed => err("host_unconfirmed", "The session host did not confirm the draft. It may still arrive, so check the terminal before asking the agent to propose it again."),
                }
            }
            Request::GrantCreate { agent, session, duration, keep_after_detach, gate } if need(Capability::Input) => {
                if self.session(&session).is_none() {
                    return err("no_such_session", "Unknown session.");
                }
                let gate_ok = self.gate.locked().verify(&gate);
                let created = self.grants_guard().create(&agent, &session, duration.into(), keep_after_detach, gate_ok, now_ms());
                match created {
                    Ok(()) => Response::Ok,
                    Err(caps::GrantError::GateFailed) => err("gate_failed", "The confirmation phrase did not match."),
                    Err(_) => err("invalid_agent", "Invalid agent or session id."),
                }
            }
            Request::GrantRevoke { agent, session } if need(Capability::Input) => {
                self.grants_guard().revoke(&agent, &session);
                Response::Ok
            }
            Request::AdminBegin { action } if need(Capability::Spawn) => {
                // The sessions counted now are the ones a confirmation will end, however many appear meanwhile.
                let targets: Vec<String> = if matches!(action, Admin::EndAllSessions) { self.sessions_guard().keys().cloned().collect() } else { Vec::new() };
                let affected = targets.len();
                self.admin_guard().begin(action.into(), targets, now_ms());
                Response::AdminPending { action, affected, wait_ms: caps::ADMIN_MIN_DELAY_MS }
            }
            Request::AdminCancel {} if need(Capability::Spawn) => {
                self.admin_guard().cancel();
                Response::Ok
            }
            Request::AdminConfirm { typed } if need(Capability::Spawn) => {
                // The admin lock is released here, before any session is ended (that waits on hosts).
                let confirmed = self.admin_guard().confirm(&typed, now_ms());
                match confirmed {
                    Ok((AdminAction::EndAllSessions, ids)) => {
                        self.audit(&format!("admin end-all confirmed by={via} sessions={}", ids.len()));
                        let mut all_ok = true;
                        for id in ids {
                            // A counted session that is already gone (ended meanwhile) needs nothing more.
                            if self.session(&id).is_none() {
                                continue;
                            }
                            all_ok &= matches!(self.end_session(&id, via), Response::Ok);
                        }
                        if all_ok {
                            Response::Ok
                        } else {
                            err("teardown_unproven", "At least one session ended without proof of a clean teardown.")
                        }
                    }
                    Ok((AdminAction::StopDaemon, _)) => {
                        // Hosts keep running: stopping the daemon never ends sessions.
                        self.audit(&format!("admin stop-daemon confirmed by={via}"));
                        self.stop();
                        Response::Ok
                    }
                    Err(AdminError::NoPendingRequest) => err("admin_no_request", "No pending admin request."),
                    Err(AdminError::TooEarly) => err("admin_too_early", "Wait for the countdown to finish."),
                    Err(AdminError::Expired) => err("admin_expired", "The request expired; start again."),
                    Err(AdminError::ConfirmationMismatch) => err("admin_mismatch", "The typed count did not match."),
                }
            }
            _ => err("forbidden", "This connection lacks the capability for that request."),
        }
    }
}

/// Whether a connection's pipe handle is still open. The handshake timer may only cancel I/O while it is, and
/// closing the handle first marks the gate closed under the same lock, so a timer can never act on a handle that
/// has been closed (and may since have been reused for another connection or a host pipe). This is the fix for
/// review finding F7 (stale timers cut 13 of 60 live connections in the test).
struct ConnGate {
    open: Mutex<bool>,
}
impl ConnGate {
    fn new() -> Arc<ConnGate> {
        Arc::new(ConnGate { open: Mutex::new(true) })
    }
    fn is_open(&self) -> bool {
        *self.open.locked()
    }
    fn cancel_io(&self, raw: usize) {
        let open = self.open.locked();
        if *open {
            cancel_io(raw);
        }
    }
    fn mark_closed(&self) {
        *self.open.locked() = false;
    }
}

/// A connection waiting to authenticate before its deadline.
struct PendingHandshake {
    raw: usize,
    deadline: Instant,
    authed: Arc<AtomicBool>,
    gate: Arc<ConnGate>,
}

/// Flushes, disconnects and closes a connection's pipe when it goes out of scope, however the handler ends. The
/// gate is closed first, so no timer can act on the handle afterwards.
struct PipeCloser {
    h: HANDLE,
    gate: Arc<ConnGate>,
}
impl Drop for PipeCloser {
    fn drop(&mut self) {
        self.gate.mark_closed();
        flush_and_disconnect_pipe(self.h);
        close_handle(self.h);
    }
}

/// One of the `MAX_CONNECTIONS` places; released when the handler ends, whatever way it ends.
struct ConnectionSlot(Arc<Shared>);
impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}
/// Counts one attached human client for as long as it lives, so a handler that ends early or panics cannot leave
/// the count too high (which would keep agent grants alive).
struct HumanGuard<'a>(&'a Shared);
impl<'a> HumanGuard<'a> {
    fn attach(shared: &'a Shared) -> HumanGuard<'a> {
        shared.humans.fetch_add(1, Ordering::SeqCst);
        HumanGuard(shared)
    }
}
impl Drop for HumanGuard<'_> {
    fn drop(&mut self) {
        self.0.humans.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(shared: Arc<Shared>, pipe: HANDLE) {
    let gate = ConnGate::new();
    let _closer = PipeCloser { h: pipe, gate: gate.clone() };
    let mut io = pipe_io(pipe);
    // Elevated (or unknowable) clients are refused with a clear, local-only message.
    if pipe_client_elevated(&io) != Some(false) {
        let _ = proto::send(&mut io, &err("elevated_client", "Workbench runs as your standard user; run this from a non-elevated shell."));
        return;
    }
    let mut nonce = [0u8; auth::NONCE_BYTES];
    random_bytes(&mut nonce);
    if proto::send(&mut io, &Response::Challenge { version: PROTOCOL_VERSION, nonce: auth::hex(&nonce) }).is_err() {
        return;
    }
    // Watchdog: a connected client that never answers is cut off. The single watchdog thread in `run` sweeps `pending`.
    let authed = Arc::new(AtomicBool::new(false));
    shared.pending.locked().push(PendingHandshake { raw: pipe as usize, deadline: Instant::now() + HANDSHAKE_TIMEOUT, authed: authed.clone(), gate });
    let (kind, agent) = match proto::recv_request(&mut io) {
        Ok(Some(Request::Hello { version, client, proof, agent }))
            if version == PROTOCOL_VERSION
                && auth::verify(&shared.secret, &nonce, version, client, &proof)
                // An agent connection must name itself with a valid id; nobody else may carry one.
                && (matches!(client, ClientKind::Agent) == agent.as_deref().is_some_and(proto::valid_agent_id))
                && (matches!(client, ClientKind::Agent) || agent.is_none()) =>
        {
            (client, agent)
        }
        Ok(Some(_)) | Err(_) => {
            thread::sleep(Duration::from_millis(200)); // blunt brute-force pacing
            let _ = proto::send(&mut io, &err("auth_failed", "Authentication failed."));
            return;
        }
        Ok(None) => return,
    };
    authed.store(true, Ordering::SeqCst);
    // Capabilities and origin come from the authenticated kind, never from a request field.
    let (caps, origin) = match (kind, agent) {
        (ClientKind::Agent, Some(id)) => (CapSet::AGENT, Origin::Agent(id)),
        _ => (CapSet::USER, Origin::Human),
    };
    if proto::send(&mut io, &Response::Ok).is_err() {
        return;
    }
    // Who is asking, for the audit log only (never used for any permission decision).
    let via = match (&origin, kind) {
        (Origin::Agent(id), _) => format!("agent:{id}"),
        (_, ClientKind::Gui) => "gui".to_owned(),
        _ => "cli".to_owned(),
    };
    // Only human clients count as "a human is attached"; an agent never keeps grants alive.
    let _human = matches!(origin, Origin::Human).then(|| HumanGuard::attach(&shared));
    while let Ok(Some(req)) = proto::recv_request(&mut io) {
        // A panic while serving one request must not take this connection's thread, the pipe handle or the
        // human count with it: the client gets an error reply and the service carries on.
        let resp = match catch_unwind(AssertUnwindSafe(|| shared.handle(caps, &origin, &via, req))) {
            Ok(resp) => resp,
            Err(_) => {
                shared.audit(&format!("a request from {via} panicked; the client was sent an error"));
                err("internal_error", "The service hit an internal error handling that request.")
            }
        };
        shared.report_poison();
        if proto::send(&mut io, &resp).is_err() {
            break;
        }
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
    }
}

/// Bind the endpoint, recover sessions, then serve until stopped. `ready` runs after
/// recovery and just before the accept loop (clients may connect from that point on).
pub fn run(cfg: BrokerConfig, ready: impl FnOnce(&Arc<Shared>)) -> Result<(), StartError> {
    ensure_private_dir(&cfg.data_dir).map_err(StartError::Io)?;
    // Single instance: the first-instance pipe is created BEFORE anything else is opened.
    let mut next = match create_pipe_instance(&cfg.pipe_name, true) {
        Ok(h) => h,
        Err(ERROR_ACCESS_DENIED) | Err(ERROR_PIPE_BUSY) => return Err(StartError::AlreadyRunning),
        Err(e) => return Err(StartError::Io(std::io::Error::from_raw_os_error(e as i32))),
    };
    let secret = load_secret(&cfg.data_dir, true).map_err(StartError::Io)?;
    let mut salt = [0u8; 16];
    random_bytes(&mut salt);
    // A phrase saved from the UI wins over the start-up flag, so changing it sticks across restarts.
    let saved = std::fs::read_to_string(cfg.data_dir.join("gate.txt")).ok().and_then(|t| {
        let (s, h) = t.trim().split_once(':')?;
        let (s, h) = (auth::unhex(s)?, auth::unhex(h)?);
        Some(Gate::from_parts(s.try_into().ok()?, h.try_into().ok()?))
    });
    let gate = match saved {
        Some(g) => g,
        None => Gate::new(&cfg.gate_phrase, salt).ok_or_else(|| StartError::Io(std::io::Error::other("invalid gate phrase")))?,
    };
    let data_dir_for_epoch = cfg.data_dir.clone();
    let shared = Arc::new(Shared {
        cfg,
        secret,
        sessions: Mutex::new(BTreeMap::new()),
        lost: Mutex::new(Vec::new()),
        grants: Mutex::new(GrantTable::new()),
        admin: Mutex::new(AdminFlow::new()),
        gate: Mutex::new(gate),
        humans: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        epoch: AtomicU64::new(next_epoch(&data_dir_for_epoch) - 1),
        reported: Mutex::new(std::collections::HashSet::new()),
        registry_write: Mutex::new(()),
        gate_serial: Mutex::new(()),
        poison_seen: AtomicU64::new(crate::sync::poison_recoveries()),
        pending: Mutex::new(Vec::new()),
        connections: AtomicUsize::new(0),
        at_capacity: AtomicBool::new(false),
        logs: Mutex::new(BTreeMap::new()),
        log_seq: AtomicU64::new(0),
        wake: Wake::new(),
    });
    shared.recover();
    shared.audit(&format!("daemon started adopted={} lost={}", shared.sessions_guard().len(), shared.lost.locked().len()));
    // Cancels host calls that outlive their deadline (a hung or suspended host).
    let wd = shared.clone();
    thread::spawn(move || {
        while !wd.stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            wd.expire_handshakes();
            let sessions: Vec<Arc<Session>> = wd.sessions_guard().values().cloned().collect();
            for s in sessions {
                let d = s.deadline.load(Ordering::SeqCst);
                if d != 0 && now_ms() > d && s.deadline.load(Ordering::SeqCst) == d {
                    cancel_io(s.raw.load(Ordering::SeqCst));
                }
            }
        }
    });
    ready(&shared);
    loop {
        let connected = accept_pipe(next);
        if shared.stop.load(Ordering::SeqCst) {
            close_handle(next);
            break;
        }
        let this = next;
        next = match create_pipe_instance(&shared.cfg.pipe_name, false) {
            Ok(h) => h,
            Err(e) => return Err(StartError::Io(std::io::Error::from_raw_os_error(e as i32))),
        };
        if connected {
            if shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                // At the limit: refuse at once instead of starting another thread.
                shared.connections.fetch_sub(1, Ordering::SeqCst);
                if !shared.at_capacity.swap(true, Ordering::SeqCst) {
                    shared.audit(&format!("connection limit ({MAX_CONNECTIONS}) reached; refusing new connections until some close"));
                }
                disconnect_pipe(this);
                close_handle(this);
            } else {
                shared.at_capacity.store(false, Ordering::SeqCst);
                let (s, slot) = (shared.clone(), ConnectionSlot(shared.clone()));
                let raw = this as usize;
                thread::spawn(move || {
                    let _slot = slot;
                    handle_connection(s, raw as HANDLE);
                });
            }
        } else {
            close_handle(this);
        }
    }
    Ok(())
}

#[cfg(test)]
mod log_table_tests {
    use super::log_to_evict;

    #[test]
    fn a_full_table_drops_the_oldest_finished_log_before_any_running_one() {
        assert_eq!(log_to_evict([(1, true), (2, false), (3, false)].into_iter()), Some(2), "the oldest running log is kept");
        assert_eq!(log_to_evict([(4, false), (1, true), (2, true)].into_iter()), Some(4));
        assert_eq!(log_to_evict([(3, true), (1, true), (2, true)].into_iter()), Some(1), "all running: the oldest goes");
        assert_eq!(log_to_evict(std::iter::empty()), None);
    }
}

#[cfg(test)]
mod audit_tests {
    use super::iso_utc;

    #[test]
    fn timestamps_are_utc_iso_8601() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00Z", "leap day");
        assert_eq!(iso_utc(1_792_000_000_000), "2026-10-14T17:46:40Z");
        assert_eq!(iso_utc(4_102_444_799_000), "2099-12-31T23:59:59Z");
    }
}
