//! Org edges for the agent map, from the roster files agent runs write to `~/.claude/agent-roster/*.json` (user-written JSON,
//! not a transcript). Read-only and bounded: a few small files, and from each one only two fields per agent, `sessionId` and
//! `reportsTo`. Everything else in those files (names, roles, events) is skipped by the parser and never kept.
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// At most this many roster files are read (the newest ones).
pub const MAX_FILES: usize = 32;
/// A roster file larger than this is skipped, not read.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
/// At most this many edges are returned.
pub const MAX_EDGES: usize = 256;

#[derive(Deserialize)]
struct Roster {
    agents: Option<Vec<Agent>>,
}
/// Both fields are taken as loose JSON so one odd entry (a number where a string belongs) costs that entry, not the whole file.
#[derive(Deserialize)]
struct Agent {
    #[serde(rename = "sessionId")]
    session_id: Option<serde_json::Value>,
    #[serde(rename = "reportsTo")]
    reports_to: Option<serde_json::Value>,
}

/// A session id is a UUID/ULID-style token (it is later compared with file names), so keep it strict.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn text(v: Option<serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) if safe_id(&s) => Some(s),
        _ => None,
    }
}

fn read_small(path: &Path) -> Option<String> {
    let mut body = Vec::new();
    // One byte past the limit tells a file that grew since it was listed from one that fits.
    fs::File::open(path).ok()?.take(MAX_FILE_BYTES + 1).read_to_end(&mut body).ok()?;
    u64::try_from(body.len()).ok().filter(|n| *n <= MAX_FILE_BYTES)?;
    String::from_utf8(body).ok()
}

/// `(manager, report)` pairs: `report`'s `reportsTo` names `manager`, both as session ids. Newest roster files first, file order
/// within a file; self edges, duplicates, empty or unsafe ids, malformed files and oversized files are dropped silently. Cycles
/// are NOT removed here (the map model drops the edge that closes one).
pub fn rosters(dir: &Path) -> Vec<(String, String)> {
    let Ok(rd) = fs::read_dir(dir) else { return vec![] };
    let mut files: Vec<(u64, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file() && m.len() <= MAX_FILE_BYTES)?;
            let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0);
            Some((modified, e.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    files.truncate(MAX_FILES);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, path) in files {
        let Some(roster) = read_small(&path).and_then(|body| serde_json::from_str::<Roster>(&body).ok()) else { continue };
        for agent in roster.agents.unwrap_or_default() {
            let (Some(report), Some(manager)) = (text(agent.session_id), text(agent.reports_to)) else { continue };
            if report != manager && seen.insert((manager.clone(), report.clone())) {
                out.push((manager, report));
                if out.len() >= MAX_EDGES {
                    return out;
                }
            }
        }
    }
    out
}
