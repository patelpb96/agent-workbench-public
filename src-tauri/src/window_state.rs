//! Remembered window position, written ONLY on a graceful close and consumed (read,
//! then deleted) at the next start. After a crash, a kill or a shutdown that never ran
//! the close handler there is no file, so the window opens centered. Pure logic with no
//! Tauri types, so every rule is unit-tested; the native glue lives in main.rs.
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

pub const MIN_WIDTH: u32 = 850;
pub const MIN_HEIGHT: u32 = 640;
/// A restored window must overlap a monitor by at least this much to count as reachable.
const MIN_VISIBLE_W: i64 = 200;
const MIN_VISIBLE_H: i64 = 100;
const MAX_FILE_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    /// The normal (not maximized) rectangle in physical pixels: outer position, INNER size.
    pub rect: Rect,
    pub maximized: bool,
}

/// The default "windowed" size used by the title bar's reset button, in logical pixels: the configured
/// 1280x800, but never more than 90% of the monitor and never below the window minimums (the minimums
/// win on a tiny screen, matching `resolve`).
pub fn default_size(monitor_width: f64, monitor_height: f64) -> (f64, f64) {
    let fit = |wanted: f64, available: f64, min: u32| wanted.min(available * 0.9).max(min as f64);
    (fit(1280.0, monitor_width, MIN_WIDTH), fit(800.0, monitor_height, MIN_HEIGHT))
}

fn overlap(a: &Rect, b: &Rect) -> (i64, i64) {
    let w = (a.x as i64 + a.width as i64).min(b.x as i64 + b.width as i64) - (a.x as i64).max(b.x as i64);
    let h = (a.y as i64 + a.height as i64).min(b.y as i64 + b.height as i64) - (a.y as i64).max(b.y as i64);
    (w.max(0), h.max(0))
}

/// Validate a saved state against the monitors present now. `None` means "center":
/// the saved rectangle is not meaningfully visible (monitor unplugged, resolution
/// changed). Sizes are clamped to the minimum and to the largest monitor.
pub fn resolve(saved: WindowState, monitors: &[Rect]) -> Option<WindowState> {
    let max_w = monitors.iter().map(|m| m.width).max()?;
    let max_h = monitors.iter().map(|m| m.height).max()?;
    let mut r = saved.rect;
    r.width = r.width.clamp(MIN_WIDTH.min(max_w), max_w);
    r.height = r.height.clamp(MIN_HEIGHT.min(max_h), max_h);
    let reachable = monitors.iter().any(|m| {
        let (w, h) = overlap(&r, m);
        w >= MIN_VISIBLE_W.min(r.width as i64) && h >= MIN_VISIBLE_H.min(r.height as i64)
    });
    reachable.then_some(WindowState { rect: r, maximized: saved.maximized })
}

/// Read the saved state and delete the file in the same step, so a later crash can
/// never leave a stale record. Missing, oversized or corrupt files give `None`.
pub fn load_and_consume(path: &Path) -> Option<WindowState> {
    let meta = fs::metadata(path).ok()?;
    let parsed = if meta.len() <= MAX_FILE_BYTES {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str::<WindowState>(&s).ok())
    } else {
        None
    };
    let _ = fs::remove_file(path);
    parsed
}

/// Atomic write (temp file then rename).
pub fn save(path: &Path, state: &WindowState) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(state).map_err(io::Error::other)?)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    fn state(x: i32, y: i32, w: u32, h: u32, maximized: bool) -> WindowState {
        WindowState { rect: Rect { x, y, width: w, height: h }, maximized }
    }

    #[test]
    fn the_reset_size_is_the_default_but_fits_the_screen() {
        assert_eq!(default_size(2560.0, 1440.0), (1280.0, 800.0), "large screen: the configured default");
        assert_eq!(default_size(1366.0, 768.0), (1229.4, 691.2), "laptop: 90% of the screen");
        assert_eq!(default_size(800.0, 600.0), (850.0, 640.0), "tiny screen: the minimums win");
        let (w, h) = default_size(3440.0, 1440.0);
        assert!(w >= MIN_WIDTH as f64 && h >= MIN_HEIGHT as f64);
    }

    #[test]
    fn an_on_screen_window_is_restored_as_saved() {
        let s = state(100, 80, 1280, 800, false);
        assert_eq!(resolve(s, &[MON]), Some(s));
        let m = state(100, 80, 1280, 800, true);
        assert!(resolve(m, &[MON]).unwrap().maximized, "maximized flag survives");
    }

    #[test]
    fn a_window_left_on_a_removed_monitor_centers_instead() {
        let second = Rect { x: 1920, y: 0, width: 1920, height: 1080 };
        let s = state(2200, 100, 1280, 800, false);
        assert_eq!(resolve(s, &[MON, second]), Some(s));
        assert_eq!(resolve(s, &[MON]), None, "second monitor unplugged");
        assert_eq!(resolve(state(-32000, -32000, 1280, 800, false), &[MON]), None, "minimized sentinel position");
        assert_eq!(resolve(state(1900, 100, 1280, 800, false), &[MON]), None, "only 20 px visible");
        assert_eq!(resolve(s, &[]), None, "no monitors at all");
    }

    #[test]
    fn sizes_are_clamped_to_the_minimum_and_the_screen() {
        let tiny = resolve(state(10, 10, 100, 100, false), &[MON]).unwrap();
        assert_eq!((tiny.rect.width, tiny.rect.height), (MIN_WIDTH, MIN_HEIGHT));
        let huge = resolve(state(0, 0, 9000, 9000, false), &[MON]).unwrap();
        assert_eq!((huge.rect.width, huge.rect.height), (1920, 1080));
        let small_screen = Rect { x: 0, y: 0, width: 800, height: 600 };
        let fit = resolve(state(0, 0, 1280, 800, false), &[small_screen]).unwrap();
        assert_eq!((fit.rect.width, fit.rect.height), (800, 600), "min is waived when the screen is smaller");
    }

    #[test]
    fn the_file_is_consumed_so_a_crash_leaves_nothing_and_bad_files_are_ignored() {
        let dir = std::env::temp_dir().join(format!("wb-winstate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("window-state.json");
        let s = state(5, 6, 1000, 700, true);
        save(&p, &s).unwrap();
        assert_eq!(load_and_consume(&p), Some(s));
        assert!(!p.exists(), "deleted on read");
        assert_eq!(load_and_consume(&p), None, "second start (after a crash) finds nothing");
        fs::write(&p, b"not json").unwrap();
        assert_eq!(load_and_consume(&p), None);
        assert!(!p.exists(), "corrupt file is removed too");
        fs::write(&p, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert_eq!(load_and_consume(&p), None);
        assert!(!p.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
