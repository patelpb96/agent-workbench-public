//! User settings (the Properties window): one small strictly validated file in the app data folder.
//! Pure logic with no Tauri types; the commands live in `props.rs`. Only options that exist in the app are
//! stored here; candidate options are specified in docs/design/properties-window.md and are not added
//! without approval.
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

const MAX_FILE_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSettings {
    /// Folder new terminals start in; empty means the user's folder.
    pub start_folder: String,
    /// Whether "Extended history (32 MiB)" is ticked when a new terminal is created.
    pub extended_history: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    /// Folder the "Launch agent" row starts in; empty means none chosen yet.
    pub launch_folder: String,
}

/// How solid the two sidebars are over the chosen background (percent; 100 = solid, today's look).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceSettings {
    /// The left rail of modules.
    pub sidebar1_opacity: u8,
    /// The right sidebar (sessions and controls).
    pub sidebar2_opacity: u8,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        AppearanceSettings { sidebar1_opacity: 100, sidebar2_opacity: 100 }
    }
}

/// How the window looks when it opens (Properties > Appearance), remembered between launches.
/// Every field defaults to the behaviour from before the option existed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSettings {
    /// The title bar slides away until the cursor rests on the top edge (true), or always shows (false).
    #[serde(default = "yes")]
    pub titlebar_autohide: bool,
    /// The canvas background mode restored at start: `solid` or `transparent` (desktop transparency).
    /// A chosen image or video is not remembered (the file must be picked again).
    #[serde(default = "solid")]
    pub canvas_mode: String,
    /// The solid fallback colour, `#rrggbb`.
    #[serde(default = "canvas_color")]
    pub canvas_color: String,
    /// The canvas / media alpha, percent.
    #[serde(default = "full")]
    pub canvas_alpha: u8,
    /// Whether the see-through, click-through canvas (Ctrl+Alt+G) was on when the app was last used.
    #[serde(default)]
    pub see_through: bool,
}

fn yes() -> bool {
    true
}
fn solid() -> String {
    "solid".into()
}
fn canvas_color() -> String {
    "#0c1118".into()
}
fn full() -> u8 {
    100
}

impl Default for WindowSettings {
    fn default() -> Self {
        WindowSettings { titlebar_autohide: true, canvas_mode: solid(), canvas_color: canvas_color(), canvas_alpha: 100, see_through: false }
    }
}

impl WindowSettings {
    fn is_valid(&self) -> bool {
        let hex = self.canvas_color.len() == 7
            && self.canvas_color.starts_with('#')
            && self.canvas_color[1..].bytes().all(|b| b.is_ascii_hexdigit());
        matches!(self.canvas_mode.as_str(), "solid" | "transparent") && hex && self.canvas_alpha <= 100
    }
}

/// tmux-style keyboard control in terminals (Properties > Terminals): the prefix key, or `off`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySettings {
    /// `ctrl+b` (tmux's default), `ctrl+a` (screen's), or `off`: no key is taken from the terminals.
    #[serde(default = "ctrl_b")]
    pub prefix: String,
}

fn ctrl_b() -> String {
    "ctrl+b".into()
}

impl Default for KeySettings {
    fn default() -> Self {
        KeySettings { prefix: ctrl_b() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub terminals: TerminalSettings,
    pub agents: AgentSettings,
    /// Absent in files written before this option existed: reads as the defaults.
    #[serde(default)]
    pub appearance: AppearanceSettings,
    /// Absent in files written before this option existed: reads as the defaults.
    #[serde(default)]
    pub window: WindowSettings,
    /// Absent in files written before this option existed: reads as the defaults (Ctrl+B).
    #[serde(default)]
    pub keys: KeySettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: 1,
            terminals: TerminalSettings { start_folder: String::new(), extended_history: false },
            agents: AgentSettings { launch_folder: String::new() },
            appearance: AppearanceSettings::default(),
            window: WindowSettings::default(),
            keys: KeySettings::default(),
        }
    }
}

impl Settings {
    /// Shape and value rules only (whether a folder exists is checked when saving, and again when used).
    pub fn is_valid(&self) -> bool {
        let start = &self.terminals.start_folder;
        let launch = &self.agents.launch_folder;
        self.version == 1
            && self.appearance.sidebar1_opacity <= 100
            && self.appearance.sidebar2_opacity <= 100
            && self.window.is_valid()
            && matches!(self.keys.prefix.as_str(), "ctrl+b" | "ctrl+a" | "off")
            && (start.is_empty() || wb_session::proto::valid_start_dir(start))
            // The launch folder is typed into PowerShell inside quotes, so it follows the stricter agent rule.
            && (launch.is_empty() || crate::agent_terminals::valid_cwd(launch))
    }

    /// The folder to start a new terminal in, or `None` for the user's folder. A saved folder that has since
    /// been deleted also means `None` (a terminal must never fail to open because a setting went stale).
    pub fn start_dir(&self) -> Option<&str> {
        let dir = self.terminals.start_folder.as_str();
        (!dir.is_empty() && Path::new(dir).is_dir()).then_some(dir)
    }
}

/// Missing, oversized, corrupt or invalid files read as the defaults.
pub fn load(path: &Path) -> Settings {
    let read = || -> Option<Settings> {
        if fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let settings: Settings = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        settings.is_valid().then_some(settings)
    };
    read().unwrap_or_default()
}

/// Validated, atomic write.
pub fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    if !settings.is_valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid settings"));
    }
    let bytes = serde_json::to_vec(settings).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn with(start: &str, launch: &str, extended: bool) -> Settings {
        Settings { version: 1, terminals: TerminalSettings { start_folder: start.into(), extended_history: extended }, agents: AgentSettings { launch_folder: launch.into() }, appearance: AppearanceSettings::default(), window: WindowSettings::default(), keys: KeySettings::default() }
    }

    #[test]
    fn the_terminal_prefix_key_defaults_to_ctrl_b_and_takes_only_known_values() {
        assert_eq!(Settings::default().keys.prefix, "ctrl+b");
        for ok in ["ctrl+b", "ctrl+a", "off"] {
            let mut s = Settings::default();
            s.keys.prefix = ok.into();
            assert!(s.is_valid(), "{ok}");
        }
        for bad in ["", "ctrl+c", "Ctrl+B", "ctrl+b "] {
            let mut s = Settings::default();
            s.keys.prefix = bad.into();
            assert!(!s.is_valid(), "{bad:?}");
        }
        let d = dir();
        let p = d.join("settings.json");
        // A file from before the option reads as Ctrl+B; one with the option keeps it.
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""}}"#).unwrap();
        assert_eq!(load(&p).keys.prefix, "ctrl+b");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""},"keys":{"prefix":"off"}}"#).unwrap();
        assert_eq!(load(&p).keys.prefix, "off");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""},"keys":{"prefix":"ctrl+q"}}"#).unwrap();
        assert_eq!(load(&p), Settings::default(), "an unknown prefix reads as the defaults");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn window_settings_default_to_todays_behaviour_and_refuse_bad_values() {
        let w = WindowSettings::default();
        assert!(w.titlebar_autohide && !w.see_through && w.canvas_mode == "solid" && w.canvas_alpha == 100);
        let mut s = Settings { window: WindowSettings { titlebar_autohide: false, canvas_mode: "transparent".into(), canvas_color: "#A0b1C2".into(), canvas_alpha: 0, see_through: true }, ..Settings::default() };
        assert!(s.is_valid());
        for bad in [
            WindowSettings { canvas_mode: "image".into(), ..WindowSettings::default() }, // media is not remembered
            WindowSettings { canvas_color: "#12345".into(), ..WindowSettings::default() },
            WindowSettings { canvas_color: "#12345g".into(), ..WindowSettings::default() },
            WindowSettings { canvas_color: "red".into(), ..WindowSettings::default() },
            WindowSettings { canvas_alpha: 101, ..WindowSettings::default() },
        ] {
            s.window = bad;
            assert!(!s.is_valid(), "{:?}", s.window);
        }
        // A file from before the section, and one with only some of its fields, both load.
        let d = dir();
        let p = d.join("settings.json");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""},"window":{"see_through":true}}"#).unwrap();
        let loaded = load(&p);
        assert!(loaded.window.see_through && loaded.window.titlebar_autohide && loaded.window.canvas_color == "#0c1118");
        let mut changed = loaded;
        changed.window.titlebar_autohide = false;
        save(&p, &changed).unwrap();
        assert!(!load(&p).window.titlebar_autohide);
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""},"window":{"canvas_mode":"video"}}"#).unwrap();
        assert_eq!(load(&p), Settings::default(), "an invalid mode reads as the defaults");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn defaults_are_the_current_behaviour() {
        let s = Settings::default();
        assert!(s.is_valid());
        assert_eq!((s.terminals.start_folder.as_str(), s.terminals.extended_history, s.agents.launch_folder.as_str()), ("", false, ""));
        assert_eq!(s.start_dir(), None);
    }

    #[test]
    fn folders_follow_the_rules_of_what_they_are_used_for() {
        for ok in [with("", "", false), with(r"C:\Users\me\proj", r"C:\Users\me\proj", true), with(r"C:\Users\O'Brien", "", false), with("", r"C:\Program Files (x86)\x", false)] {
            assert!(ok.is_valid(), "{ok:?}");
        }
        for bad in [
            with("relative", "", false),
            with(r"\\server\share", "", false),
            with(r"C:\x\..\y", "", false),
            with(&format!(r"C:\{}", "a".repeat(250)), "", false),
            with("", r"C:\it's", false), // typed into PowerShell inside quotes: stricter
            with("", "C:\\a\u{2019}b", false),
            with("", "relative", false),
            Settings { version: 2, ..Settings::default() },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }

    #[test]
    fn sidebar_opacities_are_percentages_and_older_files_without_them_still_load() {
        let mut s = Settings { appearance: AppearanceSettings { sidebar1_opacity: 0, sidebar2_opacity: 100 }, ..Settings::default() };
        assert!(s.is_valid());
        s.appearance.sidebar2_opacity = 101;
        assert!(!s.is_valid());
        let d = dir();
        let p = d.join("settings.json");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":true},"agents":{"launch_folder":""}}"#).unwrap();
        let loaded = load(&p);
        assert!(loaded.terminals.extended_history, "an older file is read, not discarded");
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        let mut changed = loaded;
        changed.appearance = AppearanceSettings { sidebar1_opacity: 40, sidebar2_opacity: 65 };
        save(&p, &changed).unwrap();
        assert_eq!(load(&p).appearance, AppearanceSettings { sidebar1_opacity: 40, sidebar2_opacity: 65 });
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false},"agents":{"launch_folder":""},"appearance":{"sidebar1_opacity":300,"sidebar2_opacity":1}}"#).unwrap();
        assert_eq!(load(&p), Settings::default(), "300 does not fit a percentage");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_start_folder_is_ignored_not_fatal() {
        let real = dir();
        let shown = real.display().to_string();
        assert_eq!(with(&shown, "", false).start_dir(), Some(shown.as_str()));
        fs::remove_dir_all(&real).unwrap();
        assert_eq!(with(&shown, "", false).start_dir(), None, "deleted folder: the terminal starts in the user's folder");
        assert_eq!(with(r"Z:\definitely\not\here", "", false).start_dir(), None);
    }

    #[test]
    fn the_file_round_trips_and_bad_files_read_as_defaults() {
        let d = dir();
        let p = d.join("settings.json");
        assert_eq!(load(&p), Settings::default(), "missing");
        let s = with(r"C:\Users\me", r"C:\work", true);
        save(&p, &s).unwrap();
        assert_eq!(load(&p), s);
        assert!(!p.with_extension("tmp").exists());
        assert!(save(&p, &with("relative", "", false)).is_err(), "invalid settings are never written");
        assert_eq!(load(&p), s, "and the saved file is untouched");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"nope","extended_history":false},"agents":{"launch_folder":""}}"#).unwrap();
        assert_eq!(load(&p), Settings::default(), "invalid content");
        fs::write(&p, br#"{"version":1,"terminals":{"start_folder":"","extended_history":false,"font":"x"},"agents":{"launch_folder":""}}"#).unwrap();
        assert_eq!(load(&p), Settings::default(), "unknown field");
        fs::write(&p, b"not json").unwrap();
        assert_eq!(load(&p), Settings::default());
        fs::write(&p, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert_eq!(load(&p), Settings::default(), "oversized");
        let _ = fs::remove_dir_all(&d);
    }
}
