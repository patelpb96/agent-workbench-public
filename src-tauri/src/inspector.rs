//! Read-only agent inspector bridge. Transcripts are discovered in their default
//! locations and referenced by an opaque id; the webview never supplies a path. Only
//! names, counts, timings and reported usage leave this module (see wb-observe).
use crate::{error, AppError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use agent_workbench::agent_terminals::ClaudeLive;
use wb_observe::{discover, link, map, peek, snapshot, AgentKind, Found, Observer, Peek};
use std::path::Path;

const MAX_SOURCES: usize = 60;
/// Subagent transcripts listed next to the sessions. Capped on their own, not inside `MAX_SOURCES`, because one session can fan out
/// into dozens of subagents and those must not push the session that produced them (or any other session) off the list.
const MAX_SUBAGENTS: usize = 20;
/// Parent sessions pulled into the list only because a listed subagent belongs to them, so a subagent is not shown without its parent.
const MAX_PARENT_ADDS: usize = 10;
/// Observers kept for the map: one per listed source, so tool facts come from an incremental tail rather than a re-read.
const MAX_MAP_OBSERVERS: usize = MAX_SOURCES + MAX_SUBAGENTS;
/// Cached per-file details (last line, model, folder) are dropped beyond this many files.
const MAX_DETAILS: usize = 400;
/// How far back a watched session is looked up by id (the visible list is only the newest `MAX_SOURCES`).
const LOCATE_LIMIT: usize = 400;
const MAX_OBSERVERS: usize = 8;
/// A session with no activity for this long reads as idle (a heuristic).
const IDLE_AFTER_MS: u64 = 5 * 60_000;
/// A transcript touched this recently is shown as active in the list.
const ACTIVE_WITHIN_MS: u64 = 2 * 60_000;
/// Cap work per request so one huge backlog cannot stall the UI thread pool.
const MAX_POLLS_PER_REQUEST: usize = 6;
/// The map polls more transcripts per call (each poll reads only what was appended), so a cold start fills in within a few calls
/// and the agents that are working now are always among them.
const MAX_MAP_POLLS: usize = 16;
/// Tools with a call in flight reported per session.
const MAX_OPEN_TOOLS: usize = 8;

#[derive(Serialize, Clone)]
pub struct SourceView {
    pub id: String,
    pub agent: &'static str,
    pub label: String,
    pub modified_ms: u64,
    pub bytes: u64,
    pub active: bool,
    /// Claude Code only: `background` or `interactive` while Claude reports the session as running.
    pub running: Option<&'static str>,
    /// The model named by the latest assistant message, when the transcript names one.
    pub model: Option<String>,
    /// The last line of the last assistant message (cleaned, bounded). The one place transcript text is read.
    pub last_line: Option<String>,
    /// The name of the folder the session works in (not the full path).
    pub folder: Option<String>,
    /// `"session"`, or `"subagent"` for a Claude subagent transcript.
    pub kind: &'static str,
    /// Opaque id of the parent session when it exists on disk; `null` otherwise.
    pub parent: Option<String>,
}

/// What is read from the end of a transcript, kept until the file changes.
#[derive(Clone)]
struct Detail {
    modified_ms: u64,
    bytes: u64,
    peek: Peek,
    folder: Option<String>,
}

/// A tool as the map shows it: name and counts only.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct MapTool {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
}
#[derive(Serialize, Clone)]
pub struct MapSession {
    pub id: String,
    pub open_calls: usize,
    /// Names of the tools running right now (empty when the session has gone quiet, so a stale call never shows as live).
    pub open_tools: Vec<String>,
    pub tools: Vec<MapTool>,
}
/// `report` reports to `manager`; both are opaque source ids of sessions in `sessions`.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct MapReport {
    pub manager: String,
    pub report: String,
}
#[derive(Serialize, Clone)]
pub struct MapView {
    pub generated_ms: u64,
    pub sessions: Vec<MapSession>,
    pub reports: Vec<MapReport>,
}

/// Observers behind the map, and where the next round-robin batch starts.
#[derive(Default)]
struct MapCache {
    observers: HashMap<String, Observer>,
    next: usize,
}

pub struct Inspector {
    sources: Mutex<HashMap<String, Found>>,
    /// Kept apart from `observers` (which serves the open details pane) so the map never evicts what the pane is following.
    map: Mutex<MapCache>,
    /// Transcripts linked to launched agents: kept apart from the discovered list (which is rebuilt from the
    /// newest files) so a linked agent never drops out of it.
    pinned: Mutex<HashMap<String, Found>>,
    details: Mutex<HashMap<PathBuf, Detail>>,
    observers: Mutex<HashMap<String, Observer>>,
}

fn agent_name(k: AgentKind) -> &'static str {
    match k {
        AgentKind::Claude => "claude",
        AgentKind::Pi => "pi",
        AgentKind::Codex => "codex",
    }
}

fn id_of(path: &std::path::Path) -> String {
    let d = Sha256::digest(path.to_string_lossy().as_bytes());
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// The parent session of a subagent transcript if its first line names the session whose folder it sits in; `None` for any file
/// that is not a clear subagent (so a stray file in a `subagents/` folder is not listed as one).
fn subagent_parent(f: &Found) -> Option<PathBuf> {
    let meta = link::subagent_meta(&f.path)?;
    let parent = f.parent.as_ref()?;
    (parent.file_stem()?.to_str()? == meta.parent_id).then(|| parent.clone())
}

/// Which sources to poll for the map this call: every active one first (so work in progress is always current), then the
/// round-robin batch fills the remaining places so quiet sources are still visited in turn. Returns the indices and where the
/// round-robin continues. Always leaves a few places for the round-robin, so a crowd of active sources cannot starve the rest.
fn poll_plan(active: &[bool], start: usize, max: usize) -> (Vec<usize>, usize) {
    let len = active.len();
    if len == 0 {
        return (vec![], 0);
    }
    let reserve = (max / 4).max(1).min(max);
    let mut chosen: Vec<usize> = (0..len).filter(|i| active[*i]).take(max - reserve).collect();
    let mut at = start % len;
    let mut visited = 0;
    while chosen.len() < max && visited < len {
        if !chosen.contains(&at) {
            chosen.push(at);
        }
        at = (at + 1) % len;
        visited += 1;
    }
    (chosen, at)
}

/// Claude session uuid (the transcript's file stem) to opaque source id, for sessions only: a subagent shares its parent's session id.
fn uuid_index(listed: &[(String, Found)]) -> HashMap<String, String> {
    listed
        .iter()
        .filter(|(_, f)| f.kind == AgentKind::Claude && f.parent.is_none())
        .filter_map(|(id, f)| f.path.file_stem().and_then(|s| s.to_str()).map(|uuid| (uuid.to_owned(), id.clone())))
        .collect()
}

/// Roster pairs `(manager uuid, report uuid)` as opaque ids; a pair where either session is not listed is noise and is dropped.
fn translate_reports(pairs: &[(String, String)], by_uuid: &HashMap<String, String>) -> Vec<MapReport> {
    pairs
        .iter()
        .filter_map(|(manager, report)| Some(MapReport { manager: by_uuid.get(manager)?.clone(), report: by_uuid.get(report)?.clone() }))
        .collect()
}

/// A session file as a source, when it exists on disk (never a path that was not verified).
fn parent_found(path: &Path, label: &str) -> Option<Found> {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let modified_ms = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok())?;
    let label = if label.is_empty() { path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { label.to_owned() };
    Some(Found { kind: AgentKind::Claude, path: path.to_path_buf(), modified_ms, bytes: meta.len(), label, parent: None })
}

/// Sessions and subagents as one list, newest first, each file once. A listed subagent whose parent session is readable but not
/// among the newest sessions brings that parent in (at most `max_parent_adds` of them) so it is never shown without it.
/// `lookup` returns a session file as a source when it exists on disk.
fn merge_sources(sessions: Vec<Found>, subagents: Vec<Found>, max_parent_adds: usize, lookup: &dyn Fn(&Path) -> Option<Found>) -> Vec<Found> {
    let mut seen: std::collections::HashSet<PathBuf> = sessions.iter().map(|f| f.path.clone()).collect();
    let mut all = sessions;
    let mut added = 0;
    for sub in &subagents {
        if let Some(parent) = &sub.parent {
            if added < max_parent_adds && !seen.contains(parent) {
                if let Some(found) = lookup(parent) {
                    seen.insert(parent.clone());
                    all.push(found);
                    added += 1;
                }
            }
        }
    }
    for sub in subagents {
        if seen.insert(sub.path.clone()) {
            all.push(sub);
        }
    }
    all.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then(a.path.cmp(&b.path)));
    all
}

impl Inspector {
    pub fn new() -> Arc<Inspector> {
        Arc::new(Inspector { sources: Mutex::new(HashMap::new()), map: Mutex::new(MapCache::default()), pinned: Mutex::new(HashMap::new()), details: Mutex::new(HashMap::new()), observers: Mutex::new(HashMap::new()) })
    }

    /// Register a transcript found by the agent launcher and return its opaque source id.
    pub fn pin(&self, kind: AgentKind, path: &std::path::Path) -> Option<String> {
        let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        let modified_ms = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
        let label = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let found = Found { kind, path: path.to_path_buf(), modified_ms, bytes: meta.len(), label, parent: None };
        let id = id_of(path);
        let mut pinned = self.pinned.lock().ok()?;
        if pinned.len() >= 64 && !pinned.contains_key(&id) {
            pinned.clear();
        }
        pinned.insert(id.clone(), found);
        Some(id)
    }

    /// Find a transcript by its opaque id among the newest `LOCATE_LIMIT` (older than the list shows), and keep it pinned.
    fn locate(&self, id: &str) -> Option<Found> {
        let home = home()?;
        let found = discover::discover(&home, LOCATE_LIMIT)
            .into_iter()
            .chain(discover::discover_subagents(&home, LOCATE_LIMIT))
            .find(|f| id_of(&f.path) == id)?;
        self.pinned.lock().ok()?.insert(id.to_owned(), found.clone());
        Some(found)
    }

    /// The last line, model and folder of a transcript, read again only when the file has changed.
    fn detail(&self, f: &Found) -> Detail {
        if let Ok(cache) = self.details.lock() {
            if let Some(d) = cache.get(&f.path).filter(|d| d.modified_ms == f.modified_ms && d.bytes == f.bytes) {
                return d.clone();
            }
        }
        let folder = link::session_ref(f.kind, &f.path)
            .and_then(|r| r.cwd.trim_end_matches('\\').rsplit('\\').next().map(|n| n.chars().take(80).collect::<String>()))
            .filter(|n| !n.is_empty() && !n.ends_with(':'));
        let detail = Detail { modified_ms: f.modified_ms, bytes: f.bytes, peek: peek(f.kind, &f.path), folder };
        if let Ok(mut cache) = self.details.lock() {
            if cache.len() >= MAX_DETAILS {
                cache.clear();
            }
            cache.insert(f.path.clone(), detail.clone());
        }
        detail
    }

    pub fn list(&self) -> Result<Vec<SourceView>, AppError> {
        let home = home().ok_or_else(|| error("inspector_unavailable", "The home folder could not be determined."))?;
        let now = discover::now_ms();
        // A file in a `subagents/` folder counts only when its first line says it is a sidechain of that very session.
        let subagents: Vec<Found> = discover::discover_subagents(&home, MAX_SUBAGENTS)
            .into_iter()
            .filter(|f| subagent_parent(f).is_some())
            .collect();
        let found = merge_sources(discover::discover(&home, MAX_SOURCES), subagents, MAX_PARENT_ADDS, &|p| parent_found(p, ""));
        let running = crate::claude_ps::running(false);
        let mut map = self.sources.lock().map_err(|_| error("inspector_unavailable", "Inspector state is unavailable."))?;
        map.clear();
        let views = found
            .into_iter()
            .map(|f| {
                let id = id_of(&f.path);
                let subagent = f.parent.is_some();
                // A subagent's `sessionId` is its parent's, so Claude's process list would answer for the parent: no `running`.
                let live = if f.kind == AgentKind::Claude && !subagent { f.path.file_stem().and_then(|s| s.to_str()).and_then(|id| running.get(id)) } else { None };
                let parent_source = f.parent.as_deref().and_then(|p| parent_found(p, &f.label));
                let detail = self.detail(&f);
                let folder = match &parent_source {
                    Some(p) if subagent => self.detail(p).folder,
                    _ => detail.folder,
                };
                let view = SourceView {
                    id: id.clone(),
                    agent: agent_name(f.kind),
                    label: f.label.clone(),
                    modified_ms: f.modified_ms,
                    bytes: f.bytes,
                    active: now.saturating_sub(f.modified_ms) <= ACTIVE_WITHIN_MS,
                    running: live.map(|l| if matches!(l, ClaudeLive::Background { .. }) { "background" } else { "interactive" }),
                    model: detail.peek.model,
                    last_line: detail.peek.last_line,
                    folder,
                    kind: if subagent { "subagent" } else { "session" },
                    parent: parent_source.map(|p| id_of(&p.path)),
                };
                map.insert(id, f);
                view
            })
            .collect();
        Ok(views)
    }

    /// The transcript behind an opaque source id: among the newest listed, the pinned ones, or further back.
    pub fn find(&self, id: &str) -> Result<Found, AppError> {
        if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("inspector_failed", "Invalid source id."));
        }
        let found = {
            let map = self.sources.lock().map_err(|_| error("inspector_unavailable", "Inspector state is unavailable."))?;
            let pinned = self.pinned.lock().map_err(|_| error("inspector_unavailable", "Inspector state is unavailable."))?;
            map.get(id).or_else(|| pinned.get(id)).cloned()
        };
        match found {
            Some(f) => Ok(f),
            None => {
                self.list()?;
                let listed = self.sources.lock().map_err(|_| error("inspector_unavailable", "Inspector state is unavailable."))?.get(id).cloned();
                match listed {
                    Some(found) => Ok(found),
                    // A watched session that is no longer among the newest: look further back before giving up.
                    None => self.locate(id).ok_or_else(|| error("inspector_failed", "That transcript is no longer available.")),
                }
            }
        }
    }

    /// Tool facts per listed source and the roster edges between sessions, for the agent map. Reads no new kind of data: the same
    /// names and counts the details pane shows, from incremental tails of at most `MAX_POLLS_PER_REQUEST` transcripts per call
    /// (round-robin, so a cold start converges), plus the roster files. Nothing is written or logged.
    pub fn map(&self) -> Result<MapView, AppError> {
        let unavailable = || error("inspector_unavailable", "Inspector state is unavailable.");
        let mut listed: Vec<(String, Found)> = self.sources.lock().map_err(|_| unavailable())?.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if listed.is_empty() {
            self.list()?;
            listed = self.sources.lock().map_err(|_| unavailable())?.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        listed.sort_by(|a, b| b.1.modified_ms.cmp(&a.1.modified_ms).then(a.0.cmp(&b.0)));
        let now = discover::now_ms();
        let mut cache = self.map.lock().map_err(|_| unavailable())?;
        let live: std::collections::HashSet<&str> = listed.iter().map(|(id, _)| id.as_str()).collect();
        cache.observers.retain(|id, _| live.contains(id.as_str()));
        let active: Vec<bool> = listed.iter().map(|(_, f)| now.saturating_sub(f.modified_ms) <= ACTIVE_WITHIN_MS).collect();
        let (batch, next) = poll_plan(&active, cache.next, MAX_MAP_POLLS);
        cache.next = next;
        for i in batch {
            let Some((id, found)) = listed.get(i) else { continue };
            if !cache.observers.contains_key(id) && cache.observers.len() >= MAX_MAP_OBSERVERS {
                continue;
            }
            // A transcript that cannot be read right now keeps whatever it showed last time.
            let _ = cache.observers.entry(id.clone()).or_insert_with(|| Observer::new(found.kind, &found.path)).poll();
        }
        let sessions: Vec<MapSession> = listed
            .iter()
            .map(|(id, _)| match cache.observers.get(id) {
                Some(obs) => {
                    let snap = snapshot(&obs.index, now, IDLE_AFTER_MS);
                    let running = matches!(snap.state, "working" | "waiting_for_human");
                    MapSession {
                        id: id.clone(),
                        open_calls: snap.open_calls,
                        open_tools: if running { obs.index.open_tool_names(MAX_OPEN_TOOLS) } else { vec![] },
                        tools: snap.tools.iter().map(|t| MapTool { name: t.name.clone(), calls: t.calls, errors: t.errors }).collect(),
                    }
                }
                None => MapSession { id: id.clone(), open_calls: 0, open_tools: vec![], tools: vec![] },
            })
            .collect();
        drop(cache);
        // Roster ids are session UUIDs; they are matched to opaque ids through the discovered file paths, never by string-matching ids.
        let by_uuid = uuid_index(&listed);
        let pairs = home().map(|h| map::rosters(&h.join(".claude").join("agent-roster"))).unwrap_or_default();
        Ok(MapView { generated_ms: now, sessions, reports: translate_reports(&pairs, &by_uuid) })
    }

    /// The end-of-transcript look for a source: the model and the one-line task, no snapshot or path.
    pub fn peek_of(&self, id: &str) -> Result<Peek, AppError> {
        let found = self.find(id)?;
        Ok(self.detail(&found).peek)
    }

    pub fn snapshot(&self, id: &str) -> Result<serde_json::Value, AppError> {
        let found = self.find(id)?;
        let mut observers = self.observers.lock().map_err(|_| error("inspector_unavailable", "Inspector state is unavailable."))?;
        if observers.len() >= MAX_OBSERVERS && !observers.contains_key(id) {
            observers.clear();
        }
        let obs = observers.entry(id.to_owned()).or_insert_with(|| Observer::new(found.kind, &found.path));
        for _ in 0..MAX_POLLS_PER_REQUEST {
            match obs.poll() {
                Ok(r) if r.more => continue,
                Ok(_) => break,
                Err(_) => return Err(error("inspector_failed", "The transcript could not be read.")),
            }
        }
        let now = discover::now_ms();
        let snap = snapshot(&obs.index, now, IDLE_AFTER_MS);
        drop(observers);
        let detail = self.detail(&found);
        Ok(serde_json::json!({
            "peek": { "last_line": detail.peek.last_line, "model": detail.peek.model, "task": detail.peek.task },
            "source": { "id": id, "agent": agent_name(found.kind), "label": found.label, "modified_ms": found.modified_ms, "bytes": found.bytes },
            "now_ms": now,
            "snapshot": snap,
        }))
    }
}

async fn run<T: Send + 'static>(i: tauri::State<'_, Arc<Inspector>>, f: impl FnOnce(&Inspector) -> Result<T, AppError> + Send + 'static) -> Result<T, AppError> {
    let i = i.inner().clone();
    tauri::async_runtime::spawn_blocking(move || f(&i)).await.map_err(|_| error("inspector_failed", "The request was interrupted."))?
}

#[tauri::command]
pub async fn inspector_sources(i: tauri::State<'_, Arc<Inspector>>) -> Result<Vec<SourceView>, AppError> {
    run(i, |i| i.list()).await
}

fn tags_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    app.path().app_data_dir().map(|d| d.join("inspector-tags.json")).map_err(|_| error("inspector_failed", "The app data folder is unavailable."))
}

/// The colour tags the user put on sessions (opaque source id -> palette colour).
#[tauri::command]
pub async fn inspector_tags_load(app: tauri::AppHandle) -> Result<agent_workbench::inspector_tags::Tags, AppError> {
    let path = tags_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || agent_workbench::inspector_tags::load(&path)).await.map_err(|_| error("inspector_failed", "The request was interrupted."))
}

#[tauri::command]
pub async fn inspector_tags_save(app: tauri::AppHandle, tags: agent_workbench::inspector_tags::Tags) -> Result<agent_workbench::inspector_tags::Tags, AppError> {
    if !tags.is_valid() {
        return Err(error("inspector_failed", "Those tags are not valid."));
    }
    let path = tags_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || agent_workbench::inspector_tags::save(&path, &tags).map(|()| tags))
        .await
        .map_err(|_| error("inspector_failed", "The request was interrupted."))?
        .map_err(|_| error("inspector_failed", "The tags could not be saved."))
}

#[tauri::command]
pub async fn inspector_map(i: tauri::State<'_, Arc<Inspector>>) -> Result<MapView, AppError> {
    run(i, |i| i.map()).await
}

#[tauri::command]
pub async fn inspector_snapshot(i: tauri::State<'_, Arc<Inspector>>, id: String) -> Result<serde_json::Value, AppError> {
    run(i, move |i| i.snapshot(&id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home(name: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("wb-inspector-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&h);
        h
    }
    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"{}\n").unwrap();
    }
    fn lookup(p: &Path) -> Option<Found> {
        parent_found(p, "")
    }
    fn ids(list: &[Found]) -> Vec<String> {
        list.iter().map(|f| id_of(&f.path)).collect()
    }

    #[test]
    fn a_fan_out_of_subagents_is_capped_on_its_own_and_the_parent_appears_once() {
        let h = home("fanout");
        let parent = h.join(".claude/projects/proj/sess1.jsonl");
        touch(&parent);
        for n in 0..30 {
            touch(&h.join(format!(".claude/projects/proj/sess1/subagents/agent-{n:02}.jsonl")));
        }
        for n in 0..5 {
            touch(&h.join(format!(".claude/projects/proj/other{n}.jsonl")));
        }
        let merged = merge_sources(discover::discover(&h, MAX_SOURCES), discover::discover_subagents(&h, MAX_SUBAGENTS), MAX_PARENT_ADDS, &lookup);
        assert_eq!(merged.iter().filter(|f| f.parent.is_some()).count(), MAX_SUBAGENTS, "20 of the 30 subagents");
        assert_eq!(merged.iter().filter(|f| f.path == parent).count(), 1, "the parent is present once");
        assert_eq!(merged.len(), MAX_SUBAGENTS + 6, "the 6 sessions are all still there");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn a_subagent_whose_parent_file_is_missing_is_listed_and_has_no_parent() {
        let h = home("orphan");
        touch(&h.join(".claude/projects/proj/gone/subagents/agent-a.jsonl"));
        let subs = discover::discover_subagents(&h, MAX_SUBAGENTS);
        let subs_path = subs[0].path.clone();
        let parent = subs[0].parent.clone().unwrap();
        assert!(lookup(&parent).is_none(), "the parent file does not exist, so no id is made for it");
        let merged = merge_sources(vec![], subs, MAX_PARENT_ADDS, &lookup);
        assert_eq!(ids(&merged), vec![id_of(&subs_path)], "listed, and nothing added");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn at_most_ten_missing_parents_are_added() {
        let h = home("caps");
        for n in 0..11 {
            touch(&h.join(format!(".claude/projects/proj/sess{n:02}.jsonl")));
            touch(&h.join(format!(".claude/projects/proj/sess{n:02}/subagents/agent-x.jsonl")));
        }
        // The session list given to the merge holds none of the parents (as when they are older than the newest 60).
        let merged = merge_sources(vec![], discover::discover_subagents(&h, MAX_SUBAGENTS), MAX_PARENT_ADDS, &lookup);
        assert_eq!(merged.iter().filter(|f| f.parent.is_none()).count(), MAX_PARENT_ADDS, "11 missing parents -> 10 added");
        assert_eq!(merged.iter().filter(|f| f.parent.is_some()).count(), 11, "every subagent is still listed");
        let _ = fs::remove_dir_all(&h);
    }

    /// Manual: `cargo test real_home -- --ignored --nocapture`. Counts only; no transcript text is printed.
    #[test]
    #[ignore = "reads the real home folder"]
    fn real_home_lists_subagents_and_builds_the_map() {
        let inspector = Inspector::new();
        let list = inspector.list().unwrap();
        if let Some(path) = std::env::var_os("WB_DUMP_INSPECTOR") {
            // For checking the real payloads against the frontend decoders (node). Written where the caller says; nothing else is kept.
            let dir = PathBuf::from(path);
            std::fs::write(dir.join("sources.json"), serde_json::to_string(&list).unwrap()).unwrap();
            std::fs::write(dir.join("map.json"), serde_json::to_string(&inspector.map().unwrap()).unwrap()).unwrap();
        }
        let subs: Vec<_> = list.iter().filter(|v| v.kind == "subagent").collect();
        let linked = subs.iter().filter(|v| v.parent.is_some()).count();
        println!("sources {} (subagents {}, with a parent {linked})", list.len(), subs.len());
        let mut last = None;
        for _ in 0..16 {
            last = Some(inspector.map().unwrap());
        }
        let view = last.unwrap();
        let with_tools = view.sessions.iter().filter(|s| !s.tools.is_empty()).count();
        println!("map sessions {} (with tools {with_tools}), reports {}", view.sessions.len(), view.reports.len());
        assert!(view.sessions.len() == list.len());
        assert!(view.reports.iter().all(|r| view.sessions.iter().any(|s| s.id == r.manager) && view.sessions.iter().any(|s| s.id == r.report)));
    }

    #[test]
    fn the_poll_plan_always_includes_active_sources_and_still_visits_the_rest_in_turn() {
        assert_eq!(poll_plan(&[], 3, 16), (vec![], 0));
        // 10 sources, 2 active: both active ones come first, then round-robin fills up to the maximum
        let mut active = vec![false; 10];
        active[7] = true;
        active[9] = true;
        let (plan, next) = poll_plan(&active, 0, 6);
        assert_eq!(&plan[..2], &[7, 9]);
        assert_eq!(plan.len(), 6);
        assert_eq!(plan.iter().collect::<std::collections::BTreeSet<_>>().len(), 6, "no source twice");
        assert_eq!(next, 4, "the round-robin moved on past the four it added");
        // every source is reached within a few calls even when many are active
        let crowd = vec![true; 40];
        let (mut seen, mut start) = (std::collections::BTreeSet::new(), 0);
        for _ in 0..40 {
            let (plan, n) = poll_plan(&crowd, start, 16);
            assert!(plan.len() <= 16);
            seen.extend(plan);
            start = n;
        }
        assert_eq!(seen.len(), 40);
        // a short list is covered at once
        assert_eq!(poll_plan(&[false, false, false], 0, 16).0.len(), 3);
    }

    #[test]
    fn roster_pairs_are_matched_to_listed_sessions_by_their_file_stem_and_unlisted_ones_are_dropped() {
        let session = |id: &str, stem: &str, parent: Option<&str>| {
            (id.to_string(), Found { kind: AgentKind::Claude, path: PathBuf::from(format!("C:/p/proj/{stem}.jsonl")), modified_ms: 1, bytes: 1, label: "proj".into(), parent: parent.map(PathBuf::from) })
        };
        let listed = vec![session("aaaaaaaaaaaaaaaa", "uuid-boss", None), session("bbbbbbbbbbbbbbbb", "uuid-worker", None), session("cccccccccccccccc", "agent-1", Some("C:/p/proj/uuid-boss.jsonl"))];
        let index = uuid_index(&listed);
        assert_eq!(index.len(), 2, "a subagent is not a session");
        let pairs = vec![("uuid-boss".to_string(), "uuid-worker".to_string()), ("uuid-boss".to_string(), "uuid-unlisted".to_string()), ("agent-1".to_string(), "uuid-worker".to_string())];
        assert_eq!(translate_reports(&pairs, &index), vec![MapReport { manager: "aaaaaaaaaaaaaaaa".into(), report: "bbbbbbbbbbbbbbbb".into() }]);
    }

    #[test]
    fn ids_are_stable_across_two_merges_of_unchanged_files() {
        let h = home("stable");
        touch(&h.join(".claude/projects/proj/sess1.jsonl"));
        for n in 0..3 {
            touch(&h.join(format!(".claude/projects/proj/sess1/subagents/agent-{n}.jsonl")));
        }
        let once = || merge_sources(discover::discover(&h, MAX_SOURCES), discover::discover_subagents(&h, MAX_SUBAGENTS), MAX_PARENT_ADDS, &lookup);
        let (a, b) = (ids(&once()), ids(&once()));
        assert_eq!(a, b);
        assert_eq!(a.len(), 4);
        assert!(a.iter().all(|id| id.len() == 16 && id.bytes().all(|c| c.is_ascii_hexdigit())));
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn only_a_file_whose_first_line_names_its_own_session_counts_as_a_subagent() {
        let h = home("meta");
        let dir = h.join(".claude/projects/proj/sess1/subagents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-ok.jsonl"), "{\"isSidechain\":true,\"agentId\":\"ok\",\"sessionId\":\"sess1\",\"message\":{}}\n").unwrap();
        fs::write(dir.join("agent-other.jsonl"), "{\"isSidechain\":true,\"agentId\":\"o\",\"sessionId\":\"sess2\"}\n").unwrap();
        fs::write(dir.join("agent-plain.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        let valid: Vec<String> = discover::discover_subagents(&h, 10)
            .into_iter()
            .filter(|f| subagent_parent(f).is_some())
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(valid, vec!["agent-ok.jsonl".to_string()]);
        let _ = fs::remove_dir_all(&h);
    }
}
