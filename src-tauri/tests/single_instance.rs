//! Single-instance launch gate: the pure policy rules, plus the real kernel mutex.
//!
//! Nothing here starts a GUI or a second process. `launch` itself needs a live desktop: the
//! focus redirect and the exit latency after a successful raise are acceptance steps in
//! `docs/design/startup-login-plan.md` (P1), not tests. The decision to stop as soon as a window
//! has been raised is part of the pure policy, so it is tested here without a desktop.
#![cfg(windows)]

use agent_workbench::integration::SHUTDOWN_BOUND;
use agent_workbench::single_instance::{
    choose, mutex_name, same_image, show_command, startup, try_claim, Attempt, Candidate, Step,
    RECLAIM_BUDGET, RECLAIM_INTERVAL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SW_SHOW};
use std::{ptr::null_mut, time::Duration};

const MAIN_SOURCE: &str = include_str!("../src/main.rs");

fn candidate(image: &str, title: &str, visible: bool, minimized: bool) -> Candidate {
    Candidate {
        window: null_mut(),
        image: image.to_string(),
        title: title.to_string(),
        visible,
        minimized,
    }
}

#[test]
fn raising_restores_only_a_minimized_window_and_keeps_a_maximized_one_maximized() {
    // SW_RESTORE on a maximized window un-maximizes it; a second launch must not shrink the app.
    assert_eq!(show_command(true), SW_RESTORE);
    assert_eq!(show_command(false), SW_SHOW);
}

#[test]
fn mutex_name_is_session_scoped_and_built_from_the_identifier() {
    assert_eq!(
        mutex_name("local.agent-workbench.spike"),
        r"Local\agent-workbench.single-instance.local.agent-workbench.spike"
    );
    assert!(mutex_name("com.other.app").starts_with(r"Local\"));
    // Different identifiers must not share a slot, including when only case or length differs.
    assert_ne!(mutex_name("com.other.app"), mutex_name("com.other.App"));
    assert_ne!(mutex_name("com.other.app"), mutex_name("com.other.app2"));
    assert_ne!(mutex_name("com.other.app"), mutex_name(""));
}

#[test]
fn mutex_name_cannot_escape_the_session_namespace_or_end_early() {
    // One separator only, the `Local\` scope: a second would address a nested namespace.
    assert_eq!(mutex_name(r"com\other app").matches('\\').count(), 1);
    assert!(mutex_name(r"com\other app").ends_with("com_other_app"));
    assert!(mutex_name(r"\\Global\evil").ends_with("_Global_evil"));
    // A NUL would silently end the name at the OS boundary.
    assert!(!mutex_name("com\0other").contains('\0'));
    assert!(mutex_name("com\0other").ends_with("com_other"));
}

#[test]
fn same_image_ignores_case_prefix_trailing_separator_and_padding() {
    let image = r"C:\Program Files\Agent Workbench\agent-workbench.exe";
    assert!(same_image(image, image));
    assert!(same_image(image, r"c:\program files\agent workbench\AGENT-WORKBENCH.EXE"));
    assert!(same_image(image, r"\\?\C:\Program Files\Agent Workbench\agent-workbench.exe"));
    assert!(same_image(image, r"C:\Program Files\Agent Workbench\agent-workbench.exe\"));
    assert!(same_image(image, r"  C:\Program Files\Agent Workbench\agent-workbench.exe  "));
    assert!(!same_image(image, r"C:\Program Files\Agent Workbench\agent-workbench2.exe"));
    assert!(!same_image(image, r"D:\Agent Workbench\agent-workbench.exe"));
}

#[test]
fn same_image_never_matches_an_unknown_image() {
    // An unreadable image path must never be treated as "ours", or a foreign window is raised.
    assert!(!same_image("", ""));
    assert!(!same_image(r"C:\Apps\Agent Workbench\agent-workbench.exe", ""));
    assert!(!same_image("", r"C:\Apps\Agent Workbench\agent-workbench.exe"));
    assert!(!same_image("   ", r"C:\Apps\Agent Workbench\agent-workbench.exe"));
}

#[test]
fn choose_refuses_windows_of_other_images_and_hidden_windows() {
    let mine = r"C:\Apps\Agent Workbench\agent-workbench.exe";
    let title = "Agent Workbench - Shell Spike";
    let notepad = r"C:\Windows\System32\notepad.exe";
    // A foreign process that happens to share the title is still refused.
    assert!(choose(&[candidate(notepad, title, true, false)], mine, title).is_none());
    // A hidden window of our own image is not a target either.
    assert!(choose(&[candidate(mine, title, false, false)], mine, title).is_none());
    assert!(choose(&[], mine, title).is_none());
    let windows = [
        candidate(notepad, title, true, false),
        candidate(mine, title, false, false),
        candidate(mine, title, true, false),
    ];
    let chosen = choose(&windows, mine, title);
    assert!(matches!(chosen, Some(window) if window.image == mine));
}

#[test]
fn choose_prefers_the_configured_title_then_a_restored_window() {
    let mine = r"C:\Apps\Agent Workbench\agent-workbench.exe";
    let title = "Agent Workbench - Shell Spike";
    let windows = [
        candidate(mine, "Background", true, false),
        candidate(mine, title, true, true),
        candidate(mine, title, true, false),
    ];
    let best = choose(&windows, mine, title);
    assert!(
        matches!(best, Some(window) if window.title == title && !window.minimized),
        "the restored main window wins"
    );
    // A minimized main window still beats a restored helper dialog: raising it restores it.
    let smaller = [candidate(mine, "Background", true, false), candidate(mine, title, true, true)];
    let minimized = choose(&smaller, mine, title);
    assert!(
        matches!(minimized, Some(window) if window.minimized),
        "a title match outranks a restored helper window"
    );
    // Nothing to prefer: the first candidate in enumeration order wins, so retries are stable.
    let ordered = [candidate(mine, "One", true, false), candidate(mine, "Two", true, false)];
    let fallback = choose(&ordered, mine, title);
    assert_eq!(fallback.map(|window| window.title.as_str()), Some("One"));
}

#[test]
fn startup_runs_when_the_slot_is_free_or_unreadable() {
    assert_eq!(startup(Attempt::Claimed, Duration::ZERO, false), Step::Run);
    assert_eq!(startup(Attempt::Claimed, RECLAIM_BUDGET, false), Step::Run);
    // A raise cannot happen when there is no holder to raise, so `focused` must not matter.
    assert_eq!(startup(Attempt::Claimed, Duration::ZERO, true), Step::Run);
    assert_eq!(startup(Attempt::Claimed, RECLAIM_BUDGET, true), Step::Run);
    // Fail open: an unusable mutex must never stop the app from starting.
    assert_eq!(startup(Attempt::Unavailable, Duration::ZERO, false), Step::Run);
    assert_eq!(startup(Attempt::Unavailable, RECLAIM_BUDGET * 2, false), Step::Run);
    assert_eq!(startup(Attempt::Unavailable, Duration::ZERO, true), Step::Run);
    assert_eq!(startup(Attempt::Unavailable, RECLAIM_BUDGET * 2, true), Step::Run);
}

#[test]
fn startup_retries_inside_the_reclaim_budget_and_yields_at_the_boundary() {
    assert_eq!(startup(Attempt::Held, Duration::ZERO, false), Step::Retry);
    assert_eq!(
        startup(Attempt::Held, RECLAIM_BUDGET - Duration::from_millis(1), false),
        Step::Retry
    );
    assert_eq!(startup(Attempt::Held, RECLAIM_BUDGET, false), Step::Yield);
    assert_eq!(startup(Attempt::Held, RECLAIM_BUDGET + RECLAIM_INTERVAL, false), Step::Yield);
}

#[test]
fn startup_stops_at_once_when_the_running_instance_was_raised() {
    // At zero elapsed time the budget would still say "retry"; a raised window must win,
    // because an invisible duplicate left waiting behind a visible Workbench is the P1 bug.
    assert_eq!(startup(Attempt::Held, Duration::ZERO, true), Step::Yield);
    assert_eq!(startup(Attempt::Held, Duration::ZERO, false), Step::Retry);
    // The raise ends the wait at every elapsed time, not only early in the budget.
    assert_eq!(startup(Attempt::Held, RECLAIM_INTERVAL, true), Step::Yield);
    assert_eq!(
        startup(Attempt::Held, RECLAIM_BUDGET - Duration::from_millis(1), true),
        Step::Yield
    );
    assert_eq!(startup(Attempt::Held, RECLAIM_BUDGET, true), Step::Yield);
    assert_eq!(startup(Attempt::Held, RECLAIM_BUDGET * 2, true), Step::Yield);
}

#[test]
fn reclaim_budget_outlasts_the_bounded_graceful_close() {
    let graceful_close = SHUTDOWN_BOUND + Duration::from_secs(1);
    assert!(
        RECLAIM_BUDGET > graceful_close + RECLAIM_INTERVAL,
        "a reopen during the graceful close must still take over"
    );
    assert!(RECLAIM_INTERVAL < RECLAIM_BUDGET, "the probe interval must not outlast the budget");
    assert_eq!(RECLAIM_BUDGET, Duration::from_secs(12), "the accepted budget is 12 s");
}

#[test]
fn the_named_mutex_is_exclusive_and_freed_when_the_claim_drops() {
    // A per-process identifier: no interference from a real launch or a parallel test run.
    let identifier = format!("local.single-instance-test.{}", std::process::id());
    let (attempt, first) = try_claim(&identifier);
    assert_eq!(attempt, Attempt::Claimed);
    assert!(first.is_some(), "the first claim holds a handle");
    let (attempt, second) = try_claim(&identifier);
    assert_eq!(attempt, Attempt::Held);
    assert!(
        second.is_none(),
        "a refused probe must not keep a handle, or the name would never free"
    );
    drop(first);
    let (attempt, third) = try_claim(&identifier);
    assert_eq!(
        attempt,
        Attempt::Claimed,
        "the name frees as soon as the owner's claim is dropped"
    );
    drop(third);
}

/// Index of `needle`; the assertion makes a renamed marker fail on this line with the marker named.
fn at(source: &str, needle: &str) -> usize {
    assert!(source.contains(needle), "main.rs no longer contains {needle}");
    source.find(needle).unwrap_or(usize::MAX)
}

#[test]
fn main_gates_a_second_instance_before_any_app_state_is_built() {
    // A duplicate launch must not create a window, start the coordinator, register IPC or
    // touch terminal/ghost state, so the gate has to precede the builder chain and its setup.
    let gate = at(MAIN_SOURCE, "single_instance::launch(");
    assert_eq!(MAIN_SOURCE.matches("single_instance::launch(").count(), 1, "exactly one gate");
    assert_eq!(
        MAIN_SOURCE.matches("tauri::generate_context!()").count(),
        1,
        "the context is hoisted, not generated twice"
    );
    assert!(
        !MAIN_SOURCE.contains(".build(tauri::generate_context!())"),
        "the builder must take the hoisted context"
    );
    assert!(
        at(MAIN_SOURCE, "tauri::generate_context!()") < gate,
        "the identifier and window title come from the hoisted context"
    );
    let builder = at(MAIN_SOURCE, "tauri::Builder::default()");
    assert!(gate < builder, "the gate precedes the builder");
    assert!(gate < at(MAIN_SOURCE, ".setup("), "no setup work runs before the gate");
    assert!(gate < at(MAIN_SOURCE, ".build(context)"), "the app is built from the hoisted context");
    let gated = MAIN_SOURCE.get(gate..builder).unwrap_or("");
    assert!(gated.contains("Startup::Duplicate"), "the duplicate branch lives inside the gate");
    assert!(gated.contains("return"), "a duplicate launch leaves without building the app");
}
