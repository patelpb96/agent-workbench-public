//! Native bridge from the webview to the persistent-session broker (`wb-daemon`).
//! The webview never talks to the broker or a host: it calls these commands, and this
//! layer holds the single authenticated connection (always the human origin). The
//! broker is started on demand and outlives this app, so closing the window never
//! ends a terminal. Design: docs/design/session-daemon.md.
use crate::{error, AppError};
use std::{
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use wb_session::{
    client::Client,
    proto::{ClientKind, Request, Response, WatchCursor},
    win,
};

const DETACHED: u32 = 0x0000_0008; // DETACHED_PROCESS
const NEW_GROUP: u32 = 0x0000_0200; // CREATE_NEW_PROCESS_GROUP
const NO_WINDOW: u32 = 0x0800_0000; // CREATE_NO_WINDOW
const BREAKAWAY: u32 = 0x0100_0000; // CREATE_BREAKAWAY_FROM_JOB

/// Per attempt; the service is local, so an answer takes milliseconds.
const QUICK_CONNECT: Duration = Duration::from_millis(300);
/// After a failed start, further calls fail fast for this long instead of retrying.
const BACKOFF: Duration = Duration::from_secs(2);
/// Calls waiting for the connection beyond this are refused at once (the UI polls again).
const MAX_WAITERS: usize = 8;
/// Output watches in flight at once (the webview keeps one going, and starts the next before the old one has
/// finished when its set of terminals changes). Each has a service connection of its own while it waits.
const MAX_WATCHES: usize = 4;

pub struct Terminals {
    conn: Mutex<Option<Client>>,
    last_failure: Mutex<Option<Instant>>,
    waiters: AtomicUsize,
    /// Idle connections kept for `watch`, which holds its connection for up to two seconds per call.
    watch_pool: Mutex<Vec<Client>>,
    watching: AtomicUsize,
    data_dir: PathBuf,
    pipe: String,
}

struct Waiting<'a>(&'a AtomicUsize);
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Terminals {
    pub fn new() -> Arc<Terminals> {
        Self::with_paths(win::default_data_dir(), win::default_pipe_name())
    }

    fn with_paths(data_dir: PathBuf, pipe: String) -> Arc<Terminals> {
        Arc::new(Terminals { conn: Mutex::new(None), last_failure: Mutex::new(None), waiters: AtomicUsize::new(0), watch_pool: Mutex::new(Vec::new()), watching: AtomicUsize::new(0), data_dir, pipe })
    }

    fn daemon_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?.with_file_name("wb-daemon.exe");
        exe.exists().then_some(exe)
    }

    /// Start the broker if nothing answers. Detached from this process and its Job so
    /// it survives the app; if breakaway is refused it still starts, and the broker
    /// itself then refuses persistent sessions with a clear message.
    fn start_daemon(&self) -> Result<(), AppError> {
        let exe = Self::daemon_exe().ok_or_else(|| error("terminal_unavailable", "The terminal service (wb-daemon.exe) is not installed next to the app."))?;
        crate::lifecycle::record("terminals: starting the service");
        for flags in [DETACHED | NEW_GROUP | NO_WINDOW | BREAKAWAY, DETACHED | NEW_GROUP | NO_WINDOW] {
            let spawned = Command::new(&exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(flags).spawn();
            if spawned.is_ok() {
                return Ok(());
            }
        }
        Err(error("terminal_unavailable", "The terminal service could not be started."))
    }

    fn connect(&self) -> Result<Client, AppError> {
        if win::self_elevated() {
            return Err(error("terminal_unavailable", "Workbench runs as your standard user; start it from a non-elevated session."));
        }
        if self.last_failure.lock().map(|f| f.is_some_and(|t| t.elapsed() < BACKOFF)).unwrap_or(false) {
            return Err(error("terminal_unavailable", "The terminal service is not available; retrying shortly."));
        }
        let quick = || Client::connect_timeout(&self.pipe, &self.data_dir, ClientKind::Gui, QUICK_CONNECT);
        let first = quick();
        if let Ok(c) = first {
            return Ok(c);
        }
        // Start the service only when it is absent (no pipe or no secret yet); a present but busy
        // or refusing service must not be spawned over.
        let absent = first.as_ref().err().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(2));
        if let Err(e) = &first {
            crate::lifecycle::record(&format!("terminals: connect failed kind={:?} os={:?} absent={absent}", e.kind(), e.raw_os_error()));
        }
        if absent {
            if let Err(e) = self.start_daemon() {
                if let Ok(mut f) = self.last_failure.lock() {
                    *f = Some(Instant::now());
                }
                return Err(e);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match quick() {
                Ok(c) => return Ok(c),
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(150)),
                Err(e) => {
                    if let Ok(mut f) = self.last_failure.lock() {
                        *f = Some(Instant::now());
                    }
                    return Err(error("terminal_unavailable", format!("The terminal service did not respond ({e}).")));
                }
            }
        }
    }

    /// One request; reconnects once if the broker restarted since the last call.
    pub fn call(&self, req: &Request) -> Result<Response, AppError> {
        // Bounded queue: when the service is unreachable, polling must not pile up behind the lock.
        if self.waiters.fetch_add(1, Ordering::SeqCst) >= MAX_WAITERS {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            return Err(error("terminal_unavailable", "The terminal service is busy; try again."));
        }
        let _waiting = Waiting(&self.waiters);
        let mut guard = self.conn.lock().map_err(|_| error("terminal_unavailable", "Terminal state is unavailable."))?;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect()?);
            }
            match guard.as_mut().unwrap().call(req) {
                Ok(Response::Error { code, message }) => {
                    let _ = code; // broker codes stay in the message; the static code is generic
                    return Err(error("terminal_failed", message));
                }
                Ok(r) => return Ok(r),
                Err(_) if attempt == 0 => *guard = None,
                Err(e) => return Err(error("terminal_unavailable", format!("Lost the terminal service ({e})."))),
            }
        }
        Err(error("terminal_unavailable", "The terminal service did not answer."))
    }
}

impl Terminals {
    /// One `Watch` on a connection of its own (see `MAX_WATCHES`). A service that predates `Watch` drops the
    /// connection, which comes back here as an error; the webview then falls back to polling.
    pub fn watch(&self, cursors: Vec<WatchCursor>, wait_ms: u64) -> Result<Response, AppError> {
        if self.watching.fetch_add(1, Ordering::SeqCst) >= MAX_WATCHES {
            self.watching.fetch_sub(1, Ordering::SeqCst);
            return Err(error("watch_busy", "Too many output watches are running."));
        }
        let _active = Waiting(&self.watching);
        let pooled = self.watch_pool.lock().ok().and_then(|mut pool| pool.pop());
        let mut client = match pooled {
            Some(c) => c,
            None => self.connect()?,
        };
        match client.call(&Request::Watch { cursors, wait_ms }) {
            Ok(Response::Error { message, .. }) => {
                self.return_watch_connection(client);
                Err(error("terminal_failed", message))
            }
            Ok(r) => {
                self.return_watch_connection(client);
                Ok(r)
            }
            Err(e) => Err(error("terminal_unavailable", format!("Lost the terminal service ({e})."))),
        }
    }

    fn return_watch_connection(&self, client: Client) {
        if let Ok(mut pool) = self.watch_pool.lock() {
            if pool.len() < MAX_WATCHES {
                pool.push(client);
            }
        }
    }
}

async fn run<T: Send + 'static>(t: tauri::State<'_, Arc<Terminals>>, f: impl FnOnce(&Terminals) -> Result<T, AppError> + Send + 'static) -> Result<T, AppError> {
    let t = t.inner().clone();
    tauri::async_runtime::spawn_blocking(move || f(&t)).await.map_err(|_| error("terminal_failed", "The terminal request was interrupted."))?
}

fn value(r: Response) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(r).map_err(|_| error("terminal_failed", "Unexpected terminal reply."))
}

fn valid_id(id: &str) -> Result<(), AppError> {
    if id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(error("terminal_failed", "Invalid session id."))
    }
}

#[tauri::command]
pub async fn terminal_list(t: tauri::State<'_, Arc<Terminals>>) -> Result<serde_json::Value, AppError> {
    run(t, |t| value(t.call(&Request::List {})?)).await
}

#[tauri::command]
pub async fn terminal_new(app: tauri::AppHandle, t: tauri::State<'_, Arc<Terminals>>, cols: u16, rows: u16, label: Option<String>, extended_history: bool, folder: Option<String>) -> Result<serde_json::Value, AppError> {
    // A folder asked for (the Files panel's "Open terminal here") must be a valid existing folder; otherwise the Properties
    // window's start folder (a stale or missing one means the user's folder).
    let cwd = match folder.as_deref().map(str::trim) {
        Some(dir) => {
            if !wb_session::proto::valid_start_dir(dir) || !std::path::Path::new(dir).is_dir() {
                return Err(error("invalid_folder", "That folder does not exist or cannot be used."));
            }
            Some(dir.to_owned())
        }
        None => crate::props::settings_path(&app).ok().and_then(|p| agent_workbench::settings::load(&p).start_dir().map(str::to_owned)),
    };
    run(t, move |t| value(t.call(&Request::NewTerminal { cols, rows, label, shell: None, extended_history: extended_history.then_some(true), cwd })?)).await
}

#[tauri::command]
pub async fn terminal_read(t: tauri::State<'_, Arc<Terminals>>, session: String, from: u64) -> Result<serde_json::Value, AppError> {
    valid_id(&session)?;
    run(t, move |t| value(t.call(&Request::Read { session, from })?)).await
}

/// Output for many terminals at once: waits until one of them has output beyond its cursor (or about a second
/// passes), then returns it. The webview keeps one of these going instead of polling every terminal.
#[tauri::command]
pub async fn terminal_watch(t: tauri::State<'_, Arc<Terminals>>, cursors: Vec<(String, u64)>, wait_ms: u64) -> Result<serde_json::Value, AppError> {
    if cursors.is_empty() || cursors.len() > wb_session::proto::MAX_WATCH_CURSORS {
        return Err(error("terminal_failed", "A watch needs between 1 and 64 terminals."));
    }
    for (session, _) in &cursors {
        valid_id(session)?;
    }
    let cursors = cursors.into_iter().map(|(session, from)| WatchCursor { session, from }).collect();
    run(t, move |t| value(t.watch(cursors, wait_ms.min(wb_session::proto::MAX_WATCH_WAIT_MS))?)).await
}

#[tauri::command]
pub async fn terminal_input(t: tauri::State<'_, Arc<Terminals>>, session: String, text: String) -> Result<(), AppError> {
    valid_id(&session)?;
    run(t, move |t| t.call(&Request::Input { session, text }).map(|_| ())).await
}

#[tauri::command]
pub async fn terminal_resize(t: tauri::State<'_, Arc<Terminals>>, session: String, cols: u16, rows: u16) -> Result<(), AppError> {
    valid_id(&session)?;
    run(t, move |t| t.call(&Request::Resize { session, cols, rows }).map(|_| ())).await
}

/// Recreate a terminal that did not survive a restart, under its old id (docs/design/restart-restore.md).
#[tauri::command]
pub async fn terminal_restore(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<serde_json::Value, AppError> {
    valid_id(&session)?;
    run(t, move |t| value(t.call(&Request::Restore { session, shell: None })?)).await
}

#[tauri::command]
pub async fn terminal_end(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<(), AppError> {
    valid_id(&session)?;
    run(t, move |t| t.call(&Request::End { session }).map(|_| ())).await
}

#[tauri::command]
pub fn terminal_autostart_get() -> bool {
    crate::autostart::is_enabled(crate::autostart::VALUE_NAME)
}

#[tauri::command]
pub fn terminal_autostart_set(enabled: bool) -> Result<bool, AppError> {
    let daemon = Terminals::daemon_exe().ok_or_else(|| error("terminal_unavailable", "The terminal service (wb-daemon.exe) is not installed next to the app."))?;
    if crate::autostart::set(crate::autostart::VALUE_NAME, &daemon, enabled) {
        Ok(crate::autostart::is_enabled(crate::autostart::VALUE_NAME))
    } else {
        Err(error("terminal_failed", "The sign-in setting could not be changed."))
    }
}

// ---- agent access: drafts, grants, confirmation phrase -------------------

fn valid_agent(id: &str) -> Result<(), AppError> {
    if wb_session::proto::valid_agent_id(id) {
        Ok(())
    } else {
        Err(error("terminal_failed", "Invalid agent name."))
    }
}

fn valid_phrase(p: &str) -> Result<(), AppError> {
    if wb_session::caps::gate_text_ok(p) {
        Ok(())
    } else {
        Err(error("terminal_failed", "Use letters, digits and ! @ # $ % ^ & * ( ) only."))
    }
}

/// Record a terminal's output to a file (tmux `pipe-pane`): `start`, `stop` or `status`. The service picks the file,
/// in its own logs folder; nothing here names a path.
#[tauri::command]
pub async fn terminal_log(t: tauri::State<'_, Arc<Terminals>>, session: String, action: String) -> Result<serde_json::Value, AppError> {
    valid_id(&session)?;
    let req = match action.as_str() {
        "start" => Request::LogStart { session },
        "stop" => Request::LogStop { session },
        "status" => Request::LogStatus { session },
        _ => return Err(error("terminal_failed", "Unknown log action.")),
    };
    run(t, move |t| value(t.call(&req)?)).await
}

#[tauri::command]
pub async fn terminal_draft(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<serde_json::Value, AppError> {
    valid_id(&session)?;
    run(t, move |t| value(t.call(&Request::GetDraft { session })?)).await
}

#[tauri::command]
pub async fn terminal_draft_accept(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<(), AppError> {
    valid_id(&session)?;
    run(t, move |t| t.call(&Request::AcceptDraft { session }).map(|_| ())).await
}

#[tauri::command]
pub async fn terminal_draft_discard(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<(), AppError> {
    valid_id(&session)?;
    run(t, move |t| t.call(&Request::DiscardDraft { session }).map(|_| ())).await
}

#[tauri::command]
pub async fn terminal_grants(t: tauri::State<'_, Arc<Terminals>>, session: String) -> Result<serde_json::Value, AppError> {
    valid_id(&session)?;
    run(t, move |t| value(t.call(&Request::Grants { session })?)).await
}

#[tauri::command]
pub async fn terminal_grant_create(t: tauri::State<'_, Arc<Terminals>>, agent: String, session: String, duration: String, keep_after_detach: bool, gate: String) -> Result<(), AppError> {
    valid_id(&session)?;
    valid_agent(&agent)?;
    valid_phrase(&gate)?;
    let duration = match duration.as_str() {
        "one_minute" => wb_session::proto::Duration::OneMinute,
        "fifteen_minutes" => wb_session::proto::Duration::FifteenMinutes,
        "one_hour" => wb_session::proto::Duration::OneHour,
        "one_day" => wb_session::proto::Duration::OneDay,
        _ => return Err(error("terminal_failed", "Unknown duration.")),
    };
    run(t, move |t| t.call(&Request::GrantCreate { agent, session, duration, keep_after_detach, gate }).map(|_| ())).await
}

#[tauri::command]
pub async fn terminal_grant_revoke(t: tauri::State<'_, Arc<Terminals>>, agent: String, session: String) -> Result<(), AppError> {
    valid_id(&session)?;
    valid_agent(&agent)?;
    run(t, move |t| t.call(&Request::GrantRevoke { agent, session }).map(|_| ())).await
}

#[tauri::command]
pub async fn terminal_gate_set(t: tauri::State<'_, Arc<Terminals>>, current: String, new: String) -> Result<(), AppError> {
    valid_phrase(&current)?;
    valid_phrase(&new)?;
    run(t, move |t| t.call(&Request::SetGate { current, new }).map(|_| ())).await
}

fn layout_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("terminal-layout.json"))
        .map_err(|_| error("terminal_failed", "The app data folder is unavailable."))
}

/// The remembered pane arrangement, or `None` when nothing valid is saved.
#[tauri::command]
pub fn terminal_layout_load(app: tauri::AppHandle) -> Result<Option<serde_json::Value>, AppError> {
    Ok(agent_workbench::terminal_layout::load(&layout_path(&app)?))
}

/// Validated, atomic save of the pane arrangement (view state only).
#[tauri::command]
pub fn terminal_layout_save(app: tauri::AppHandle, layout: serde_json::Value) -> Result<(), AppError> {
    agent_workbench::terminal_layout::save(&layout_path(&app)?, &layout)
        .map_err(|_| error("terminal_failed", "The pane layout could not be saved."))
}

fn windows_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("terminal-windows.json"))
        .map_err(|_| error("terminal_failed", "The app data folder is unavailable."))
}

/// Floating terminal windows and session links on the Agents canvas, or `None` when nothing valid is saved.
#[tauri::command]
pub fn terminal_windows_load(app: tauri::AppHandle) -> Result<Option<serde_json::Value>, AppError> {
    Ok(agent_workbench::terminal_windows::load(&windows_path(&app)?))
}

/// Validated, atomic save (view state only).
#[tauri::command]
pub fn terminal_windows_save(app: tauri::AppHandle, windows: serde_json::Value) -> Result<(), AppError> {
    agent_workbench::terminal_windows::save(&windows_path(&app)?, &windows)
        .map_err(|_| error("terminal_failed", "The terminal windows could not be saved."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unreachable service must make polling fail fast and must not queue up calls, or the
    /// app could not even exit while the service is down (observed 2026-10-21).
    #[test]
    fn an_unreachable_service_fails_fast_and_does_not_pile_up_calls() {
        let dir = std::env::temp_dir().join(format!("wb-term-test-{}", std::process::id()));
        let t = Terminals::with_paths(dir, format!(r"\\.\pipe\wb-term-test-{}", std::process::id()));
        let started = Instant::now();
        let first = t.call(&Request::List {});
        assert!(first.is_err());
        // The test executable has no wb-daemon.exe beside it, so starting it fails; later calls
        // fail immediately (backoff) rather than retrying for seconds each.
        let burst = Instant::now();
        let handles: Vec<_> = (0..40)
            .map(|_| {
                let t = t.clone();
                std::thread::spawn(move || t.call(&Request::List {}).is_err())
            })
            .collect();
        assert!(handles.into_iter().all(|h| h.join().unwrap()), "every call reports the outage");
        assert!(burst.elapsed() < Duration::from_secs(2), "40 concurrent calls took {:?}", burst.elapsed());
        assert!(started.elapsed() < Duration::from_secs(4), "total {:?}", started.elapsed());
        assert_eq!(t.waiters.load(Ordering::SeqCst), 0, "no call is left waiting");
    }
}
