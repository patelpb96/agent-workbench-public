//! Agents launched in a terminal by Workbench (Claude Code, Pi, Codex): the validated launch command and
//! the small registry that remembers which terminal runs which agent, in which folder, started when,
//! and under which session id. VIEW/LINK data only: no prompts and no transcript text. The registry
//! lives in the app data folder so a restart of Workbench re-links the still-running agents. Pure logic
//! with no Tauri types; the commands live in `agents.rs`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, fs, io, path::Path};

pub const MAX_ENTRIES: usize = 64;
const MAX_FILE_BYTES: u64 = 32 * 1024;
const MAX_CWD: usize = 240;
/// Sanity bound for a launch time (year 2001 .. 2100, in ms).
const MIN_MS: u64 = 1_000_000_000_000;
const MAX_MS: u64 = 4_102_444_800_000;

pub const AGENTS: [&str; 3] = ["claude", "pi", "codex"];

pub fn agent_title(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("Claude Code"),
        "pi" => Some("Pi"),
        "codex" => Some("Codex"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The terminal (service session id, 8 hex).
    pub terminal: String,
    /// `claude`, `pi` or `codex`.
    pub agent: String,
    /// The id given to Claude Code / Pi with `--session-id`, or the id of the session that was resumed;
    /// empty for a Codex agent started fresh (it has none).
    pub session_id: String,
    pub cwd: String,
    pub launched_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    pub entries: Vec<Entry>,
}

fn terminal_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn session_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// An absolute drive path that is safe to put inside a PowerShell single-quoted string on one typed line.
/// Refused: relative or UNC paths, `..` segments, control characters (a CR/LF would end the line), the
/// ASCII single quote AND the Unicode quotes PowerShell treats as single quotes, and characters Windows
/// does not allow in paths.
pub fn valid_cwd(cwd: &str) -> bool {
    let b = cwd.as_bytes();
    cwd.chars().count() <= MAX_CWD
        && b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && !cwd.chars().any(|c| c.is_control() || matches!(c, '\'' | '"' | '`' | '<' | '>' | '|' | '*' | '?' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}'))
        && !cwd.split('\\').any(|seg| seg == ".." || seg == ".")
}

/// The single line typed into the new PowerShell terminal (the caller adds the Enter key).
pub fn launch_line(agent: &str, session_id: &str, cwd: &str) -> Option<String> {
    if !valid_cwd(cwd) {
        return None;
    }
    let command = match agent {
        "claude" if session_id_ok(session_id) => format!("claude --session-id {session_id}"),
        "pi" if session_id_ok(session_id) => format!("pi --session-id {session_id}"),
        "codex" => "codex".to_owned(),
        _ => return None,
    };
    Some(format!("Set-Location -LiteralPath '{cwd}'; {command}"))
}

/// The single line that RESUMES an existing session (typed like `launch_line`). The id is the agent's own
/// session id, already checked to be a plain token; each agent takes it differently.
pub fn resume_line(agent: &str, session_id: &str, cwd: &str) -> Option<String> {
    if !valid_cwd(cwd) || !session_id_ok(session_id) {
        return None;
    }
    let command = match agent {
        "claude" => format!("claude --resume {session_id}"),
        "pi" => format!("pi --session {session_id}"),
        "codex" => format!("codex resume {session_id}"),
        _ => return None,
    };
    Some(format!("Set-Location -LiteralPath '{cwd}'; {command}"))
}

/// The line that ATTACHES to a Claude Code session already running in the background (`short` is the id
/// `claude agents` shows for it). Attaching starts no second copy: the session keeps running either way.
pub fn attach_line(short: &str, cwd: &str) -> Option<String> {
    (valid_cwd(cwd) && session_id_ok(short)).then(|| format!("Set-Location -LiteralPath '{cwd}'; claude attach {short}"))
}

/// What `claude agents --json` says about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeLive {
    /// Not running (or not listed).
    NotRunning,
    /// Running in the background; `short` is its id for `claude attach`.
    Background { short: String },
    /// Running in another terminal window (an interactive session).
    Interactive,
}

/// Every running session in the JSON that `claude agents --json` prints (an array), by full session id.
/// Anything unexpected is left out.
pub fn parse_claude_agent_list(json: &str) -> std::collections::HashMap<String, ClaudeLive> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return Default::default() };
    items
        .iter()
        .take(256)
        .filter_map(|item| {
            let id = item.get("sessionId").and_then(Value::as_str).filter(|id| session_id_ok(id))?;
            let live = match item.get("kind").and_then(Value::as_str) {
                Some("background") => match item.get("id").and_then(Value::as_str) {
                    Some(short) if session_id_ok(short) => ClaudeLive::Background { short: short.to_owned() },
                    _ => ClaudeLive::Interactive,
                },
                _ => ClaudeLive::Interactive,
            };
            Some((id.to_owned(), live))
        })
        .collect()
}

/// `session_id` in that listing; `NotRunning` when it is not there (or the listing is unusable), so the
/// caller falls back to its own check.
pub fn parse_claude_agents(json: &str, session_id: &str) -> ClaudeLive {
    parse_claude_agent_list(json).remove(session_id).unwrap_or(ClaudeLive::NotRunning)
}

/// The terminal's tab label: "Claude Code · folder".
pub fn label(agent: &str, cwd: &str) -> String {
    let folder = cwd.trim_end_matches('\\').rsplit('\\').next().filter(|f| !f.ends_with(':') && !f.is_empty()).unwrap_or(cwd);
    format!("{} · {}", agent_title(agent).unwrap_or("Agent"), folder.chars().take(60).collect::<String>())
}

impl Registry {
    pub fn new() -> Self {
        Registry { version: 1, entries: Vec::new() }
    }
    pub fn is_valid(&self) -> bool {
        let mut seen = HashSet::new();
        self.version == 1
            && self.entries.len() <= MAX_ENTRIES
            && self.entries.iter().all(|e| {
                terminal_id(&e.terminal)
                    && seen.insert(e.terminal.as_str())
                    && valid_cwd(&e.cwd)
                    && (MIN_MS..=MAX_MS).contains(&e.launched_ms)
                    && match e.agent.as_str() {
                        "claude" | "pi" => session_id_ok(&e.session_id),
                        // Codex cannot be given an id when it starts; only a resumed session has one.
                        "codex" => e.session_id.is_empty() || session_id_ok(&e.session_id),
                        _ => false,
                    }
            })
    }
    pub fn find(&self, terminal: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.terminal == terminal)
    }
    /// Add an entry (the oldest is dropped at the cap). Returns false when it is not valid.
    pub fn add(&mut self, entry: Entry) -> bool {
        let mut next = self.clone();
        next.entries.retain(|e| e.terminal != entry.terminal);
        next.entries.push(entry);
        if next.entries.len() > MAX_ENTRIES {
            next.entries.remove(0);
        }
        if !next.is_valid() {
            return false;
        }
        *self = next;
        true
    }
    pub fn forget(&mut self, terminal: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.terminal != terminal);
        self.entries.len() != before
    }
    /// Drop entries whose terminal no longer exists. Call only after a successful terminal listing.
    pub fn reconcile(&mut self, existing: &HashSet<String>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| existing.contains(&e.terminal));
        self.entries.len() != before
    }
}

/// Missing, oversized, corrupt or invalid files all read as an empty registry.
pub fn load(path: &Path) -> Registry {
    let read = || -> Option<Registry> {
        if fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let reg: Registry = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        reg.is_valid().then_some(reg)
    };
    read().unwrap_or_else(Registry::new)
}

/// Validated, atomic write.
pub fn save(path: &Path, reg: &Registry) -> io::Result<()> {
    if !reg.is_valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent registry"));
    }
    let bytes = serde_json::to_vec(reg).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "agent registry too large"));
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";
    const AT: u64 = 1_792_000_000_000;

    fn entry(t: &str, agent: &str) -> Entry {
        Entry { terminal: t.into(), agent: agent.into(), session_id: if agent == "codex" { String::new() } else { ID.into() }, cwd: r"C:\Users\USERPROFILE\proj".into(), launched_ms: AT }
    }
    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-agents-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_launch_line_is_one_fixed_template_per_agent() {
        let cwd = r"C:\Users\USERPROFILE\My Projects (x86)\a&b";
        assert_eq!(launch_line("claude", ID, cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; claude --session-id {ID}"));
        assert_eq!(launch_line("pi", ID, cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; pi --session-id {ID}"));
        assert_eq!(launch_line("codex", "", cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; codex"));
        assert_eq!(launch_line("bash", ID, cwd), None, "unknown agent");
        assert_eq!(launch_line("claude", "", cwd), None, "Claude needs a session id");
        assert_eq!(launch_line("claude", "a b", cwd), None);
        assert_eq!(launch_line("pi", "x; rm -rf", cwd), None, "a session id can never carry shell syntax");
        assert!(!launch_line("codex", "", cwd).unwrap().contains('\n'));
    }

    #[test]
    fn folders_that_could_break_out_of_the_typed_line_are_refused() {
        for ok in [r"C:\", r"C:\Users", r"d:\work\my proj", r"C:\Users\USERPROFILE\café", r"C:\a (1)\b,c#d$e", r"C:\Program Files (x86)\x"] {
            assert!(valid_cwd(ok), "{ok}");
        }
        for bad in [
            "", "C:", r"\\server\share", r"relative\dir", "/c/Users", r"C:\a'b", "C:\\a\u{2018}b", "C:\\a\u{2019}b", "C:\\a\u{201A}b", "C:\\a\u{201B}b",
            "C:\\a\nb", "C:\\a\rb", "C:\\a\0b", "C:\\a\tb", "C:\\a\"b", "C:\\a`b", r"C:\a|b", r"C:\a<b", r"C:\a*b", r"C:\a?b", r"C:\x\..\y", r"C:\x\.\y",
            &format!("C:\\{}", "a".repeat(250)),
        ] {
            assert!(!valid_cwd(bad), "{bad:?}");
            assert_eq!(launch_line("codex", "", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn labels_show_the_agent_and_the_folder() {
        assert_eq!(label("claude", r"C:\Users\USERPROFILE\proj"), "Claude Code · proj");
        assert_eq!(label("pi", r"C:\Users\USERPROFILE\proj\"), "Pi · proj");
        assert_eq!(label("codex", r"C:\"), "Codex · C:\\");
        assert!(label("codex", &format!("C:\\{}", "x".repeat(200))).chars().count() < 80);
    }

    #[test]
    fn the_registry_validates_every_entry_and_stays_bounded() {
        let mut reg = Registry::new();
        assert!(reg.add(entry("0a1b2c3d", "claude")));
        assert!(reg.add(entry("deadbeef", "codex")));
        assert!(reg.add(entry("0a1b2c3d", "pi")), "re-adding a terminal replaces its entry");
        assert_eq!(reg.entries.len(), 2);
        assert_eq!(reg.find("0a1b2c3d").unwrap().agent, "pi");
        let before = reg.clone();
        for bad in [
            Entry { terminal: "NOPE".into(), ..entry("0a1b2c3d", "pi") },
            Entry { agent: "bash".into(), ..entry("0a1b2c3d", "pi") },
            Entry { session_id: String::new(), ..entry("0a1b2c3d", "claude") },
            Entry { session_id: "has space".into(), ..entry("0a1b2c3d", "pi") },
            Entry { session_id: "has space".into(), ..entry("0a1b2c3d", "codex") },
            Entry { cwd: "relative".into(), ..entry("0a1b2c3d", "pi") },
            Entry { launched_ms: 5, ..entry("0a1b2c3d", "pi") },
        ] {
            assert!(!reg.add(bad.clone()), "{bad:?}");
            assert_eq!(reg, before, "a refused entry changes nothing");
        }
        for i in 0..MAX_ENTRIES + 5 {
            assert!(reg.add(entry(&format!("{:08x}", 0x4000 + i), "codex")));
        }
        assert_eq!(reg.entries.len(), MAX_ENTRIES);
        assert!(reg.find("0a1b2c3d").is_none(), "the oldest entries are dropped at the cap");
        assert!(reg.forget(&format!("{:08x}", 0x4000 + MAX_ENTRIES + 4)));
        assert!(!reg.forget("00000000"));
        let keep: HashSet<String> = [format!("{:08x}", 0x4000 + 10)].into();
        assert!(reg.reconcile(&keep));
        assert_eq!(reg.entries.len(), 1);
        assert!(!reg.reconcile(&keep));
    }

    #[test]
    fn the_registry_file_round_trips_and_bad_files_read_as_empty() {
        let d = dir();
        let p = d.join("agent-terminals.json");
        assert_eq!(load(&p), Registry::new(), "missing");
        let mut reg = Registry::new();
        reg.add(entry("0a1b2c3d", "claude"));
        save(&p, &reg).unwrap();
        assert_eq!(load(&p), reg);
        assert!(!p.with_extension("tmp").exists());
        // Hand-edited / corrupt files are ignored, never trusted.
        fs::write(&p, br#"{"version":1,"entries":[{"terminal":"0a1b2c3d","agent":"claude","session_id":"x y","cwd":"C:\\x","launched_ms":1792000000000}]}"#).unwrap();
        assert_eq!(load(&p), Registry::new());
        fs::write(&p, br#"{"version":1,"entries":[],"extra":1}"#).unwrap();
        assert_eq!(load(&p), Registry::new(), "unknown field");
        fs::write(&p, b"not json").unwrap();
        assert_eq!(load(&p), Registry::new());
        fs::write(&p, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert_eq!(load(&p), Registry::new(), "oversized");
        assert!(save(&p, &Registry { version: 2, entries: vec![] }).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_claude_session_running_in_the_background_is_attached_to_not_resumed() {
        let json = r#"[
          {"pid":1,"id":"c0ffee12","cwd":"C:\\Users\\USERPROFILE","kind":"background","sessionId":"c0ffee12-1234-4abc-8def-1234567890ab","status":"busy"},
          {"pid":2,"id":"0b1c2d3e","cwd":"C:\\x","kind":"interactive","sessionId":"0b1c2d3e-aaaa-4bbb-8ccc-111122223333"},
          {"pid":3,"id":"../evil","cwd":"C:\\x","kind":"background","sessionId":"99999999-aaaa-4bbb-8ccc-111122223333"}
        ]"#;
        assert_eq!(parse_claude_agents(json, "c0ffee12-1234-4abc-8def-1234567890ab"), ClaudeLive::Background { short: "c0ffee12".into() });
        assert_eq!(parse_claude_agents(json, "0b1c2d3e-aaaa-4bbb-8ccc-111122223333"), ClaudeLive::Interactive);
        assert_eq!(parse_claude_agents(json, "99999999-aaaa-4bbb-8ccc-111122223333"), ClaudeLive::Interactive, "an unsafe short id is never typed");
        assert_eq!(parse_claude_agents(json, "ffffffff-aaaa-4bbb-8ccc-111122223333"), ClaudeLive::NotRunning);
        for odd in ["", "not json", "{}", "[1,2]", "[{\"sessionId\":5}]"] {
            assert_eq!(parse_claude_agents(odd, ID), ClaudeLive::NotRunning, "{odd:?}");
        }
        let cwd = r"C:\Users\USERPROFILE";
        assert_eq!(attach_line("c0ffee12", cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; claude attach c0ffee12"));
        assert_eq!(attach_line("a b", cwd), None);
        assert_eq!(attach_line("c0ffee12", "relative"), None);
    }

    #[test]
    fn resuming_types_one_fixed_line_per_agent_and_refuses_anything_unsafe() {
        let cwd = r"C:\Users\USERPROFILE\proj";
        assert_eq!(resume_line("claude", ID, cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; claude --resume {ID}"));
        assert_eq!(resume_line("pi", ID, cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; pi --session {ID}"));
        assert_eq!(resume_line("codex", ID, cwd).unwrap(), format!("Set-Location -LiteralPath '{cwd}'; codex resume {ID}"));
        let long = "x".repeat(65);
        for bad_id in ["", "a b", "x;calc", "../x", "$(x)", "a\nb", long.as_str()] {
            assert_eq!(resume_line("claude", bad_id, cwd), None, "{bad_id:?}");
        }
        for bad_cwd in ["relative", r"C:\it's", r"C:\a\..\b", "C:\\a\nb", r"\\server\share"] {
            assert_eq!(resume_line("pi", ID, bad_cwd), None, "{bad_cwd:?}");
        }
        assert_eq!(resume_line("bash", ID, cwd), None);
        // A resumed Codex session is registered with its id; a fresh one has none.
        let mut reg = Registry::new();
        assert!(reg.add(Entry { session_id: ID.into(), ..entry("0a1b2c3d", "codex") }));
        assert!(reg.add(entry("0a1b2c3e", "codex")));
    }
}
