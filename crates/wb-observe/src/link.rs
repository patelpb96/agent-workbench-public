//! Find the transcript that belongs to an agent Workbench launched in a terminal.
//!
//! Claude Code and Pi accept a session id on the command line, so their transcript is found by that id
//! alone. Codex cannot be given an id, so its rollout file is matched by working folder and start time.
//! Read-only: directory listings, file names, modification times, and for Codex only the FIRST line of
//! each recent candidate (the session metadata: working folder and start time). Nothing else is read.
use crate::{tail::AgentKind, time::parse_iso_ms};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

/// Codex files are matched if they started at most this long before the launch was recorded
/// (clock and file-system timestamp skew).
pub const SKEW_MS: u64 = 5_000;
/// First lines longer than this are not session metadata we will look at.
const MAX_FIRST_LINE: u64 = 64 * 1024;
/// How many recent Codex candidates are inspected.
const MAX_CANDIDATES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub kind: AgentKind,
    /// The id given to Claude Code / Pi (`--session-id`); unused for Codex.
    pub session_id: String,
    /// Absolute working folder the agent was started in.
    pub cwd: String,
    pub launched_ms: u64,
}

fn dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    rd.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}
fn jsonl_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    rd.filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let ms = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
            Some((e.path(), ms))
        })
        .collect()
}

/// A session id is a UUID/ULID-style token: it ends up in a file-name comparison, so keep it strict.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Compare working folders the way Windows does: case-insensitive, either slash, no trailing slash,
/// no `\\?\` prefix.
pub fn same_folder(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().strip_prefix(r"\\?\").unwrap_or(s.trim());
        s.replace('/', "\\").trim_end_matches('\\').to_lowercase()
    };
    !a.trim().is_empty() && norm(a) == norm(b)
}

fn first_line(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(MAX_FIRST_LINE)).read_line(&mut line).ok()?;
    (line.ends_with('\n') || line.len() < MAX_FIRST_LINE as usize).then_some(line)
}

/// The parent session and agent tag of a Claude subagent transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentMeta {
    pub parent_id: String,
    pub agent_id: String,
}

/// The parent session and agent tag of a Claude subagent transcript, from line 1 only. Only the three named keys are taken
/// from it: the `message` object that follows them is skipped by the parser and never kept or looked at.
pub fn subagent_meta(path: &Path) -> Option<SubagentMeta> {
    #[derive(serde::Deserialize)]
    struct Line1 {
        #[serde(rename = "isSidechain")]
        is_sidechain: Option<bool>,
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        #[serde(rename = "agentId")]
        agent_id: Option<String>,
    }
    let line: Line1 = serde_json::from_str(&first_line(path)?).ok()?;
    let (parent_id, agent_id) = (line.session_id?, line.agent_id?);
    (line.is_sidechain == Some(true) && safe_id(&parent_id) && safe_id(&agent_id)).then_some(SubagentMeta { parent_id, agent_id })
}

/// The transcript for `launch`, or `None` while it does not exist yet (or cannot be told apart).
/// `claimed` holds transcripts already linked to other agent terminals; they are never returned.
pub fn resolve(home: &Path, launch: &Launch, claimed: &HashSet<PathBuf>) -> Option<PathBuf> {
    match launch.kind {
        AgentKind::Claude => {
            if !safe_id(&launch.session_id) {
                return None;
            }
            let wanted = format!("{}.jsonl", launch.session_id);
            dirs(&home.join(".claude").join("projects"))
                .into_iter()
                .map(|project| project.join(&wanted))
                .find(|p| p.is_file() && !claimed.contains(p))
        }
        AgentKind::Pi => {
            if !safe_id(&launch.session_id) {
                return None;
            }
            let suffix = format!("_{}", launch.session_id);
            dirs(&home.join(".pi").join("agent").join("sessions"))
                .into_iter()
                .flat_map(|project| jsonl_files(&project))
                .map(|(path, _)| path)
                .find(|p| !claimed.contains(p) && p.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(&suffix)))
        }
        // A resumed Codex session is known by id: its rollout file is `rollout-<time>-<id>.jsonl`.
        AgentKind::Codex if !launch.session_id.is_empty() => {
            if !safe_id(&launch.session_id) {
                return None;
            }
            let suffix = format!("-{}", launch.session_id);
            dirs(&home.join(".codex").join("sessions"))
                .into_iter()
                .flat_map(|year| dirs(&year))
                .flat_map(|month| dirs(&month))
                .flat_map(|day| jsonl_files(&day))
                .map(|(path, _)| path)
                .find(|p| !claimed.contains(p) && p.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(&suffix)))
        }
        AgentKind::Codex => {
            let earliest = launch.launched_ms.saturating_sub(SKEW_MS);
            let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
            for year in dirs(&home.join(".codex").join("sessions")) {
                for month in dirs(&year) {
                    for day in dirs(&month) {
                        // Files only grow, so a file last written before the launch cannot be ours.
                        candidates.extend(jsonl_files(&day).into_iter().filter(|(_, modified)| *modified >= earliest));
                    }
                }
            }
            candidates.sort_by_key(|(_, modified)| *modified);
            candidates.truncate(MAX_CANDIDATES);
            let mut best: Option<(u64, PathBuf)> = None;
            for (path, _) in candidates {
                if claimed.contains(&path) {
                    continue;
                }
                let Some(line) = first_line(&path) else { continue };
                let Ok(meta) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                if meta.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
                    continue;
                }
                let payload = meta.get("payload")?;
                let started = payload.get("timestamp").and_then(|t| t.as_str()).and_then(parse_iso_ms);
                let cwd = payload.get("cwd").and_then(|c| c.as_str());
                let (Some(started), Some(cwd)) = (started, cwd) else { continue };
                if started < earliest || !same_folder(cwd, &launch.cwd) {
                    continue;
                }
                // The earliest start after the launch wins (two agents in one folder resolve in launch order).
                if best.as_ref().map_or(true, |(b, _)| started < *b) {
                    best = Some((started, path));
                }
            }
            best.map(|(_, path)| path)
        }
    }
}

/// Where an existing session can be resumed: its own id and the folder it ran in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub id: String,
    pub cwd: String,
}

/// How much of a transcript is searched for its working folder: the start, and the end (long Claude Code
/// sessions only record the folder once they are underway).
const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 512 * 1024;

fn cwds_in(reader: impl BufRead, out: &mut Vec<String>) {
    for line in reader.lines().map_while(Result::ok) {
        // A line cut short by the byte limit does not parse and is skipped.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
            if !out.iter().any(|c| c == cwd) {
                out.push(cwd.to_owned());
            }
        }
    }
}

/// Claude Code names a project folder after its working folder with every character that is not a letter
/// or digit replaced by `-`. Used to tell which recorded folder the session really belongs to.
pub fn claude_project_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The id and working folder of an existing session, read from its transcript: the first line for Pi and
/// Codex, a bounded search at the start and end for Claude Code. Read-only; nothing else is looked at.
pub fn session_ref(kind: AgentKind, path: &Path) -> Option<SessionRef> {
    match kind {
        AgentKind::Pi | AgentKind::Codex => {
            let meta: serde_json::Value = serde_json::from_str(&first_line(path)?).ok()?;
            let (id, cwd) = if kind == AgentKind::Pi {
                if meta.get("type")?.as_str()? != "session" {
                    return None;
                }
                (meta.get("id")?.as_str()?, meta.get("cwd")?.as_str()?)
            } else {
                if meta.get("type")?.as_str()? != "session_meta" {
                    return None;
                }
                let payload = meta.get("payload")?;
                (payload.get("id")?.as_str()?, payload.get("cwd")?.as_str()?)
            };
            safe_id(id).then(|| SessionRef { id: id.to_owned(), cwd: cwd.to_owned() })
        }
        AgentKind::Claude => {
            let id = path.file_stem()?.to_str()?;
            if !safe_id(id) {
                return None;
            }
            let project = path.parent()?.file_name()?.to_str()?;
            let mut file = fs::File::open(path).ok()?;
            let len = file.metadata().ok()?.len();
            let mut found = Vec::new();
            cwds_in(BufReader::new((&file).take(HEAD_BYTES)), &mut found);
            if len > HEAD_BYTES {
                use std::io::{Seek, SeekFrom};
                let from = len.saturating_sub(TAIL_BYTES).max(HEAD_BYTES);
                file.seek(SeekFrom::Start(from)).ok()?;
                let mut tail = BufReader::new((&file).take(TAIL_BYTES));
                let mut partial = String::new();
                let _ = tail.read_line(&mut partial); // starts mid-line
                cwds_in(tail, &mut found);
            }
            // The folder the session started in names its project; later entries may be in a subfolder of it
            // (the agent changed directory), so a parent of a recorded folder counts as well.
            for cwd in &found {
                let mut dir = Some(Path::new(cwd.as_str()));
                while let Some(d) = dir {
                    if let Some(text) = d.to_str().filter(|t| claude_project_name(t).eq_ignore_ascii_case(project)) {
                        return Some(SessionRef { id: id.to_owned(), cwd: text.to_owned() });
                    }
                    dir = d.parent();
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("wb-link-{}", std::process::id() as u64 * 1_000_000 + rand_suffix()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }
    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    fn launch(kind: AgentKind, id: &str, cwd: &str, at: u64) -> Launch {
        Launch { kind, session_id: id.into(), cwd: cwd.into(), launched_ms: at }
    }
    const NOW: u64 = 1_792_000_000_000;
    fn meta(started_iso: &str, cwd: &str) -> String {
        format!("{{\"timestamp\":\"{started_iso}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"x\",\"timestamp\":\"{started_iso}\",\"cwd\":{}}}}}\n", serde_json::to_string(cwd).unwrap())
    }

    #[test]
    fn claude_is_found_by_its_session_id_alone_and_only_once() {
        let h = home();
        let id = "11111111-2222-4333-8444-555555555555";
        assert_eq!(resolve(&h, &launch(AgentKind::Claude, id, r"C:\x", NOW), &HashSet::new()), None, "not there yet");
        let path = h.join(".claude/projects/C--x/11111111-2222-4333-8444-555555555555.jsonl");
        write(&path, "{}\n");
        write(&h.join(".claude/projects/C--x/99999999-2222-4333-8444-555555555555.jsonl"), "{}\n");
        assert_eq!(resolve(&h, &launch(AgentKind::Claude, id, r"C:\x", NOW), &HashSet::new()), Some(path.clone()));
        assert_eq!(resolve(&h, &launch(AgentKind::Claude, id, r"C:\x", NOW), &HashSet::from([path])), None, "claimed by another terminal");
        for bad in ["", "../escape", "a/b", "a\\b", &"x".repeat(65)] {
            assert_eq!(resolve(&h, &launch(AgentKind::Claude, bad, r"C:\x", NOW), &HashSet::new()), None, "{bad:?}");
        }
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn pi_is_found_by_the_id_in_its_file_name() {
        let h = home();
        let path = h.join(".pi/agent/sessions/--C--x--/2026-10-22T04-17-18-303Z_abcd-1234.jsonl");
        write(&path, "{}\n");
        write(&h.join(".pi/agent/sessions/--C--x--/2026-10-22T04-17-18-303Z_zzzz-9999.jsonl"), "{}\n");
        assert_eq!(resolve(&h, &launch(AgentKind::Pi, "abcd-1234", r"C:\x", NOW), &HashSet::new()), Some(path));
        assert_eq!(resolve(&h, &launch(AgentKind::Pi, "1234", r"C:\x", NOW), &HashSet::new()), None, "a partial id never matches");
        assert_eq!(resolve(&h, &launch(AgentKind::Pi, "nope", r"C:\x", NOW), &HashSet::new()), None);
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn codex_is_matched_by_folder_and_start_time_earliest_after_launch() {
        let h = home();
        let day = h.join(".codex/sessions/2026/10/22");
        let launched = parse_iso_ms("2026-10-22T10:00:00.000Z").unwrap();
        let l = launch(AgentKind::Codex, "", r"C:\Users\USERPROFILE\proj", launched);
        write(&day.join("rollout-a.jsonl"), &meta("2026-10-22T10:00:03.000Z", r"C:\Users\USERPROFILE\proj"));
        write(&day.join("rollout-b.jsonl"), &meta("2026-10-22T10:00:09.000Z", r"C:\Users\USERPROFILE\proj"));
        write(&day.join("rollout-other-folder.jsonl"), &meta("2026-10-22T10:00:01.000Z", r"C:\Users\USERPROFILE\elsewhere"));
        write(&day.join("rollout-before.jsonl"), &meta("2026-10-22T09:00:00.000Z", r"C:\Users\USERPROFILE\proj"));
        write(&day.join("rollout-garbage.jsonl"), "not json\n");
        write(&day.join("rollout-not-meta.jsonl"), "{\"type\":\"event_msg\"}\n");
        let a = day.join("rollout-a.jsonl");
        // Files written before the launch cannot be ours, so make every candidate look recently written.
        let touch = |p: &Path| { let f = fs::OpenOptions::new().write(true).open(p).unwrap(); f.set_modified(std::time::SystemTime::now() + Duration::from_secs(1)).unwrap(); };
        for e in fs::read_dir(&day).unwrap() { touch(&e.unwrap().path()); }
        // `launched` is in the past relative to "now" mtimes, which is all the mtime filter needs.
        assert_eq!(resolve(&h, &l, &HashSet::new()), Some(a.clone()), "earliest matching start wins");
        assert_eq!(resolve(&h, &l, &HashSet::from([a.clone()])), Some(day.join("rollout-b.jsonl")), "a second agent in the same folder gets the next one");
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "", r"C:\Users\USERPROFILE\PROJ\", launched), &HashSet::new()), Some(a), "folder compare ignores case and a trailing slash");
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "", r"C:\nothing", launched), &HashSet::new()), None);
        // A file started well before the launch is not matched even if it is in the right folder.
        let late = launch(AgentKind::Codex, "", r"C:\Users\USERPROFILE\proj", parse_iso_ms("2026-10-22T10:05:00.000Z").unwrap());
        assert_eq!(resolve(&h, &late, &HashSet::new()), None);
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn folders_compare_like_windows_paths() {
        assert!(same_folder(r"C:\Users\USERPROFILE", "c:/users/USERPROFILE/"));
        assert!(same_folder(r"\\?\C:\Users\USERPROFILE", r"C:\Users\USERPROFILE"));
        assert!(!same_folder(r"C:\Users\USERPROFILE", r"C:\Users\USERPROFILE2"));
        assert!(!same_folder("", ""));
    }

    #[test]
    fn an_oversized_first_line_is_ignored_not_read() {
        let h = home();
        let day = h.join(".codex/sessions/2026/10/22");
        write(&day.join("rollout-big.jsonl"), &format!("{}\n", "x".repeat(200_000)));
        let l = launch(AgentKind::Codex, "", r"C:\p", 1);
        assert_eq!(resolve(&h, &l, &HashSet::new()), None);
        let _ = fs::remove_dir_all(&h);
    }

    fn cwd_line(kind: &str, id: &str, cwd: &str) -> String {
        format!("{}\n", serde_json::json!({"type": kind, "id": id, "cwd": cwd}))
    }
    fn user_line(cwd: &str) -> String {
        format!("{}\n", serde_json::json!({"type": "user", "cwd": cwd}))
    }

    #[test]
    fn pi_and_codex_sessions_give_their_id_and_folder_from_the_first_line() {
        let h = home();
        let pi = h.join(".pi/agent/sessions/--C--x--/2026-10-22T04-17-18-303Z_abcd-1234.jsonl");
        write(&pi, &(cwd_line("session", "abcd-1234", r"C:\Users\USERPROFILE\proj") + "{\"type\":\"message\"}\n"));
        assert_eq!(session_ref(AgentKind::Pi, &pi), Some(SessionRef { id: "abcd-1234".into(), cwd: r"C:\Users\USERPROFILE\proj".into() }));
        let codex = h.join(".codex/sessions/2026/10/22/rollout-2026-10-22T12-00-00-0a1b2c3d-4e5f.jsonl");
        write(&codex, &format!("{}\n", serde_json::json!({"type": "session_meta", "payload": {"id": "0a1b2c3d-4e5f", "cwd": r"C:\fixture\system"}})));
        assert_eq!(session_ref(AgentKind::Codex, &codex), Some(SessionRef { id: "0a1b2c3d-4e5f".into(), cwd: r"C:\fixture\system".into() }));
        let odd = h.join(".pi/agent/sessions/--C--x--/odd.jsonl");
        for body in ["not json\n".to_string(), "{\"type\":\"model_change\"}\n".to_string(), cwd_line("session", "../x", r"C:\p"), "{\"type\":\"session\",\"id\":\"ok\"}\n".to_string()] {
            write(&odd, &body);
            assert_eq!(session_ref(AgentKind::Pi, &odd), None, "{body:?}");
        }
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn claude_sessions_give_their_id_and_the_folder_that_matches_their_project_name() {
        let h = home();
        let id = "11111111-2222-4333-8444-555555555555";
        let path = h.join(format!(".claude/projects/C--Users-USERPROFILE-my-proj/{id}.jsonl"));
        // The folder shows up at the start.
        write(&path, &("{\"type\":\"mode\",\"sessionId\":\"x\"}\n".to_string() + &user_line(r"C:\Users\USERPROFILE\my proj")));
        assert_eq!(session_ref(AgentKind::Claude, &path), Some(SessionRef { id: id.into(), cwd: r"C:\Users\USERPROFILE\my proj".into() }));
        // Only at the end of a long file (the start is past the head limit): found there.
        let filler = format!("{{\"type\":\"attachment\",\"pad\":\"{}\"}}\n", "x".repeat(1000));
        let long = filler.repeat(400) + &user_line(r"C:\Users\USERPROFILE\my proj");
        write(&path, &long);
        assert!(fs::metadata(&path).unwrap().len() > HEAD_BYTES + 1000);
        assert_eq!(session_ref(AgentKind::Claude, &path).map(|r| r.cwd), Some(r"C:\Users\USERPROFILE\my proj".into()));
        // Recorded only in a subfolder (the agent changed directory): the project's own folder is the parent.
        write(&path, &user_line(r"C:\Users\USERPROFILE\my proj\src\deep"));
        assert_eq!(session_ref(AgentKind::Claude, &path).map(|r| r.cwd), Some(r"C:\Users\USERPROFILE\my proj".into()));
        // A recorded folder that does not belong to this project folder is not trusted.
        write(&path, &user_line(r"C:\somewhere\else"));
        assert_eq!(session_ref(AgentKind::Claude, &path), None);
        // A file name that is not a safe id is refused.
        let bad = h.join(".claude/projects/C--Users-USERPROFILE-my-proj/a b.jsonl");
        write(&bad, &user_line(r"C:\Users\USERPROFILE\my proj"));
        assert_eq!(session_ref(AgentKind::Claude, &bad), None);
        assert_eq!(claude_project_name(r"C:\Users\USERPROFILE\my.proj"), "C--Users-USERPROFILE-my-proj");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn a_resumed_codex_session_is_linked_by_its_id_not_by_start_time() {
        let h = home();
        let day = h.join(".codex/sessions/2026/10/01");
        let old = day.join("rollout-2026-10-01T09-00-00-0a1b2c3d-4e5f.jsonl");
        write(&old, "{}\n");
        write(&day.join("rollout-2026-10-01T09-00-00-ffff-0000.jsonl"), "{}\n");
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "0a1b2c3d-4e5f", r"C:\x", NOW), &HashSet::new()), Some(old.clone()));
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "0a1b2c3d-4e5f", r"C:\x", NOW), &HashSet::from([old])), None);
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "../x", r"C:\x", NOW), &HashSet::new()), None);
        assert_eq!(resolve(&h, &launch(AgentKind::Codex, "1234", r"C:\x", NOW), &HashSet::new()), None, "a partial id never matches");
        let _ = fs::remove_dir_all(&h);
    }
}
