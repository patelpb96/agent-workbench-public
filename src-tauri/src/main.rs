#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use agent_workbench::{
    execution_v2::EXECUTION_NOTIFICATION,
    integration::{ipc_v2, CoordinatorV2, SHUTDOWN_BOUND},
    single_instance::{self, Startup},
    splash::{self, RevealReason, Splash},
    window_state::{self, Rect, WindowState},
};
use image::{ImageFormat, ImageReader};
use media_validation::{image_dimensions_are_safe, media_host_ok, parse_byte_range};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
};
use tauri::{Emitter, Manager};

mod agents;
mod autostart;
mod claude_ps;
mod ghost;
mod inspector;
mod lifecycle;
mod media_validation;
mod props;
mod terminals;

/// Where the graceful-close window state is written (set once in setup).
static WINDOW_STATE_PATH: OnceLock<PathBuf> = OnceLock::new();
/// The running splash, set once in setup. Absent before that, and absent when the splash is off.
static SPLASH: OnceLock<Splash> = OnceLock::new();
/// The main window's handle (as an integer), for the session-end save.
static MAIN_HWND: OnceLock<isize> = OnceLock::new();
/// Whether the shell was handed over. The handoff is once per run, so a duplicate page-load event
/// or a close racing the reveal cannot show or re-position the window twice.
static REVEALED: AtomicBool = AtomicBool::new(false);
/// Last known non-maximized, non-minimized rectangle (physical pixels).
static LAST_NORMAL: Mutex<Option<Rect>> = Mutex::new(None);
/// Whether the window was maximized when it last moved or resized, and whether this run already saved its position (the normal
/// close does; the session-end and exit paths save from these cached values only when nothing else did).
static LAST_MAXIMIZED: AtomicBool = AtomicBool::new(false);
static STATE_SAVED: AtomicBool = AtomicBool::new(false);

fn current_rect(window: &tauri::Window) -> Option<Rect> {
    if window.is_maximized().unwrap_or(true) || window.is_minimized().unwrap_or(true) {
        return None;
    }
    // Outer position with INNER size: set_size applies to the inner area, so saving the
    // outer size would grow the window by the border each cycle.
    let (p, s) = (window.outer_position().ok()?, window.inner_size().ok()?);
    Some(Rect { x: p.x, y: p.y, width: s.width, height: s.height })
}

/// Restore the position saved by a previous graceful close, otherwise center.
fn restore_or_center(window: &tauri::WebviewWindow, path: &std::path::Path) {
    let monitors: Vec<Rect> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| Rect { x: m.position().x, y: m.position().y, width: m.size().width, height: m.size().height })
        .collect();
    // The file is consumed here, so a crash before the next graceful close leaves none.
    match window_state::load_and_consume(path).and_then(|s| window_state::resolve(s, &monitors)) {
        Some(s) => {
            let _ = window.set_size(tauri::PhysicalSize::new(s.rect.width, s.rect.height));
            let _ = window.set_position(tauri::PhysicalPosition::new(s.rect.x, s.rect.y));
            if s.maximized {
                let _ = window.maximize();
            }
            *LAST_NORMAL.lock().unwrap() = Some(s.rect);
            lifecycle::record("window restored");
        }
        None => {
            let _ = window.center();
            lifecycle::record("window centered");
        }
    }
}

fn save_window_state(window: &tauri::Window) {
    let Some(path) = WINDOW_STATE_PATH.get() else { return };
    let maximized = window.is_maximized().unwrap_or(false);
    // While maximized the live rectangle is the screen; keep the last normal one.
    let rect = current_rect(window).or(*LAST_NORMAL.lock().unwrap());
    if let Some(rect) = rect {
        let ok = window_state::save(path, &WindowState { rect, maximized }).is_ok();
        if ok {
            STATE_SAVED.store(true, Ordering::SeqCst);
        }
        lifecycle::record(if ok { "window state saved" } else { "window state save failed" });
    }
}

/// A debug build is the development channel: it runs beside the normal build instead of being refused as a duplicate, with
/// its own data folder, a "[DEV]" window title, the amber icon (`tools/make-icons.mjs`) and a DEV badge in the title bar.
const DEV_BUILD: bool = cfg!(debug_assertions);

/// The development channel's identity: its own identifier (the single-instance name, the app data folder and the WebView2
/// profile all derive from it) and a window title the gate can tell apart from the normal build's.
fn apply_dev_identity(config: &mut tauri::Config) {
    config.identifier = format!("{}.dev", config.identifier);
    if let Some(window) = config.app.windows.first_mut() {
        window.title = format!("{} [DEV]", window.title);
    }
}

/// Windows is ending the session (restart, shutdown, sign-out): save the window position the way a normal close does. Windows
/// does not close the window then, so without this the position is lost at every restart. It runs on the listener thread, so
/// it uses only what is safe there: the last known normal rectangle and whether the window is maximized, read from the window
/// handle directly.
fn save_window_state_at_session_end() {
    save_cached_window_state("session end");
}

/// Save the window position from the values cached by the last move or resize, when this run has not saved it yet. Used where
/// there is no window event to ask (Windows ending the session) and as a backstop when the app exits some other graceful way
/// without a window close (the first real restart left no saved position, because Windows ended the app without closing it).
fn save_cached_window_state(why: &str) {
    let Some(path) = WINDOW_STATE_PATH.get() else { return };
    if STATE_SAVED.load(Ordering::SeqCst) {
        return;
    }
    if !REVEALED.load(Ordering::SeqCst) {
        lifecycle::record(&format!("window state saved at {why}: skipped (splash still up)"));
        return;
    }
    let maximized = LAST_MAXIMIZED.load(Ordering::SeqCst);
    let rect = *LAST_NORMAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match rect {
        Some(rect) => {
            let ok = window_state::save(path, &WindowState { rect, maximized }).is_ok();
            if ok {
                STATE_SAVED.store(true, Ordering::SeqCst);
            }
            lifecycle::record(&format!("window state saved at {why}: {}", if ok { "ok" } else { "failed" }));
        }
        None => lifecycle::record(&format!("window state saved at {why}: skipped (no known position)")),
    }
}

/// Milliseconds since the process started, for the splash clock.
fn now_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The reveal handoff: the one place the shell becomes visible.
///
/// The splash owns the screen until this point, which is why the saved window geometry is
/// consumed here and only here: `restore_or_center` reads `window-state.json` and deletes it as it
/// reads it, so restoring earlier would leave the shell positioned behind the splash and nothing
/// to restore at the handoff. `REVEALED` makes the whole thing idempotent.
fn reveal_main(app: &tauri::AppHandle, reason: RevealReason) {
    if REVEALED.swap(true, Ordering::SeqCst) {
        return;
    }
    lifecycle::record(&format!("splash revealed reason={}", reason.as_str()));
    if let Some(window) = app.get_webview_window("main") {
        // Show first, then place: both calls run inside this one dispatch, before Windows paints,
        // so the pre-restore rectangle is never on screen.
        if let Err(failure) = window.show() {
            lifecycle::record(&format!("main window show failed: {failure}"));
        }
        match WINDOW_STATE_PATH.get() {
            Some(path) => restore_or_center(&window, path),
            // No app-data directory: centring is still the handoff's job.
            None => {
                let _ = window.center();
                lifecycle::record("window centered");
            }
        }
    }
    if let Some(splash) = SPLASH.get() {
        splash.stop();
    }
}

#[derive(Debug, Serialize)]
struct AppError {
    code: &'static str,
    user_message: String,
    recoverable: bool,
}
fn error(code: &'static str, message: impl Into<String>) -> AppError {
    AppError {
        code,
        user_message: message.into(),
        recoverable: true,
    }
}
#[derive(Serialize)]
struct RuntimeInfo {
    interface_version: u8,
    app_version: &'static str,
    os: &'static str,
    providers: Vec<ProviderRuntime>,
}
#[derive(Serialize)]
struct ProviderRuntime {
    provider: &'static str,
    executable_found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    capabilities: Vec<Capability>,
}
#[derive(Serialize)]
struct Capability {
    name: &'static str,
    status: &'static str,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<String>,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackgroundKind {
    Solid,
    Transparent,
    Image,
    Video,
}
#[derive(Deserialize)]
struct WindowAppearance {
    background_kind: BackgroundKind,
    color_rgba: String,
    window_alpha: u8,
}
#[derive(Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    Image,
    Video,
}
#[derive(Serialize)]
struct SelectedBackground {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<MediaKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
}
struct Asset {
    file: File,
    mime: &'static str,
    length: u64,
}
#[derive(Default)]
struct Media(Mutex<HashMap<String, Asset>>);

#[tauri::command]
fn reset_window(window: tauri::WebviewWindow) -> Result<(), AppError> {
    // Back to a normal, smaller, centered window, even when the window was merely resized to full
    // screen size without being maximized (the title bar's reset button).
    let fail = |_| error("window_failed", "The window could not be reset.");
    window.unmaximize().map_err(fail)?;
    let (width, height) = match window.current_monitor() {
        Ok(Some(monitor)) => {
            let scale = monitor.scale_factor().max(0.5);
            window_state::default_size(monitor.size().width as f64 / scale, monitor.size().height as f64 / scale)
        }
        _ => (1280.0, 800.0),
    };
    window.set_size(tauri::LogicalSize::new(width, height)).map_err(fail)?;
    window.center().map_err(fail)
}

#[tauri::command]
fn get_runtime_info() -> Result<RuntimeInfo, AppError> {
    // Discovery only. Never execute Claude or start a provider agent process.
    let providers = ["claude", "codex"]
        .into_iter()
        .map(|provider| {
            let executable_dir = std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths).find(|dir| {
                    ["exe", "cmd", "bat"]
                        .iter()
                        .any(|ext| dir.join(format!("{provider}.{ext}")).is_file())
                })
            });
            let version = executable_dir.as_ref().and_then(|dir| {
                let package = if provider == "codex" {
                    "@openai/codex"
                } else {
                    "@anthropic-ai/claude-code"
                };
                let bytes =
                    std::fs::read(dir.join("node_modules").join(package).join("package.json"))
                        .ok()?;
                let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                if manifest["name"].as_str()? != package {
                    return None;
                }
                manifest["version"].as_str().map(str::to_owned)
            });
            ProviderRuntime {
                provider,
                executable_found: executable_dir.is_some(),
                version,
                capabilities: [
                    "structured_output",
                    "streaming_input",
                    "resume",
                    "cancel",
                    "approval_events",
                    "usage_reporting",
                ]
                .into_iter()
                .map(|name| Capability {
                    name,
                    status: "unknown",
                    source: "not probed: process execution disabled for spike",
                    checked_at: None,
                })
                .collect(),
            }
        })
        .collect();
    Ok(RuntimeInfo {
        interface_version: 1,
        app_version: if DEV_BUILD { concat!(env!("CARGO_PKG_VERSION"), "-dev") } else { env!("CARGO_PKG_VERSION") },
        os: std::env::consts::OS,
        providers,
    })
}

#[tauri::command]
fn set_window_appearance(
    window: tauri::WebviewWindow,
    appearance: WindowAppearance,
) -> Result<(), AppError> {
    let text = appearance
        .color_rgba
        .strip_prefix('#')
        .unwrap_or(&appearance.color_rgba);
    if text.len() != 8
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
        || appearance.window_alpha > 100
    {
        return Err(error(
            "invalid_appearance",
            "Use #RRGGBBAA and an alpha from 0 to 100.",
        ));
    }
    let value =
        u32::from_str_radix(text, 16).map_err(|_| error("invalid_appearance", "Invalid color."))?;
    let [r, g, b, _] = value.to_be_bytes();
    // Apply canvas alpha in the UI exactly once. Keep the native backing fully
    // transparent so CSS/media alpha is not composited twice.
    let alpha = match appearance.background_kind {
        BackgroundKind::Solid => 255,
        BackgroundKind::Transparent | BackgroundKind::Image | BackgroundKind::Video => 0,
    };
    match window.set_background_color(Some(tauri::window::Color(r, g, b, alpha))) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = window.set_background_color(Some(tauri::window::Color(r, g, b, 255)));
            if matches!(appearance.background_kind, BackgroundKind::Solid) {
                Ok(()) // The opaque UI canvas remains usable as the solid fallback.
            } else {
                Err(error(
                    "transparency_unsupported",
                    "Native background update failed; solid fallback restored. Paint the solid canvas in the UI.",
                ))
            }
        }
    }
}

/// Records what the picker produced (kind, size, dimensions or the refusal code; never a path or a name) so a
/// background that does not appear can be diagnosed from the lifecycle log.
#[tauri::command]
async fn open_background_file(
    app: tauri::AppHandle,
    kind: MediaKind,
) -> Result<SelectedBackground, AppError> {
    let result = pick_background(app, kind).await;
    lifecycle::record(&match &result {
        Ok(b) if b.status == "cancelled" => "media pick cancelled".to_string(),
        Ok(b) => format!(
            "media pick selected mime={} dimensions={}x{}",
            b.mime_type.as_deref().unwrap_or("?"),
            b.width.unwrap_or(0),
            b.height.unwrap_or(0)
        ),
        Err(e) => format!("media pick refused code={}", e.code),
    });
    result
}

async fn pick_background(
    app: tauri::AppHandle,
    kind: MediaKind,
) -> Result<SelectedBackground, AppError> {
    let picked = rfd::AsyncFileDialog::new()
        .add_filter(
            match kind {
                MediaKind::Image => "Image",
                MediaKind::Video => "Video",
            },
            match kind {
                MediaKind::Image => &["png", "jpg", "jpeg", "webp"][..],
                MediaKind::Video => &["mp4", "webm"][..],
            },
        )
        .pick_file()
        .await;
    let Some(picked) = picked else {
        return Ok(SelectedBackground {
            status: "cancelled",
            asset_id: None,
            asset_url: None,
            kind: None,
            display_name: None,
            mime_type: None,
            width: None,
            height: None,
        });
    };
    let path = picked.path();
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match (kind, ext.as_str()) {
        (MediaKind::Image, "png") => "image/png",
        (MediaKind::Image, "jpg" | "jpeg") => "image/jpeg",
        (MediaKind::Image, "webp") => "image/webp",
        (MediaKind::Video, "mp4") => "video/mp4",
        (MediaKind::Video, "webm") => "video/webm",
        _ => {
            return Err(error(
                "unsupported_format",
                "Select PNG, JPEG, WebP, MP4, or WebM.",
            ))
        }
    };
    let file = File::open(path)
        .map_err(|_| error("file_unavailable", "The selected file cannot be opened."))?;
    let metadata = file
        .metadata()
        .map_err(|_| error("file_unavailable", "Cannot read selected media metadata."))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(error(
            "invalid_media",
            "Select a nonempty regular media file.",
        ));
    }
    let size_limit = match kind {
        MediaKind::Image => 8 * 1024 * 1024,
        MediaKind::Video => 64 * 1024 * 1024,
    };
    if metadata.len() > size_limit {
        return Err(error(
            "media_too_large",
            "Images are limited to 8 MiB and videos to 64 MiB in this spike.",
        ));
    }
    let dimensions = if matches!(kind, MediaKind::Image) {
        let format = match ext.as_str() {
            "png" => ImageFormat::Png,
            "jpg" | "jpeg" => ImageFormat::Jpeg,
            "webp" => ImageFormat::WebP,
            _ => return Err(error("unsupported_format", "Unsupported image format.")),
        };
        let probe = file
            .try_clone()
            .map_err(|_| error("file_unavailable", "Cannot inspect selected image."))?;
        let reader = ImageReader::with_format(BufReader::new(probe), format);
        let (width, height) = reader
            .into_dimensions()
            .map_err(|_| error("invalid_media", "The selected image header is invalid."))?;
        if !image_dimensions_are_safe(width, height) {
            return Err(error(
                "image_dimensions_too_large",
                "Images must be at most 16,384 pixels per side and 16 megapixels total.",
            ));
        }
        Some((width, height))
    } else {
        None
    };
    let id = uuid::Uuid::new_v4().to_string();
    let state = app.state::<Media>();
    let mut assets = state
        .0
        .lock()
        .map_err(|_| error("internal", "Media state unavailable."))?;
    assets.clear(); // At most one selected file stays accessible; reselection revokes the previous ID.
    assets.insert(
        id.clone(),
        Asset {
            file,
            mime,
            length: metadata.len(),
        },
    );
    Ok(SelectedBackground {
        status: "selected",
        asset_url: Some(format!("http://background.localhost/{id}")),
        asset_id: Some(id),
        kind: Some(kind),
        display_name: Some(picked.file_name()),
        mime_type: Some(mime.into()),
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
    })
}

fn main() {
    // A development build has its own terminal service (data folder and pipe), chosen before anything starts: the service the
    // app launches, the shells inside its terminals and `wb` run there all inherit this.
    if DEV_BUILD {
        std::env::set_var("AW_CHANNEL", "dev");
    }
    // The splash clock starts with the process. The 4000 ms deadline is measured from the splash
    // appearing, and `Splash::start` below adds this offset to it.
    let started = std::time::Instant::now();
    // One GUI per logon session (docs/design/startup-login-plan.md, P1). The context is hoisted so
    // the gate below can use the identifier and main-window title the app was built with, and so
    // that same context is handed to the builder. A duplicate launch leaves here before any
    // window, coordinator, terminal or IPC work exists; `_claim` holds the named mutex and must
    // stay alive until this function returns, which frees the name for the next launch.
    let mut context = tauri::generate_context!();
    if DEV_BUILD {
        apply_dev_identity(context.config_mut());
    }
    let (identifier, title) = {
        let config = context.config();
        let title = config.app.windows.first().map(|window| window.title.clone()).unwrap_or_default();
        (config.identifier.clone(), title)
    };
    let _claim = match single_instance::launch(&identifier, &title) {
        Startup::Duplicate { .. } => return,
        Startup::Primary(claim) => Some(claim),
        Startup::Unclaimed => None,
    };
    tauri::Builder::default()
        .manage(Media::default())
        .manage(terminals::Terminals::new())
        .manage(inspector::Inspector::new())
        .manage(std::sync::Arc::new(ghost::Ghost::default()))
        .manage(std::sync::Arc::new(agents::AgentLinks::default()))
        // The native page-load signal is the "shell is ready" input: no frontend change, and no
        // guessing from the animation's own progress. Only a finished load of the main webview
        // counts, so a secondary window cannot reveal the splash.
        .on_page_load(|webview, payload| {
            if payload.event() != tauri::webview::PageLoadEvent::Finished || webview.label() != "main" {
                return;
            }
            lifecycle::record("main page load finished");
            if let Some(splash) = SPLASH.get() {
                splash.shell_ready();
            }
        })
        // `move`: the setup closure must be `'static`, so it cannot borrow `started` from `main`.
        // `Instant` is `Copy`, so `move` takes a copy of the process-start instant at the moment
        // this closure is created and nothing else: `started` is the only outer local the body
        // names (`identifier` and `title` are used before the chain, and `context` — which
        // `.build(context)` below still needs — is not referenced here at all, so it is not
        // captured). Keep that true: any further outer local read inside this body must be
        // cloned before the chain rather than moved into it. `now_ms(started)` is still evaluated
        // when `Splash::start` runs, so the splash clock is unchanged.
        .setup(move |app| {
            let handle = app.handle().clone();
            if let Some(hwnd) = app.get_webview_window("main").and_then(|w| w.hwnd().ok()) {
                let _ = MAIN_HWND.set(hwnd.0 as isize);
                if !agent_workbench::session_end::start(save_window_state_at_session_end, lifecycle::record) {
                    lifecycle::record("session-end listener could not start");
                }
            }
            if DEV_BUILD {
                if let (Some(window), Ok(icon)) = (app.get_webview_window("main"), tauri::image::Image::from_bytes(include_bytes!("../icons/icon-dev.png"))) {
                    let _ = window.set_icon(icon);
                }
            }
            // The splash comes up first: it is the first thing the user sees, and the coordinator
            // below can take seconds. `AW_NO_SPLASH=1` opts out; reduced motion does not.
            let splash = if splash::opt_out_from(std::env::var("AW_NO_SPLASH").ok().as_deref()) {
                Splash::disabled()
            } else {
                let reveal = app.handle().clone();
                Splash::start(now_ms(started), move |reason| {
                    let handoff = reveal.clone();
                    // The window thread fires this on its own thread when the deadline expires, so
                    // the whole handoff is marshalled onto the app's main thread, where window
                    // calls belong.
                    if let Err(failure) = reveal.run_on_main_thread(move || reveal_main(&handoff, reason)) {
                        lifecycle::record(&format!("splash reveal dispatch failed: {failure}"));
                    }
                })
            };
            let splash_showing = splash.is_showing();
            let _ = SPLASH.set(splash);
            if let Some(ghost) = app.try_state::<std::sync::Arc<ghost::Ghost>>() {
                ghost::start(app.handle(), ghost.inner());
            }
            let execution = match app.path().app_data_dir() {
                Ok(directory) => {
                    lifecycle::init(directory.clone());
                    lifecycle::record("setup");
                    agent_workbench::codex::set_diagnostic_sink(lifecycle::record);
                    let state_path = directory.join("window-state.json");
                    let _ = WINDOW_STATE_PATH.set(state_path);
                    // The restore is deliberately NOT done here any more: `reveal_main` runs it at
                    // the handoff, so the shell is positioned at the moment it appears.
                    // v2 journal in its own fresh directory (user decision
                    // 2026-10-21); legacy `simulated-execution` is never opened.
                    CoordinatorV2::start_production(directory.join("execution-v2"), move |hint| {
                        handle.emit(EXECUTION_NOTIFICATION, hint).is_ok()
                    })
                }
                Err(_) => CoordinatorV2::disabled(),
            };
            app.manage(execution);
            // A splash that is not on screen (opt-out, or it failed to appear) hands over at once;
            // otherwise the reveal callback does it, so the saved geometry is consumed exactly
            // once, at the moment the shell appears.
            if !splash_showing {
                reveal_main(app.handle(), RevealReason::Manual);
            }
            Ok(())
        })
        // Windows URL: http://background.localhost/<asset_id>. No paths, directory scopes,
        // filesystem plugin, or asset protocol permissions are exposed. Range requests allow seeking.
        .register_uri_scheme_protocol("background", |context, request| {
            use tauri::http::{Response, StatusCode};
            let respond = || -> Option<_> {
                if request.method() != "GET" && request.method() != "HEAD" {
                    return None;
                }
                if !media_host_ok(request.uri().host()) {
                    return None;
                }
                let id = request.uri().path().strip_prefix('/')?;
                if id.len() != 36 || request.uri().query().is_some() {
                    return None;
                }
                let media = context.app_handle().state::<Media>();
                let mut map = media.0.lock().ok()?;
                let asset = map.get_mut(id)?;
                if asset.file.metadata().ok()?.len() != asset.length {
                    return None;
                }
                let range_error = || {
                    Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header("Content-Range", format!("bytes */{}", asset.length))
                        .body(Vec::new())
                        .ok()
                };
                let (start, end, partial) = match request.headers().get("range") {
                    Some(header) => {
                        let Ok(value) = header.to_str() else {
                            return range_error();
                        };
                        let Ok((start, end)) = parse_byte_range(value, asset.length) else {
                            return range_error();
                        };
                        (start, end, true)
                    }
                    None => (0, asset.length - 1, false),
                };
                let count = end - start + 1;
                let mut body = Vec::new();
                if request.method() != "HEAD" {
                    asset.file.seek(SeekFrom::Start(start)).ok()?;
                    let bytes_read = asset
                        .file
                        .by_ref()
                        .take(count)
                        .read_to_end(&mut body)
                        .ok()?;
                    if bytes_read as u64 != count {
                        return None;
                    }
                }
                let mut response = Response::builder()
                    .status(if partial {
                        StatusCode::PARTIAL_CONTENT
                    } else {
                        StatusCode::OK
                    })
                    .header("Content-Type", asset.mime)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", count.to_string())
                    .header("Cache-Control", "no-store")
                    .header("Access-Control-Allow-Origin", "http://tauri.localhost")
                    .header("X-Content-Type-Options", "nosniff");
                if partial {
                    response = response.header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", asset.length),
                    );
                }
                static SERVED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                    lifecycle::record(&format!("media served mime={} bytes={count} partial={partial}", asset.mime));
                }
                response.body(body).ok()
            };
            respond().unwrap_or_else(|| {
                lifecycle::record(&format!(
                    "media request refused method={} host={:?} path_len={}",
                    request.method(),
                    request.uri().host(),
                    request.uri().path().len()
                ));
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Vec::new())
                    .unwrap()
            })
        })
        .invoke_handler(tauri::generate_handler![
            get_runtime_info,
            reset_window,
            set_window_appearance,
            open_background_file,
            ipc_v2::bootstrap_workspace_v2,
            ipc_v2::create_session_v2,
            ipc_v2::start_fake_run_v2,
            ipc_v2::start_codex_run_v2,
            ipc_v2::cancel_run_v2,
            ipc_v2::get_workspace_snapshot_v2,
            ipc_v2::get_run_progress_v2,
            ipc_v2::preview_codex_context_v2,
            ipc_v2::list_run_events_v2,
            ipc_v2::save_layout_v2,
            ghost::ghost_info,
            ghost::ghost_toggle,
            ghost::ghost_report,
            terminals::terminal_list,
            terminals::terminal_new,
            terminals::terminal_read,
            terminals::terminal_watch,
            terminals::terminal_input,
            terminals::terminal_resize,
            terminals::terminal_end,
            terminals::terminal_restore,
            agents::agent_restore,
            terminals::terminal_autostart_get,
            terminals::terminal_autostart_set,
            terminals::terminal_log,
            terminals::terminal_draft,
            terminals::terminal_draft_accept,
            terminals::terminal_draft_discard,
            terminals::terminal_grants,
            terminals::terminal_grant_create,
            terminals::terminal_grant_revoke,
            terminals::terminal_gate_set,
            terminals::terminal_layout_load,
            terminals::terminal_layout_save,
            terminals::terminal_windows_load,
            terminals::terminal_windows_save,
            inspector::inspector_tags_load,
            inspector::inspector_tags_save,
            props::settings_load,
            props::settings_save,
            props::pick_folder,
            props::locations_list,
            props::locations_open,
            props::files_list,
            props::files_home,
            props::files_reveal,
            agents::agent_launch,
            agents::agent_resume,
            agents::agent_terminals,
            agents::agent_forget,
            agents::agent_link,
            inspector::inspector_sources,
            inspector::inspector_snapshot,
            inspector::inspector_map
        ])
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    lifecycle::record("window close_requested");
                    // Graceful close only: this is the one place the position is saved.
                    if window.label() == "main" {
                        // Only a window that was actually handed over carries the rectangle the
                        // user saw; a close during the splash would save the pre-restore default.
                        if REVEALED.load(Ordering::SeqCst) {
                            save_window_state(window);
                        } else {
                            lifecycle::record("window state saved: skipped (splash still up)");
                        }
                        // Closing during the splash must not leave the topmost splash window behind,
                        // and no handoff is wanted on the way out.
                        if let Some(splash) = SPLASH.get() {
                            splash.stop();
                        }
                    }
                }
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => {
                    if let Some(rect) = current_rect(window) {
                        *LAST_NORMAL.lock().unwrap() = Some(rect);
                    }
                    LAST_MAXIMIZED.store(window.is_maximized().unwrap_or(false), Ordering::SeqCst);
                }
                tauri::WindowEvent::Destroyed => {
                    lifecycle::record("window destroyed");
                    if window.label() == "main" {
                        // The main window is gone: the splash has nothing left to wait for.
                        if let Some(splash) = SPLASH.get() {
                            splash.stop();
                        }
                    }
                }
                tauri::WindowEvent::Focused(focused) => {
                    lifecycle::record(&format!("window focused={focused}"))
                }
                _ => {}
            }
            // Never query or emit to a window that is being torn down.
            if matches!(
                event,
                tauri::WindowEvent::Focused(_) | tauri::WindowEvent::Resized(_)
            ) {
                let visible =
                    window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(true);
                let _ = window.emit(
                    "window_visibility_changed",
                    serde_json::json!({"is_visible": visible}),
                );
            }
        })
        .build(context)
        .expect("failed to initialize native shell")
        .run(|app, event| {
            match &event {
                tauri::RunEvent::Ready => lifecycle::record("run ready"),
                tauri::RunEvent::ExitRequested { code, .. } => {
                    lifecycle::record(&format!("run exit_requested code={code:?}"))
                }
                tauri::RunEvent::Exit => lifecycle::record("run exit"),
                _ => {}
            }
            if matches!(event, tauri::RunEvent::Exit) {
                // A graceful exit that did not come through a window close (Windows ending the session) still keeps the position.
                save_cached_window_state("exit");
                // Whatever happened on the way here, no topmost splash window outlives the app.
                if let Some(splash) = SPLASH.get() {
                    splash.stop();
                }
                if let Some(execution) = app.try_state::<CoordinatorV2>() {
                    // Development-phase clean close: the owner cancels an active
                    // Codex run, proves teardown and commits `cancelled` within
                    // its bound; anything left is interrupted on the next open.
                    execution.shutdown(SHUTDOWN_BOUND + std::time::Duration::from_secs(1));
                    lifecycle::record("execution shutdown returned");
                }
            }
        });
}

#[cfg(test)]
mod dev_channel_tests {
    use super::*;

    #[test]
    fn the_development_channel_gets_its_own_identity_and_title() {
        let mut config = tauri::Config { identifier: "local.agent-workbench.spike".into(), ..Default::default() };
        config.app.windows.push(tauri::utils::config::WindowConfig { title: "Agent Workbench - Shell Spike".into(), ..Default::default() });
        apply_dev_identity(&mut config);
        assert_eq!(config.identifier, "local.agent-workbench.spike.dev");
        assert_eq!(config.app.windows[0].title, "Agent Workbench - Shell Spike [DEV]");
        // So the single-instance name differs from the normal build's: both can run side by side.
        assert_ne!(single_instance::mutex_name(&config.identifier), single_instance::mutex_name("local.agent-workbench.spike"));
    }

    #[test]
    fn a_config_without_windows_still_gets_the_identifier() {
        let mut config = tauri::Config { identifier: "x".into(), ..Default::default() };
        apply_dev_identity(&mut config);
        assert_eq!(config.identifier, "x.dev");
    }
}
