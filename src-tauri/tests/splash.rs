//! Splash wiring: source facts only. Nothing here creates a window.
//!
//! The Win32 window itself cannot run in a headless suite (it needs a desktop and a message
//! loop), so what is checked here is what a reviewer would check by reading the two files that
//! wire the splash into the app: the main window must be created hidden, the splash must be up
//! before the coordinator starts, and the reveal must be driven by the native page-load hook
//! rather than by the frontend.
//!
//! The lifecycle itself (`shell_ready || deadline || failed`, the reason ordering, `disabled()`
//! and the poisoned-lock fallback) is unit-tested in `src/splash/` with a `Splash` that never
//! creates a window.

const MAIN_SOURCE: &str = include_str!("../src/main.rs");
const CONFIG: &str = include_str!("../tauri.conf.json");

/// Index of the *first* `needle`; the assertion makes a renamed marker fail on this line, with the
/// marker named. Because it is the first match, an anchor must not be a string that also occurs
/// earlier in a comment.
fn at(source: &str, needle: &str) -> usize {
    assert!(source.contains(needle), "the source no longer contains {needle}");
    source.find(needle).unwrap_or(usize::MAX)
}

#[test]
fn the_main_window_is_created_hidden_and_nothing_else_changed() {
    assert!(
        CONFIG.contains(r#""visible": false"#),
        "the main window must be created hidden, so the splash owns the screen until the handoff"
    );
    assert!(
        !CONFIG.contains(r#""visible": true"#),
        "no window may be created visible: the splash would be covered at once"
    );
    // Everything the reveal depends on must stay exactly as it was: the size the saved geometry
    // is resolved against, the transparency and the frameless look.
    for kept in [
        r#""label": "main""#,
        r#""width": 1280"#,
        r#""height": 800"#,
        r#""minWidth": 850"#,
        r#""minHeight": 640"#,
        r#""transparent": true"#,
        r#""decorations": false"#,
        r#""dragDropEnabled": false"#,
        r#"default-src 'self'"#,
    ] {
        assert!(CONFIG.contains(kept), "tauri.conf.json no longer keeps {kept}");
    }
}

#[test]
fn the_splash_starts_before_the_coordinator_and_can_be_opted_out_of() {
    let decide = at(MAIN_SOURCE, "splash::opt_out_from(");
    let start = at(MAIN_SOURCE, "Splash::start(");
    let disabled = at(MAIN_SOURCE, "Splash::disabled()");
    let coordinator = at(MAIN_SOURCE, "CoordinatorV2::start_production(");

    assert_eq!(MAIN_SOURCE.matches("Splash::start(").count(), 1, "exactly one splash");
    assert_eq!(
        MAIN_SOURCE.matches(r#"std::env::var("AW_NO_SPLASH")"#).count(),
        1,
        "one opt-out, one place"
    );
    assert!(decide < start, "the opt-out is decided before the splash starts");
    assert!(disabled < coordinator, "the opt-out path is inside the same decision");
    assert!(
        start < coordinator,
        "the splash is on screen before the coordinator starts: the reveal must not wait for it"
    );
    assert!(
        at(MAIN_SOURCE, "let _ = SPLASH.set(splash)") < coordinator,
        "the handle is published before anything that can block"
    );
}

#[test]
fn the_reveal_is_driven_by_the_native_page_load_hook() {
    // One hook, registered on the builder (so it applies to the config window's webview), keyed on
    // a *finished* load of the main webview.
    assert_eq!(MAIN_SOURCE.matches(".on_page_load(").count(), 1, "exactly one page-load hook");
    let hook = at(MAIN_SOURCE, ".on_page_load(");
    let hook_body = MAIN_SOURCE.get(hook..at(MAIN_SOURCE, ".setup(")).unwrap_or("");
    assert!(
        hook_body.contains("PageLoadEvent::Finished"),
        "only a finished load may reveal the splash"
    );
    assert!(
        hook_body.contains(r#"webview.label() != "main""#),
        "a secondary window's page load must not reveal the splash"
    );
    assert_eq!(
        MAIN_SOURCE.matches("shell_ready()").count(),
        1,
        "the page-load hook is the only thing that marks the shell ready"
    );
    assert!(hook_body.contains("shell_ready()"), "and it does so from the hook");
}

#[test]
fn the_reveal_handoff_shows_the_window_places_it_and_stops_the_splash() {
    let handoff = at(MAIN_SOURCE, "fn reveal_main(");
    let body = MAIN_SOURCE.get(handoff..at(MAIN_SOURCE, "#[derive(Debug, Serialize)]")).unwrap_or("");
    assert!(body.contains("REVEALED.swap(true"), "the handoff happens exactly once");
    let show = body.find("window.show()").unwrap_or(usize::MAX);
    let restore = body.find("restore_or_center(&window, path)").unwrap_or(usize::MAX);
    let stop = body.find("splash.stop()").unwrap_or(usize::MAX);
    assert!(show < restore, "the window is shown, then placed");
    assert!(restore < stop, "the splash stops last, once the shell is there");
    assert!(
        !MAIN_SOURCE.contains("restore_or_center(&window, &state_path)"),
        "the restore must only happen at the handoff, not during setup"
    );
}

#[test]
fn closing_during_the_splash_stops_it_without_leaving_a_topmost_window() {
    // `CloseRequested` and `Destroyed` on the main window both take the splash down. Both arms live
    // in the builder's `.on_window_event(`, which is registered on the chain before `.build`, so it
    // covers the window `tauri.conf.json` creates. The slice ends at the build's own `expect`:
    // `.build(context)` is *not* usable as the end anchor because it is also named in the setup
    // comment above ("`.build(context)` below still needs"), and the first match wins.
    let start = at(MAIN_SOURCE, ".on_window_event(");
    let end = at(MAIN_SOURCE, "failed to initialize native shell");
    let events = MAIN_SOURCE.get(start..end).unwrap_or("");
    assert!(
        !events.is_empty(),
        "the window-event handler slice is empty: its anchors no longer span the builder chain"
    );
    // A count of two only means "both lifecycle arms stop the splash" if both arms are in the
    // slice, so pin the arms themselves rather than trusting the count alone.
    for arm in ["tauri::WindowEvent::CloseRequested", "tauri::WindowEvent::Destroyed"] {
        assert!(events.contains(arm), "the window-event handler no longer matches {arm}");
    }
    assert!(
        events.find("tauri::WindowEvent::CloseRequested").unwrap_or(usize::MAX)
            < events.find("tauri::WindowEvent::Destroyed").unwrap_or(usize::MAX),
        "the close arm must still be matched before the destroyed arm"
    );
    assert_eq!(
        events.matches("splash.stop();").count(),
        2,
        "closing and destroying the main window both stop the splash"
    );
    assert!(
        events.contains(r#"window.label() == "main""#),
        "only the main window's lifecycle touches the splash"
    );
    // A close before the handoff must not save the pre-restore rectangle over the saved state.
    assert!(
        events.contains("if REVEALED.load(Ordering::SeqCst)"),
        "the window state is only saved once the shell was handed over"
    );
    // The app's own close is still the app's close: the splash never cancels a `CloseRequested`.
    assert!(
        !events.contains("api.prevent_close()"),
        "a close must never be swallowed while the splash is up"
    );
}
