//! Launch Claude Code, Pi or Codex in a persistent terminal and link it to its transcript.
//! Design: docs/design/agent-terminals.md. A launch is a HUMAN (GUI) action: an ordinary PowerShell
//! terminal is created and the fixed, validated launch line is typed into it, so the user sees exactly
//! what ran and keeps the shell when the agent exits. The webview never supplies a transcript path:
//! it asks for the link by terminal id and receives an opaque inspector source id.
use crate::{error, inspector::Inspector, terminals::Terminals, AppError};
use agent_workbench::agent_terminals::{self as reg, Entry};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tauri::Manager;
use wb_observe::{link, AgentKind};
use wb_session::proto::{Request, Response, Shell};

/// Transcripts already linked to a terminal (in memory; recomputed after a restart in launch order).
#[derive(Default)]
pub struct AgentLinks {
    linked: Mutex<HashMap<String, PathBuf>>,
    /// Serialises registry read-modify-write cycles.
    registry: Mutex<()>,
}

fn registry_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    app.path().app_data_dir().map(|d| d.join("agent-terminals.json")).map_err(|_| error("agent_failed", "The app data folder is unavailable."))
}
fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}
fn kind_of(agent: &str) -> Option<AgentKind> {
    match agent {
        "claude" => Some(AgentKind::Claude),
        "pi" => Some(AgentKind::Pi),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Create the terminal and type the launch line. Returns the new terminal's info and the registry entry.
fn launch(t: &Terminals, registry_file: &Path, links: &AgentLinks, agent: &str, cwd: &str) -> Result<Value, AppError> {
    if reg::agent_title(agent).is_none() {
        return Err(error("agent_failed", "Unknown agent."));
    }
    if !reg::valid_cwd(cwd) {
        return Err(error("agent_failed", "Choose an absolute folder path such as C:\\Users\\you\\project (no quotes or special characters)."));
    }
    if !Path::new(cwd).is_dir() {
        return Err(error("agent_failed", "That folder does not exist."));
    }
    // Claude Code and Pi are given an id we can find their transcript by; Codex cannot be given one.
    let session_id = if agent == "codex" { String::new() } else { uuid::Uuid::new_v4().to_string() };
    let line = reg::launch_line(agent, &session_id, cwd).ok_or_else(|| error("agent_failed", "The launch command could not be built."))?;
    start_terminal(t, registry_file, links, agent, cwd, session_id, &line)
}

/// Create a PowerShell terminal, register it as running `agent`, then type `line` into it. Registered
/// before typing, so even a failure afterwards leaves a terminal the user can still see and end.
fn start_terminal(t: &Terminals, registry_file: &Path, links: &AgentLinks, agent: &str, cwd: &str, session_id: String, line: &str) -> Result<Value, AppError> {
    let launched_ms = now_ms();
    let created = t.call(&Request::NewTerminal { cols: 120, rows: 30, label: Some(reg::label(agent, cwd)), shell: Some(Shell::PowerShell), extended_history: None, cwd: None })?;
    let Response::Created { session } = created else {
        return Err(error("agent_failed", "The terminal service gave an unexpected answer."));
    };
    let terminal = session.id.clone();
    // Registered before typing, so even a failure below leaves a terminal the user can still see and end.
    {
        let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
        let mut registry = reg::load(registry_file);
        let entry = Entry { terminal: terminal.clone(), agent: agent.to_owned(), session_id, cwd: cwd.to_owned(), launched_ms };
        if !registry.add(entry) || reg::save(registry_file, &registry).is_err() {
            return Err(error("agent_failed", "The agent was not registered; the terminal was created but nothing was started in it."));
        }
    }
    t.call(&Request::Input { session: terminal.clone(), text: format!("{line}\r") })?;
    Ok(json!({ "terminal": serde_json::to_value(&session).unwrap_or(Value::Null), "agent": agent }))
}

/// A transcript written this recently probably belongs to a session still running somewhere else.
const BUSY_WITHIN_MS: u64 = 2 * 60_000;

fn agent_of(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Pi => "pi",
        AgentKind::Codex => "codex",
    }
}

/// Resume an existing session (one the user started themselves, found by the inspector) in a Workbench
/// terminal. Windows cannot move a running process between consoles, so this starts a NEW process that
/// continues the same session; two processes writing one session at once would interleave it, so a session
/// whose transcript changed in the last two minutes is only resumed when `force` says the user checked.
/// The agent, its id and its folder all come from the transcript, never from the webview.
fn resume(t: &Terminals, registry_file: &Path, links: &AgentLinks, inspector: &Inspector, source: &str, force: bool) -> Result<Value, AppError> {
    let found = inspector.find(source).map_err(|_| error("agent_failed", "That session is no longer available."))?;
    let agent = agent_of(found.kind);
    let session = link::session_ref(found.kind, &found.path).ok_or_else(|| error("agent_failed", "This session's id and folder could not be read from its transcript."))?;
    if !reg::valid_cwd(&session.cwd) {
        return Err(error("agent_failed", "The folder this session ran in has a path that cannot be typed safely, so it was not resumed."));
    }
    if !Path::new(&session.cwd).is_dir() {
        return Err(error("agent_failed", "The folder this session ran in no longer exists."));
    }
    let mut line = reg::resume_line(agent, &session.id, &session.cwd).ok_or_else(|| error("agent_failed", "The resume command could not be built."))?;
    // Already open in a Workbench terminal that still exists: show that one instead of starting a second copy.
    let existing = {
        let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
        reg::load(registry_file).entries.into_iter().filter(|e| e.agent == agent && e.session_id == session.id).map(|e| e.terminal).collect::<Vec<_>>()
    };
    if !existing.is_empty() {
        if let Response::Sessions { items } = t.call(&Request::List {})? {
            if let Some(open) = items.into_iter().find(|i| existing.contains(&i.id)) {
                return Ok(json!({ "state": "already_open", "agent": agent, "terminal": serde_json::to_value(&open).unwrap_or(Value::Null) }));
            }
        }
    }
    // Claude Code knows which of its sessions are running. One running in the background is ATTACHED to (no
    // second copy: Claude refuses to resume it); one open in another window is only resumed after the user confirms.
    let mut running = false;
    if agent == "claude" {
        match crate::claude_ps::live(&session.id) {
            reg::ClaudeLive::Background { short } => {
                line = reg::attach_line(&short, &session.cwd).ok_or_else(|| error("agent_failed", "The attach command could not be built."))?;
                return finish_open(t, registry_file, links, agent, &session, &line, source);
            }
            reg::ClaudeLive::Interactive => {
                if !force {
                    return Ok(json!({ "state": "busy", "agent": agent, "age_ms": 0, "running": true }));
                }
                running = true;
            }
            reg::ClaudeLive::NotRunning => {}
        }
    }
    let modified = std::fs::metadata(&found.path).ok().and_then(|m| m.modified().ok()).and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
    let age_ms = now_ms().saturating_sub(modified);
    if !force && !running && age_ms <= BUSY_WITHIN_MS {
        return Ok(json!({ "state": "busy", "agent": agent, "age_ms": age_ms, "running": false }));
    }
    finish_open(t, registry_file, links, agent, &session, &line, source)
}

fn finish_open(t: &Terminals, registry_file: &Path, links: &AgentLinks, agent: &str, session: &link::SessionRef, line: &str, source: &str) -> Result<Value, AppError> {
    let mut started = start_terminal(t, registry_file, links, agent, &session.cwd, session.id.clone(), line)?;
    if let Value::Object(map) = &mut started {
        map.insert("state".into(), json!("started"));
        map.insert("source".into(), json!(source));
    }
    Ok(started)
}

/// Recreate an agent's terminal after a restart: the terminal itself comes back under its old id (so its registry entry, its
/// window and its transcript link stay valid), then the agent's resume line is typed into it, the same line "Open in terminal"
/// uses. A terminal that was not launched as an agent, or an agent with no session id to resume (a Codex started fresh), comes
/// back as a plain shell and says so.
fn restore(t: &Terminals, registry_file: &Path, links: &AgentLinks, terminal: &str) -> Result<Value, AppError> {
    let entry = {
        let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
        reg::load(registry_file).entries.into_iter().find(|e| e.terminal == terminal)
    };
    // Agents are always launched in PowerShell (`start_terminal`), whatever an older service recorded for the terminal.
    let created = t.call(&Request::Restore { session: terminal.to_owned(), shell: entry.as_ref().map(|_| Shell::PowerShell) })?;
    let Response::Created { session } = created else {
        return Err(error("agent_failed", "The terminal service gave an unexpected answer."));
    };
    let line = entry.as_ref().and_then(|e| reg::resume_line(&e.agent, &e.session_id, &e.cwd));
    let resumed = match &line {
        Some(line) => {
            // A failed typing leaves a working terminal; the user sees the shell and can type the command.
            t.call(&Request::Input { session: terminal.to_owned(), text: format!("{line}\r") }).is_ok()
        }
        None => false,
    };
    Ok(json!({ "terminal": serde_json::to_value(&session).unwrap_or(Value::Null), "agent": entry.map(|e| e.agent), "resumed": resumed }))
}

fn list(registry_file: &Path, links: &AgentLinks) -> Result<Value, AppError> {
    let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
    let registry = reg::load(registry_file);
    Ok(json!(registry.entries.iter().map(|e| json!({ "terminal": e.terminal, "agent": e.agent, "cwd": e.cwd, "launched_ms": e.launched_ms })).collect::<Vec<_>>()))
}

fn forget(registry_file: &Path, links: &AgentLinks, terminal: &str) -> Result<(), AppError> {
    let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
    let mut registry = reg::load(registry_file);
    if registry.forget(terminal) {
        reg::save(registry_file, &registry).map_err(|_| error("agent_failed", "The agent registry could not be saved."))?;
    }
    if let Ok(mut map) = links.linked.lock() {
        map.remove(terminal);
    }
    Ok(())
}

/// Find the agent's transcript. Claims are decided in launch order so two Codex agents started in the
/// same folder are told apart the same way every time. `pending` until the transcript exists.
fn find_link(registry_file: &Path, links: &AgentLinks, inspector: &Inspector, terminal: &str) -> Result<Value, AppError> {
    let entries = {
        let _guard = links.registry.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
        let mut all = reg::load(registry_file).entries;
        all.sort_by_key(|e| e.launched_ms);
        all
    };
    if !entries.iter().any(|e| e.terminal == terminal) {
        return Err(error("agent_failed", "That terminal was not launched as an agent."));
    }
    let home = home().ok_or_else(|| error("agent_failed", "The home folder could not be determined."))?;
    let mut map = links.linked.lock().map_err(|_| error("agent_failed", "Agent state is unavailable."))?;
    for entry in &entries {
        if !map.contains_key(&entry.terminal) {
            let Some(kind) = kind_of(&entry.agent) else { continue };
            let claimed: HashSet<PathBuf> = map.values().cloned().collect();
            let launch = link::Launch { kind, session_id: entry.session_id.clone(), cwd: entry.cwd.clone(), launched_ms: entry.launched_ms };
            if let Some(path) = link::resolve(&home, &launch, &claimed) {
                map.insert(entry.terminal.clone(), path);
            }
        }
        if entry.terminal == terminal {
            break;
        }
    }
    let entry = entries.iter().find(|e| e.terminal == terminal).expect("checked above");
    match map.get(terminal) {
        Some(path) => {
            let kind = kind_of(&entry.agent).expect("registry entries are validated");
            let source = inspector.pin(kind, path).ok_or_else(|| error("agent_failed", "The transcript could not be opened."))?;
            Ok(json!({ "state": "linked", "source": source, "agent": entry.agent }))
        }
        None => Ok(json!({ "state": "pending", "agent": entry.agent })),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, AppError> + Send + 'static) -> Result<T, AppError> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| error("agent_failed", "The request was interrupted."))?
}

#[tauri::command]
pub async fn agent_launch(app: tauri::AppHandle, t: tauri::State<'_, Arc<Terminals>>, links: tauri::State<'_, Arc<AgentLinks>>, agent: String, cwd: String) -> Result<Value, AppError> {
    let (t, links, file) = (t.inner().clone(), links.inner().clone(), registry_path(&app)?);
    blocking(move || launch(&t, &file, &links, &agent, &cwd)).await
}

#[tauri::command]
pub async fn agent_resume(app: tauri::AppHandle, t: tauri::State<'_, Arc<Terminals>>, links: tauri::State<'_, Arc<AgentLinks>>, inspector: tauri::State<'_, Arc<Inspector>>, source: String, force: bool) -> Result<Value, AppError> {
    if source.len() != 16 || !source.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error("agent_failed", "Invalid session id."));
    }
    let (t, links, inspector, file) = (t.inner().clone(), links.inner().clone(), inspector.inner().clone(), registry_path(&app)?);
    blocking(move || resume(&t, &file, &links, &inspector, &source, force)).await
}

#[tauri::command]
pub async fn agent_restore(app: tauri::AppHandle, t: tauri::State<'_, Arc<Terminals>>, links: tauri::State<'_, Arc<AgentLinks>>, terminal: String) -> Result<Value, AppError> {
    if terminal.len() != 8 || !terminal.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error("agent_failed", "Invalid terminal id."));
    }
    let file = registry_path(&app)?;
    let (t, links) = (t.inner().clone(), links.inner().clone());
    blocking(move || restore(&t, &file, &links, &terminal)).await
}

#[tauri::command]
pub async fn agent_terminals(app: tauri::AppHandle, links: tauri::State<'_, Arc<AgentLinks>>) -> Result<Value, AppError> {
    let (links, file) = (links.inner().clone(), registry_path(&app)?);
    blocking(move || list(&file, &links)).await
}

#[tauri::command]
pub async fn agent_forget(app: tauri::AppHandle, links: tauri::State<'_, Arc<AgentLinks>>, terminal: String) -> Result<(), AppError> {
    let (links, file) = (links.inner().clone(), registry_path(&app)?);
    blocking(move || forget(&file, &links, &terminal)).await
}

#[tauri::command]
pub async fn agent_link(app: tauri::AppHandle, links: tauri::State<'_, Arc<AgentLinks>>, inspector: tauri::State<'_, Arc<Inspector>>, terminal: String) -> Result<Value, AppError> {
    if terminal.len() != 8 || !terminal.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error("agent_failed", "Invalid terminal id."));
    }
    let (links, inspector, file) = (links.inner().clone(), inspector.inner().clone(), registry_path(&app)?);
    blocking(move || find_link(&file, &links, &inspector, &terminal)).await
}
