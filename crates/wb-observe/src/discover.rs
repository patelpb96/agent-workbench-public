//! Find agent transcripts in their default locations. Read-only directory listing:
//! file names and modification times only, never contents.
use crate::tail::AgentKind;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: AgentKind,
    pub path: PathBuf,
    pub modified_ms: u64,
    pub bytes: u64,
    /// Short human label derived from the file's location (project folder or date).
    pub label: String,
    /// A Claude subagent transcript's parent session file (`<project>/<session>.jsonl`), named after the folder the subagent sits
    /// in. It may not exist on disk: the caller checks. `None` for ordinary sessions.
    pub parent: Option<PathBuf>,
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn jsonl_files(dir: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    rd.filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| (e.path(), m)))
        .collect()
}

fn dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    rd.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `home` is the user's home directory. Returns the newest `limit` transcripts across
/// Claude Code (`.claude/projects/<project>/*.jsonl`), Pi (`.pi/agent/sessions/<project>/*.jsonl`)
/// and Codex (`.codex/sessions/YYYY/MM/DD/*.jsonl`), newest first.
pub fn discover(home: &Path, limit: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for project in dirs(&home.join(".claude").join("projects")) {
        for (path, meta) in jsonl_files(&project) {
            out.push(Found { kind: AgentKind::Claude, label: name(&project), modified_ms: mtime_ms(&meta), bytes: meta.len(), path, parent: None });
        }
    }
    for project in dirs(&home.join(".pi").join("agent").join("sessions")) {
        for (path, meta) in jsonl_files(&project) {
            out.push(Found { kind: AgentKind::Pi, label: name(&project), modified_ms: mtime_ms(&meta), bytes: meta.len(), path, parent: None });
        }
    }
    for year in dirs(&home.join(".codex").join("sessions")) {
        for month in dirs(&year) {
            for day in dirs(&month) {
                let label = format!("{}-{}-{}", name(&year), name(&month), name(&day));
                for (path, meta) in jsonl_files(&day) {
                    out.push(Found { kind: AgentKind::Codex, label: label.clone(), modified_ms: mtime_ms(&meta), bytes: meta.len(), path, parent: None });
                }
            }
        }
    }
    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then(a.path.cmp(&b.path)));
    out.truncate(limit);
    out
}

/// Newest Claude subagent transcripts across all projects. Read-only directory listing.
/// `label` is the project folder name (the same label the parent session gets).
///
/// Walks `.claude/projects/<project>/<session>/subagents/*.jsonl`; `parent` is `<project>/<session>.jsonl`. Not part of
/// [`discover`], which stays sessions-only: the caller merges the two.
pub fn discover_subagents(home: &Path, limit: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for project in dirs(&home.join(".claude").join("projects")) {
        for session in dirs(&project) {
            let parent = project.join(format!("{}.jsonl", name(&session)));
            for (path, meta) in jsonl_files(&session.join("subagents")) {
                out.push(Found { kind: AgentKind::Claude, label: name(&project), modified_ms: mtime_ms(&meta), bytes: meta.len(), path, parent: Some(parent.clone()) });
            }
        }
    }
    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then(a.path.cmp(&b.path)));
    out.truncate(limit);
    out
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"{}\n").unwrap();
    }

    #[test]
    fn finds_the_three_layouts_ignores_other_files_and_orders_newest_first() {
        let home = std::env::temp_dir().join(format!("wb-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        touch(&home.join(".claude/projects/proj-a/s1.jsonl"));
        touch(&home.join(".claude/projects/proj-a/notes.txt"));
        touch(&home.join(".claude/projects/proj-a/sub/deep.jsonl")); // nested dirs are not scanned
        touch(&home.join(".pi/agent/sessions/proj-b/2026_x.jsonl"));
        touch(&home.join(".codex/sessions/2026/10/21/rollout-1.jsonl"));
        std::thread::sleep(std::time::Duration::from_millis(30));
        touch(&home.join(".claude/projects/proj-c/newest.jsonl"));
        let found = discover(&home, 10);
        assert_eq!(found.len(), 4, "{found:?}");
        assert_eq!(found[0].path.file_name().unwrap(), "newest.jsonl", "newest first");
        assert!(found.iter().any(|f| f.kind == AgentKind::Codex && f.label == "2026-10-21"));
        assert!(found.iter().any(|f| f.kind == AgentKind::Pi && f.label == "proj-b"));
        assert_eq!(discover(&home, 2).len(), 2, "limit applies");
        assert!(discover(&home.join("missing"), 5).is_empty());
        assert!(found.iter().all(|f| f.parent.is_none()), "sessions have no parent");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn subagents_are_found_under_their_session_with_the_parent_path_and_stay_out_of_discover() {
        let home = std::env::temp_dir().join(format!("wb-discover-sub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        touch(&home.join(".claude/projects/proj-a/sess1.jsonl"));
        touch(&home.join(".claude/projects/proj-a/sess1/subagents/agent-aaa.jsonl"));
        touch(&home.join(".claude/projects/proj-a/sess1/subagents/agent-bbb.meta.json")); // not a transcript
        touch(&home.join(".claude/projects/proj-a/sess1/other/agent-ccc.jsonl")); // not under subagents/
        std::thread::sleep(std::time::Duration::from_millis(30));
        touch(&home.join(".claude/projects/proj-b/sess2/subagents/agent-ddd.jsonl")); // parent file is missing
        let subs = discover_subagents(&home, 10);
        assert_eq!(subs.len(), 2, "{subs:?}");
        assert_eq!(subs[0].path.file_name().unwrap(), "agent-ddd.jsonl", "newest first");
        assert_eq!(subs[0].label, "proj-b");
        assert_eq!(subs[0].parent.as_deref(), Some(home.join(".claude/projects/proj-b/sess2.jsonl").as_path()));
        assert_eq!(subs[1].parent.as_deref(), Some(home.join(".claude/projects/proj-a/sess1.jsonl").as_path()));
        assert_eq!(discover_subagents(&home, 1).len(), 1, "limit applies");
        assert!(discover_subagents(&home.join("missing"), 5).is_empty());
        let sessions = discover(&home, 10);
        assert!(sessions.iter().all(|f| f.parent.is_none() && !f.path.to_string_lossy().contains("subagents")), "discover stays sessions-only: {sessions:?}");
        let _ = fs::remove_dir_all(&home);
    }
}
