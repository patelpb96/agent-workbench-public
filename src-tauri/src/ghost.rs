//! See-through mode ("ghost"): a global hotkey (Ctrl+Alt+G) makes the Agents canvas transparent and lets clicks on its
//! bare background fall through to whatever window is behind, while the panels, terminal windows, sidebars and title
//! bar stay fully usable. Design and limits: docs/design/overlay-harness.md.
//!
//! How the click-through works. Windows can make a whole window click-through, not just its transparent pixels, so
//! while the mode is on a thread watches the cursor (about 80 times a second) and turns click-through on exactly when
//! the cursor is outside every interactive area. While the mode is on the window is also kept above all other windows,
//! so clicking a window underneath does not bring it over the panels. The page sends those areas (as rectangles, in physical pixels
//! relative to the window's client area) whenever its layout changes; nothing is asked of the page per cursor move.
//! A press that began on an interactive area keeps the window interactive until the button is released, so a drag
//! that leaves a panel is not cut off. The mode lives here, not in the page, so a stuck page cannot trap the window:
//! the hotkey always turns it off.
use crate::{error, lifecycle, AppError};
use serde::Serialize;
use std::{
    mem::zeroed,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_LBUTTON},
        WindowsAndMessaging::{GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, MSG, WM_HOTKEY, WS_EX_LAYERED, WS_EX_TRANSPARENT},
    },
};

/// Sent to the page whenever the mode changes: `{ on: bool }`.
pub const EVENT: &str = "ghost-changed";
pub const HOTKEY_TEXT: &str = "Ctrl+Alt+G";
const HOTKEY_ID: i32 = 0x4157;
const KEY_G: u32 = 0x47;
const POLL: Duration = Duration::from_millis(12);
/// Most rectangles the page may send (a few dozen panels and windows at most; anything more is refused).
const MAX_RECTS: usize = 256;

#[derive(Default)]
pub struct Ghost {
    /// See-through mode is wanted.
    on: AtomicBool,
    /// The page says it applies right now (the Agents tab is showing).
    effective: AtomicBool,
    /// A dialog or menu is open: the whole window takes clicks.
    whole: AtomicBool,
    /// Interactive areas: x, y, width, height in physical pixels, relative to the window's client area.
    rects: Mutex<Vec<[f64; 4]>>,
    poller: AtomicBool,
    registered: AtomicBool,
    /// What was last applied to the window, so a repeated report does not re-apply it (re-asserting always-on-top
    /// moves the window in the z-order every time, which showed as flicker).
    on_top_applied: AtomicBool,
    through_applied: AtomicBool,
}

#[derive(Serialize)]
pub struct GhostInfo {
    on: bool,
    registered: bool,
    hotkey: &'static str,
}

impl Ghost {
    fn engaged(&self) -> bool {
        self.on.load(Ordering::SeqCst) && self.effective.load(Ordering::SeqCst) && !self.whole.load(Ordering::SeqCst)
    }

    pub fn info(&self) -> GhostInfo {
        GhostInfo { on: self.on.load(Ordering::SeqCst), registered: self.registered.load(Ordering::SeqCst), hotkey: HOTKEY_TEXT }
    }

    /// Turn the mode on or off (from the hotkey, or the page's own key handler when the hotkey is unavailable).
    pub fn set_on(self: &Arc<Self>, app: &AppHandle, on: bool) {
        self.on.store(on, Ordering::SeqCst);
        let _ = app.emit(EVENT, serde_json::json!({ "on": on }));
        self.refresh(app);
    }

    /// Start or stop the cursor watch to match the current state; when the mode is not engaged the window takes clicks.
    fn refresh(self: &Arc<Self>, app: &AppHandle) {
        if self.engaged() {
            // Always on top while it is see-through: clicking a window underneath must not bring that window over the
            // panels (that is the point of the mode), and it ends with the mode.
            self.apply_on_top(app, true);
            self.enter_layered(app);
            self.ensure_poller(app);
        } else {
            self.leave_layered(app);
            self.apply_on_top(app, false);
        }
    }

    fn apply_on_top(&self, app: &AppHandle, on_top: bool) {
        if self.on_top_applied.swap(on_top, Ordering::SeqCst) != on_top {
            set_on_top(app, on_top);
        }
    }

    /// The window becomes a layered one once, when the mode starts, and stays one until it ends; while it is on only the
    /// click-through bit is flipped (see `style_through`). Switching the layered style on and off with every cursor
    /// move redrew the transparent window each time, which showed as flicker on the canvas behind the panels.
    fn enter_layered(&self, app: &AppHandle) {
        if !self.through_applied.swap(true, Ordering::SeqCst) {
            set_click_through(app, true);
        }
    }

    fn leave_layered(&self, app: &AppHandle) {
        if self.through_applied.swap(false, Ordering::SeqCst) {
            set_click_through(app, false);
        }
    }

    fn ensure_poller(self: &Arc<Self>, app: &AppHandle) {
        if self.poller.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let (ghost, app) = (self.clone(), app.clone());
        thread::spawn(move || {
            let mut interactive = true;
            let mut size = client_size(&app);
            while ghost.engaged() {
                // A layered window keeps taking clicks only within the size it had when it became layered: turned on while
                // the window was still at its starting size (see-through restored from the last run, then maximized),
                // everything outside 1280x800 let clicks through. On any size change the layering is applied afresh.
                let now = client_size(&app);
                if now != size {
                    size = now;
                    set_click_through(&app, false);
                    set_click_through(&app, true);
                    lifecycle::record(&format!("ghost relayered at {}x{}", now.0, now.1));
                }
                interactive = ghost.interactive_at_cursor(&app, interactive);
                // Checked every tick (a cheap read): the window system can rewrite the styles when the window is
                // maximized or restored, and the bit must be put back at once.
                style_through(&app, !interactive);
                thread::sleep(POLL);
            }
            ghost.leave_layered(&app);
            ghost.apply_on_top(&app, false);
            ghost.poller.store(false, Ordering::SeqCst);
            // Engaged again between the last check and the store above: keep going.
            if ghost.engaged() {
                ghost.ensure_poller(&app);
            }
        });
    }

    /// Is the cursor over something interactive? `was` is the previous answer: a held left button keeps it.
    fn interactive_at_cursor(&self, app: &AppHandle, was: bool) -> bool {
        let Some(origin) = client_origin(app) else { return true };
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: `pt` is a local, valid out-parameter.
        if unsafe { GetCursorPos(&mut pt) } == 0 {
            return true;
        }
        // SAFETY: plain query of the left mouse button's state.
        let button_down = unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } < 0;
        if was && button_down {
            return true;
        }
        let (x, y) = (f64::from(pt.x - origin.x), f64::from(pt.y - origin.y));
        match self.rects.lock() {
            Ok(rects) => over_any(&rects, x, y),
            Err(_) => true,
        }
    }
}

/// Is the point inside any rectangle (x, y, width, height; the far edges are outside)?
fn over_any(rects: &[[f64; 4]], x: f64, y: f64) -> bool {
    rects.iter().any(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
}

/// Screen position of the window's client area origin.
fn client_origin(app: &AppHandle) -> Option<POINT> {
    let hwnd = app.get_webview_window("main")?.hwnd().ok()?;
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: the handle belongs to the live main window; `origin` is a valid out-parameter.
    (unsafe { ClientToScreen(hwnd.0 as _, &mut origin) } != 0).then_some(origin)
}

/// The window's client area size in physical pixels ((0, 0) when it cannot be read).
fn client_size(app: &AppHandle) -> (i32, i32) {
    let Some(hwnd) = app.get_webview_window("main").and_then(|w| w.hwnd().ok()) else { return (0, 0) };
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: the handle belongs to the live main window; `r` is a valid out-parameter.
    if unsafe { GetClientRect(hwnd.0 as _, &mut r) } == 0 {
        return (0, 0);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// Flip only the click-through bit of the window, keeping the layered style: no frame change, no redraw.
fn style_through(app: &AppHandle, through: bool) {
    let Some(hwnd) = app.get_webview_window("main").and_then(|w| w.hwnd().ok()) else { return };
    let handle = hwnd.0 as _;
    // SAFETY: the handle belongs to the live main window; these read and write only its extended style bits.
    unsafe {
        let now = GetWindowLongPtrW(handle, GWL_EXSTYLE);
        let layered = WS_EX_LAYERED as isize;
        let transparent = WS_EX_TRANSPARENT as isize;
        let wanted = if through { now | layered | transparent } else { (now | layered) & !transparent };
        if wanted != now {
            SetWindowLongPtrW(handle, GWL_EXSTYLE, wanted);
        }
    }
}

fn set_on_top(app: &AppHandle, on_top: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_always_on_top(on_top);
    }
}

fn set_click_through(app: &AppHandle, through: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_ignore_cursor_events(through);
    }
}

/// Register the hotkey and handle it on a thread of its own. If another program already owns the key, the mode can
/// still be toggled with the same key while Workbench has focus (the page asks `ghost_info` and handles it).
pub fn start(app: &AppHandle, ghost: &Arc<Ghost>) {
    let (app, ghost) = (app.clone(), ghost.clone());
    thread::spawn(move || {
        // SAFETY: a thread message queue is created by the first message call; the key is registered for this thread.
        let ok = unsafe { RegisterHotKey(null_mut(), HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, KEY_G) } != 0;
        ghost.registered.store(ok, Ordering::SeqCst);
        lifecycle::record(&format!("ghost hotkey {HOTKEY_TEXT} registered={ok}"));
        if !ok {
            return;
        }
        // SAFETY: `msg` is a local, valid out-parameter for each call.
        let mut msg: MSG = unsafe { zeroed() };
        while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
            if msg.message == WM_HOTKEY {
                let now = !ghost.on.load(Ordering::SeqCst);
                ghost.set_on(&app, now);
            }
        }
    });
}

#[tauri::command]
pub fn ghost_info(ghost: State<'_, Arc<Ghost>>) -> GhostInfo {
    ghost.info()
}

/// The page's own key handler (used only when the global hotkey could not be registered).
#[tauri::command]
pub fn ghost_toggle(app: AppHandle, ghost: State<'_, Arc<Ghost>>) -> bool {
    let now = !ghost.on.load(Ordering::SeqCst);
    ghost.set_on(&app, now);
    now
}

/// The page reports whether the mode applies (the Agents tab is showing), whether a dialog or menu is open, and the
/// interactive rectangles in physical pixels. Cheap to call whenever the layout changes.
#[tauri::command]
pub fn ghost_report(app: AppHandle, ghost: State<'_, Arc<Ghost>>, effective: bool, whole: bool, rects: Vec<[f64; 4]>) -> Result<(), AppError> {
    if rects.len() > MAX_RECTS || rects.iter().any(|r| r.iter().any(|v| !v.is_finite())) {
        lifecycle::record(&format!("ghost report refused n={}", rects.len()));
        return Err(error("ghost_invalid", "Too many or invalid areas."));
    }
    // What the clickable areas reach (their right and bottom edge), logged when that changes: a stale or short report
    // shows here as a reach smaller than the window.
    let reach = rects.iter().fold((0f64, 0f64), |(x, y), r| (x.max(r[0] + r[2]), y.max(r[1] + r[3])));
    if let Ok(mut current) = ghost.rects.lock() {
        let before = current.iter().fold((0f64, 0f64), |(x, y), r| (x.max(r[0] + r[2]), y.max(r[1] + r[3])));
        if before != reach || current.len() != rects.len() {
            lifecycle::record(&format!("ghost report n={} reach={}x{} effective={effective} whole={whole}", rects.len(), reach.0.round(), reach.1.round()));
        }
        *current = rects;
    }
    ghost.effective.store(effective, Ordering::SeqCst);
    ghost.whole.store(whole, Ordering::SeqCst);
    ghost.refresh(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_is_interactive_only_inside_a_reported_area() {
        let areas = [[100.0, 50.0, 200.0, 100.0], [0.0, 0.0, 40.0, 40.0]];
        assert!(over_any(&areas, 100.0, 50.0), "top-left corner is inside");
        assert!(over_any(&areas, 299.0, 149.0));
        assert!(!over_any(&areas, 300.0, 100.0), "the right edge is outside");
        assert!(!over_any(&areas, 150.0, 150.0), "the bottom edge is outside");
        assert!(over_any(&areas, 10.0, 10.0), "a second area counts");
        assert!(!over_any(&areas, 500.0, 500.0), "bare canvas passes clicks through");
        assert!(!over_any(&[], 1.0, 1.0), "with nothing reported every click passes through");
    }

    #[test]
    fn the_mode_is_engaged_only_when_on_applicable_and_no_dialog_is_open() {
        let g = Ghost::default();
        assert!(!g.engaged());
        g.on.store(true, Ordering::SeqCst);
        assert!(!g.engaged(), "on but not on the Agents tab");
        g.effective.store(true, Ordering::SeqCst);
        assert!(g.engaged());
        g.whole.store(true, Ordering::SeqCst);
        assert!(!g.engaged(), "a dialog takes the whole window");
    }
}
