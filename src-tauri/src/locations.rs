//! Where Agent Workbench's things live on THIS machine, for the Properties "Data" tab. Only places that
//! exist are listed. The webview never names a path: it gets this list (display only) and asks to open one
//! by its id; the path is looked up again natively. Pure logic with no Tauri types.
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Location {
    /// A fixed name from the list below (what the webview sends back to open it).
    pub id: &'static str,
    pub label: &'static str,
    pub path: String,
    /// A short fact about it (size, count, age); empty when there is nothing useful to say.
    pub detail: String,
    /// A file (opened by revealing it in Explorer) rather than a folder.
    pub file: bool,
}

/// The folders everything is found from.
#[derive(Clone, Debug)]
pub struct Roots {
    pub app_data: PathBuf,
    pub local_data: PathBuf,
    pub home: PathBuf,
    pub exe_dir: PathBuf,
}

fn dir_size(path: &Path, depth: usize) -> u64 {
    let Ok(entries) = fs::read_dir(path) else { return 0 };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.metadata() {
            Ok(m) if m.is_file() => m.len(),
            Ok(m) if m.is_dir() && depth < 4 => dir_size(&e.path(), depth + 1),
            _ => 0,
        })
        .sum()
}
fn count_entries(path: &Path) -> usize {
    fs::read_dir(path).map(|r| r.filter_map(Result::ok).count()).unwrap_or(0)
}
pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / 1_048_576.0),
    }
}
pub fn human_age(since: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(since).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        s if s < 90 => "just now".to_owned(),
        s if s < 5400 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

fn dir_entry(id: &'static str, label: &'static str, path: PathBuf, detail: String) -> Option<Location> {
    path.is_dir().then(|| Location { id, label, path: path.display().to_string(), detail, file: false })
}

/// The source folder this app was built from: the nearest parent of the running executable that holds both a
/// `.git` folder and `src-tauri`. Only present for a development build.
fn source_repo(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir.ancestors().take(8).find(|p| p.join(".git").exists() && p.join("src-tauri").is_dir()).map(Path::to_path_buf)
}

/// The backup repository set up by `tools/backup/backup.mjs` (its config names the folder).
fn backup_repo(local_data: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(local_data.join("AgentWorkbench").join("backup").join("config.json")).ok()?;
    let repo = serde_json::from_str::<serde_json::Value>(&text).ok()?.get("repo")?.as_str()?.to_owned();
    let repo = PathBuf::from(repo);
    repo.is_dir().then_some(repo)
}

pub fn collect(roots: &Roots, now: SystemTime) -> Vec<Location> {
    let mut out = Vec::new();
    let mut push = |l: Option<Location>| out.extend(l);
    push(dir_entry("app_data", "App data (settings, layout, tags)", roots.app_data.clone(), human_size(dir_size(&roots.app_data, 0))));
    let settings = roots.app_data.join("settings.json");
    if settings.is_file() {
        push(Some(Location { id: "settings", label: "Settings file", path: settings.display().to_string(), detail: String::new(), file: true }));
    }
    push(dir_entry("journal", "Run history (journal)", roots.app_data.join("execution-v2"), human_size(dir_size(&roots.app_data.join("execution-v2"), 0))));
    push(dir_entry("terminal_service", "Terminal service (secret, session list, audit log)", roots.local_data.join("AgentWorkbench").join("wb"), String::new()));
    if let Some(repo) = backup_repo(&roots.local_data) {
        let backups = repo.join("backups");
        let mut files: Vec<(SystemTime, String)> = fs::read_dir(&backups)
            .map(|r| {
                r.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "awbk"))
                    .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.file_name().to_string_lossy().into_owned())))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let detail = match files.last() {
            Some((at, _)) => format!("{} encrypted backups, newest {}", files.len(), human_age(*at, now)),
            None => "no backups yet".to_owned(),
        };
        push(dir_entry("backup_repo", "Private backup repository (encrypted, never share)", repo, detail));
    }
    push(dir_entry("stable_copy", "Stable copy of the app", roots.local_data.join("AgentWorkbench").join("stable"), String::new()));
    if let Some(repo) = source_repo(&roots.exe_dir) {
        push(dir_entry("source", "Source code (this build)", repo, String::new()));
    }
    push(dir_entry("app_folder", "The running app", roots.exe_dir.clone(), String::new()));
    for (id, label, first, second) in [
        ("transcripts_claude", "Claude Code sessions", ".claude", "projects"),
        ("transcripts_pi", "Pi sessions", ".pi", "agent/sessions"),
        ("transcripts_codex", "Codex sessions", ".codex", "sessions"),
    ] {
        let path = roots.home.join(first).join(second);
        let n = count_entries(&path);
        push(dir_entry(id, label, path, format!("{n} folder{}", if n == 1 { "" } else { "s" })));
    }
    out
}

/// The location for one id, if it is listed (looked up again, never taken from the webview).
pub fn resolve(roots: &Roots, id: &str, now: SystemTime) -> Option<Location> {
    collect(roots, now).into_iter().find(|l| l.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (Roots, PathBuf) {
        let base = std::env::temp_dir().join(format!("wb-locations-{}", uuid::Uuid::new_v4()));
        let r = Roots { app_data: base.join("appdata"), local_data: base.join("local"), home: base.join("home"), exe_dir: base.join("exe") };
        for d in [&r.app_data, &r.local_data, &r.home, &r.exe_dir] {
            fs::create_dir_all(d).unwrap();
        }
        (r, base)
    }
    fn ids(list: &[Location]) -> Vec<&'static str> {
        list.iter().map(|l| l.id).collect()
    }

    #[test]
    fn only_places_that_exist_on_this_machine_are_listed() {
        let (r, base) = roots();
        let now = SystemTime::now();
        assert_eq!(ids(&collect(&r, now)), vec!["app_data", "app_folder"], "an empty machine shows only what is there");
        fs::write(r.app_data.join("settings.json"), "{}").unwrap();
        fs::create_dir_all(r.app_data.join("execution-v2")).unwrap();
        fs::write(r.app_data.join("execution-v2").join("execution.sqlite3"), vec![0u8; 2048]).unwrap();
        fs::create_dir_all(r.local_data.join("AgentWorkbench").join("wb")).unwrap();
        fs::create_dir_all(r.home.join(".claude").join("projects").join("a")).unwrap();
        fs::create_dir_all(r.home.join(".claude").join("projects").join("b")).unwrap();
        let list = collect(&r, now);
        assert_eq!(ids(&list), vec!["app_data", "settings", "journal", "terminal_service", "app_folder", "transcripts_claude"]);
        let by = |id: &str| list.iter().find(|l| l.id == id).unwrap().clone();
        assert!(by("settings").file && !by("app_data").file);
        assert_eq!(by("journal").detail, "2 KB");
        assert_eq!(by("transcripts_claude").detail, "2 folders");
        assert!(!ids(&list).contains(&"transcripts_pi"), "no Pi sessions here, so no Pi row");
        assert!(!ids(&list).contains(&"backup_repo") && !ids(&list).contains(&"stable_copy") && !ids(&list).contains(&"source"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn the_backup_repository_and_stable_copy_and_source_appear_when_present() {
        let (r, base) = roots();
        let repo = base.join("private-backup");
        fs::create_dir_all(repo.join("backups")).unwrap();
        fs::create_dir_all(r.local_data.join("AgentWorkbench").join("backup")).unwrap();
        fs::write(r.local_data.join("AgentWorkbench").join("backup").join("config.json"), serde_json::json!({ "repo": repo.display().to_string() }).to_string()).unwrap();
        let now = SystemTime::now();
        assert_eq!(collect(&r, now).iter().find(|l| l.id == "backup_repo").unwrap().detail, "no backups yet");
        fs::write(repo.join("backups").join("workbench-20261022-120000.awbk"), "x").unwrap();
        fs::write(repo.join("backups").join("notes.txt"), "x").unwrap();
        let detail = collect(&r, now).iter().find(|l| l.id == "backup_repo").unwrap().detail.clone();
        assert!(detail.starts_with("1 encrypted backups, newest just now"), "{detail}");
        fs::create_dir_all(r.local_data.join("AgentWorkbench").join("stable")).unwrap();
        // A development build: the executable folder sits inside a repository with src-tauri.
        let src = base.join("src");
        fs::create_dir_all(src.join(".git")).unwrap();
        fs::create_dir_all(src.join("src-tauri").join("target").join("release")).unwrap();
        let dev = Roots { exe_dir: src.join("src-tauri").join("target").join("release"), ..r.clone() };
        assert!(ids(&collect(&dev, now)).contains(&"source") && ids(&collect(&dev, now)).contains(&"stable_copy"));
        assert!(!ids(&collect(&r, now)).contains(&"source"), "an installed build is not inside a source tree");
        // A repository folder that was deleted is not listed.
        fs::remove_dir_all(&repo).unwrap();
        assert!(!ids(&collect(&r, now)).contains(&"backup_repo"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn an_id_resolves_only_when_listed_and_ages_and_sizes_read_well() {
        let (r, base) = roots();
        let now = SystemTime::now();
        assert!(resolve(&r, "app_data", now).is_some());
        for bad in ["", "../x", r"C:\Windows", "transcripts_pi", "settings"] {
            assert!(resolve(&r, bad, now).is_none(), "{bad:?}");
        }
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(3 * 1_048_576), "3.0 MB");
        let t = SystemTime::UNIX_EPOCH;
        let at = |s: u64| human_age(t, t + std::time::Duration::from_secs(s));
        assert_eq!((at(10).as_str(), at(600).as_str(), at(7200).as_str(), at(400_000).as_str()), ("just now", "10 min ago", "2 h ago", "4 days ago"));
        let _ = fs::remove_dir_all(base);
    }
}
