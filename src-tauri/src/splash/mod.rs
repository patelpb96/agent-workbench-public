//! Startup splash, ported from the reviewed standalone Win32 demo into an isolated module.
//!
//! Present today:
//! - `art` rasterises one frame into a 32-bit pixel buffer: a pure function of
//!   `(elapsed_ms, ArtOptions)`, including the reduced-motion and high-contrast decisions.
//! - `timeline` owns the reveal state machine — `(shell_ready && minimum show time) || deadline || failed` — with an
//!   idempotent, generation-scoped reveal.
//! - `window` (Windows only) is the Win32 layered window, its DIB and its message loop, on one
//!   dedicated thread.
//! - this module is the seam `main.rs` uses: one [`timeline::Timeline`] behind one mutex as the
//!   single source of truth, one monotonic clock both threads read, and the smoke-thin
//!   [`Splash`] handle.
//!
//! # Threading
//! `Timeline` is a plain `Copy` value; the only shared thing here is `Arc<Shared>`, which holds
//! the timeline, the reveal callback and the clock base. Every entry point takes the lock, reads
//! or advances the timeline, takes the callback if a reveal is happening *now*, and only then
//! invokes it, outside the lock, because the callback re-enters the seam. Nothing is `unwrap`ped
//! on the lock: a poisoned mutex is treated as "reveal immediately", so the shell can never be
//! stranded behind a splash whose state cannot be trusted.
//!
//! # Generations
//! A handle from an older run (a replay, a second attempt) can only reach the state it was
//! created with: each [`Splash`] owns its own timeline, its own stop flag and its own window
//! thread. "A stale generation must not touch a newer window" therefore holds by construction
//! rather than by a check, and `timeline`'s own `*_from(generation, ..)` entry points remain the
//! guard for callers that share one timeline.
//!
//! # Headless by default
//! The window lives behind the private `Sink` trait. Production supplies `window::Win32Sink` (a
//! detached Win32 thread); [`Splash::disabled`], a failed spawn and every test use the null sink
//! (`Splash::headless`), so the default `cargo test` never creates a window.

pub mod art;
pub mod timeline;
#[cfg(windows)]
mod window;

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use timeline::{Reveal, Timeline, MIN_SHOW_MS};

/// The hard deadline after the splash appeared: the splash may not outlive it, whatever the
/// shell is doing. Re-exported from [`timeline`] so callers have one name for it.
pub const DEADLINE_MS: u64 = timeline::DEADLINE_MS;

/// Why the splash gave way to the shell. Reported to the reveal callback so the handoff can be
/// logged without guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealReason {
    /// A page load finished in the main webview: the shell can take over.
    ShellReady,
    /// The 4000 ms budget ran out.
    Deadline,
    /// The splash could not be shown (class, window, DC/DIB, first present, or no window thread).
    Failed,
    /// A caller asked for the reveal outright.
    Manual,
    /// The splash mutex was poisoned; the splash is treated as revealed.
    LockPoisoned,
    /// The splash window disappeared on its own (a user close, or the system destroying it).
    WindowClosed,
}

impl RevealReason {
    /// A stable short label for the lifecycle log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShellReady => "shell_ready",
            Self::Deadline => "deadline",
            Self::Failed => "failed",
            Self::Manual => "manual",
            Self::LockPoisoned => "lock_poisoned",
            Self::WindowClosed => "window_closed",
        }
    }
}

/// The reveal handoff: whatever the caller wants to do once the splash is done (show the shell,
/// drop the topmost window, resume).
///
/// `Send` and not `Sync`: the callback is taken out of the mutex before it is invoked, so it is
/// never shared, which also keeps callers free to capture things that are `Send` but not `Sync`.
type RevealFn = Box<dyn Fn(RevealReason) + Send + 'static>;

/// The part of the splash that owns the OS window. Production supplies the Win32 thread; tests
/// and [`Splash::disabled`] supply [`NullSink`], so the whole seam is exercised with no window.
pub(crate) trait Sink: Send + Sync + 'static {
    /// Ask the window to end before its next frame. Idempotent, callable from any thread, and
    /// scoped to the windows this sink owns, so a stale handle cannot reach a newer one.
    fn stop(&self);
}

/// The sink of a splash that has no window at all.
struct NullSink;

impl Sink for NullSink {
    fn stop(&self) {}
}

/// The mutable state behind the one mutex: the timeline, the reveal callback until it fires, and
/// the reason the reveal happened.
struct State {
    timeline: Timeline,
    on_reveal: Option<RevealFn>,
    reason: Option<RevealReason>,
}

impl State {
    fn new(now: u64, on_reveal: Option<RevealFn>) -> Self {
        Self {
            timeline: Timeline::new(now, DEADLINE_MS),
            on_reveal,
            reason: None,
        }
    }

    /// Record why the reveal happened and take the callback so the caller can invoke it outside
    /// the lock. Once only: the callback is gone after the first reveal, so a second entry point
    /// racing the first can neither re-fire it nor report a second reason.
    fn take(&mut self, reason: RevealReason) -> Option<(RevealFn, RevealReason)> {
        self.reason.get_or_insert(reason);
        self.on_reveal.take().map(|callback| (callback, reason))
    }
}

/// Everything the seam shares with the window thread: the clock and the guarded state. Crate
/// internal: only `window` and the tests ever name it.
pub(crate) struct Shared {
    /// The moment the splash appeared, for the clock below.
    base: Instant,
    /// The caller's reading of its own monotonic clock when the splash started. Every later
    /// reading is `offset_ms + real elapsed`, so `now` values are continuous with the caller's
    /// clock (the deadline therefore fires `DEADLINE_MS` after the splash appeared, not after
    /// the process started).
    offset_ms: u64,
    state: Mutex<State>,
}

impl Shared {
    fn new(now: u64, on_reveal: Option<RevealFn>) -> Self {
        Self {
            base: Instant::now(),
            offset_ms: now,
            state: Mutex::new(State::new(now, on_reveal)),
        }
    }

    /// Milliseconds since the caller's clock started, saturating instead of wrapping.
    fn now(&self) -> u64 {
        let elapsed = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.offset_ms.saturating_add(elapsed)
    }

    /// Lock the state, tolerating poison. The flag says the previous holder panicked; the state
    /// itself is still readable, so the caller can reveal and be done with it.
    fn lock(&self) -> (MutexGuard<'_, State>, bool) {
        match self.state.lock() {
            Ok(guard) => (guard, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        }
    }

    /// Advance the timeline under the lock and fire the reveal callback afterwards, if this step
    /// produced a reveal. A poisoned lock always reveals: a splash that cannot report its own
    /// state must not stand between the user and the shell.
    fn advance<F>(&self, requested: RevealReason, step: F) -> Reveal
    where
        F: FnOnce(&mut Timeline, u64) -> Reveal,
    {
        let now = self.now();
        let (mut state, poisoned) = self.lock();
        let (reveal, fired) = if poisoned {
            // `reveal` is idempotent, so this is safe on a state that already revealed; `take`
            // still hands back a callback that has not fired yet, which is the case that matters.
            let _already = state.timeline.reveal();
            (Reveal::AlreadyRevealed, state.take(RevealReason::LockPoisoned))
        } else {
            let reveal = step(&mut state.timeline, now);
            let fired = match reveal {
                Reveal::Show => {
                    let reason = reason_for(&state.timeline, now, requested);
                    state.take(reason)
                }
                // Nothing to report: a `Wait` keeps drawing, `AlreadyRevealed` means the one
                // callback that exists has already fired.
                Reveal::Wait | Reveal::AlreadyRevealed => None,
            };
            (reveal, fired)
        };
        drop(state);
        if let Some((callback, reason)) = fired {
            callback(reason);
        }
        reveal
    }

    /// One tick attributed to the deadline. The window thread calls this once per frame, so the
    /// 4000 ms backstop is driven by the same clock as the animation.
    fn tick(&self) -> Reveal {
        // `Manual` is refined by `reason_for` into what actually held: the deadline, a ready shell whose minimum show
        // time has just ended, or a failure.
        self.advance(RevealReason::Manual, |timeline, now| timeline.tick(now))
    }

    /// Require the splash to stay up for the animation's length even when the shell is ready sooner.
    fn set_min_show(&self, ms: u64) {
        let (mut state, _poisoned) = self.lock();
        state.timeline = state.timeline.with_min_show(ms);
    }

    fn shell_ready(&self) {
        let _reveal = self.advance(RevealReason::ShellReady, |timeline, now| timeline.shell_ready(now));
    }

    fn failed(&self) {
        let _reveal = self.advance(RevealReason::Failed, |timeline, _now| timeline.failed());
    }

    fn reveal(&self) {
        let _reveal = self.advance(RevealReason::Manual, |timeline, _now| timeline.reveal());
    }

    /// The splash window is gone without a reveal having been asked for: report it, so the shell
    /// appears instead of the app sitting invisible behind nothing.
    fn window_closed(&self) {
        let _reveal = self.advance(RevealReason::WindowClosed, |timeline, _now| timeline.reveal());
    }

    /// End the splash without a handoff (the app is closing). The callback is dropped, so no
    /// late reveal can show a window the user already closed.
    fn cancel(&self) {
        let (mut state, _poisoned) = self.lock();
        let _already = state.timeline.reveal();
        state.on_reveal = None;
        drop(state);
    }

    /// Whether the splash should still be on screen. A poisoned lock reports "no": the caller
    /// then reveals (see `advance`) rather than drawing over a state it cannot read.
    fn is_showing(&self) -> bool {
        let (state, poisoned) = self.lock();
        !poisoned && state.timeline.should_draw_splash()
    }

    /// The reduced-motion system setting, as probed by the window thread at creation. It is an
    /// input to the art, never a bypass: see [`Timeline::set_reduced_motion`].
    fn set_reduced_motion(&self, reduced: bool) {
        let (mut state, _poisoned) = self.lock();
        state.timeline.set_reduced_motion(reduced);
    }
}

/// Why the splash gave way. A `Manual` request is refined into the predicate that actually held,
/// so a log never says "manual" when the deadline had already passed. The first reason wins:
/// the reveal is emitted once, so a later failure cannot rewrite the reason of a reveal that
/// already happened.
fn reason_for(timeline: &Timeline, now: u64, requested: RevealReason) -> RevealReason {
    match requested {
        RevealReason::Manual if timeline.is_failed() => RevealReason::Failed,
        RevealReason::Manual if timeline.is_shell_ready() => RevealReason::ShellReady,
        RevealReason::Manual if now >= timeline.deadline_at() => RevealReason::Deadline,
        other => other,
    }
}

/// Whether the environment asks for no splash at all.
///
/// The ruled opt-out is `AW_NO_SPLASH=1`; any other value (including empty, `0` and `false`)
/// leaves the splash on, so a typo never removes it silently. Reduced motion is deliberately not
/// an opt-out: it selects a static first frame ([`ArtOptions`](art::ArtOptions)) and the splash
/// still appears for as long as the shell needs.
pub fn opt_out_from(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1"))
}

/// The handle `main.rs` drives. Cheap to clone, safe from any thread, at any time, any number of
/// times, including after the splash is gone.
pub struct Splash {
    shared: Arc<Shared>,
    sink: Arc<dyn Sink>,
}

impl Clone for Splash {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            sink: Arc::clone(&self.sink),
        }
    }
}

impl Splash {
    /// Put the splash on screen and return at once; the window is created on its own thread.
    ///
    /// `now` is the caller's monotonic millisecond reading (for example
    /// `start.elapsed().as_millis()` from the instant the process started); the seam keeps its
    /// own clock from that offset, so the window thread and the main thread compare times on one
    /// clock. `on_reveal` is invoked exactly once, when the splash gives way, on whichever thread
    /// noticed (the page-load hook, a window event, or the window thread's own deadline tick) —
    /// a caller that must touch a window is responsible for hopping to the UI thread.
    ///
    /// If no window can be created at all, the callback fires straight away with
    /// [`RevealReason::Failed`], so the shell is never blocked behind a splash that is not there.
    pub fn start<F>(now: u64, on_reveal: F) -> Self
    where
        F: Fn(RevealReason) + Send + 'static,
    {
        Self::assemble(now, Some(Box::new(on_reveal)))
    }

    /// A splash that never exists: nothing is shown, nothing is spawned, no callback is invoked
    /// and every method is a no-op. For the `AW_NO_SPLASH=1` opt-out.
    pub fn disabled() -> Self {
        let splash = Self::headless(0, None);
        // There is no callback to fire; recording the reveal is all this does.
        splash.shared.reveal();
        splash
    }

    /// Build a splash around an optional callback, choosing the window back end. Used by `start`:
    /// this is the only path that can create a window.
    fn assemble(now: u64, on_reveal: Option<RevealFn>) -> Self {
        let shared = Arc::new(Shared::new(now, on_reveal));
        // The page usually loads in well under a second; without this the splash would be gone before it was seen.
        shared.set_min_show(MIN_SHOW_MS);
        let sink = spawn_sink(&shared);
        Self { shared, sink }
    }

    /// Build a splash whose back end is [`NullSink`]: no thread, no window, nothing to show. Used
    /// by [`Splash::disabled`] and by the tests, which is exactly why the window lives behind
    /// [`Sink`] — the default `cargo test` never creates a window.
    fn headless(now: u64, on_reveal: Option<RevealFn>) -> Self {
        Self {
            shared: Arc::new(Shared::new(now, on_reveal)),
            sink: Arc::new(NullSink),
        }
    }

    /// Milliseconds on the splash clock (`now` at `start`, plus real time since).
    pub fn now(&self) -> u64 {
        self.shared.now()
    }

    /// The shell can take over: reveal once the minimum show time (the animation) has passed, at once if it already has.
    /// Idempotent and safe from a
    /// stale handle, a poisoned lock or a closed splash.
    pub fn shell_ready(&self) {
        self.shared.shell_ready();
    }

    /// Reveal for any other reason (a skip key, the caller's own decision). Idempotent.
    pub fn reveal(&self) {
        self.shared.reveal();
    }

    /// The splash is broken: reveal at once so a broken splash never stands between the user and
    /// the shell. Idempotent.
    pub fn failed(&self) {
        self.shared.failed();
    }

    /// Take the splash down without a handoff, for shutdown: the splash window is destroyed and
    /// the reveal callback is dropped, so a late reveal cannot show a window on the way out.
    pub fn stop(&self) {
        self.shared.cancel();
        self.sink.stop();
    }

    /// Whether the splash should still be on screen. `false` before the first frame of a
    /// disabled splash, after any reveal, and after a poisoned lock.
    pub fn is_showing(&self) -> bool {
        self.shared.is_showing()
    }

    /// The reason the reveal happened, once it has; `None` while the splash is up.
    pub fn reason(&self) -> Option<RevealReason> {
        let (state, _poisoned) = self.shared.lock();
        state.reason
    }
}

/// Choose the window back end, and fail open when there is none.
fn spawn_sink(shared: &Arc<Shared>) -> Arc<dyn Sink> {
    #[cfg(windows)]
    if let Some(sink) = window::spawn(Arc::clone(shared)) {
        return Arc::new(sink);
    }
    // Not Windows, or the window thread could not be started: there is no splash to wait for, so
    // report the failure now and let the caller show the shell.
    shared.failed();
    Arc::new(NullSink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        thread,
    };

    /// A callback that records every reason it is given.
    fn recorder() -> (Arc<Mutex<Vec<RevealReason>>>, impl Fn(RevealReason) + Send + 'static) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (seen, move |reason| {
            if let Ok(mut seen) = sink.lock() {
                seen.push(reason);
            }
        })
    }

    fn seen(recorded: &Arc<Mutex<Vec<RevealReason>>>) -> Vec<RevealReason> {
        recorded.lock().map(|seen| seen.clone()).unwrap_or_default()
    }

    #[test]
    fn a_disabled_splash_is_never_showing_and_never_calls_back() {
        let splash = Splash::disabled();
        assert!(!splash.is_showing());
        // Everything stays harmless and silent, however often and from wherever it is called.
        splash.shell_ready();
        splash.reveal();
        splash.failed();
        assert!(!splash.is_showing());
        splash.stop();
        assert!(!splash.is_showing());
        assert_eq!(splash.reason(), Some(RevealReason::Manual));
    }

    #[test]
    fn the_opt_out_is_exactly_aw_no_splash_equals_one() {
        assert!(opt_out_from(Some("1")));
        assert!(opt_out_from(Some(" 1 ")), "a padded value still opts out");
        for value in [Some("0"), Some(""), Some("false"), Some("true"), Some("yes"), None] {
            assert!(!opt_out_from(value), "{value:?} must leave the splash on");
        }
    }

    #[test]
    fn the_reveal_callback_fires_once_however_often_the_seam_is_poked() {
        let (recorded, callback) = recorder();
        let splash = Splash::headless(0, Some(Box::new(callback)));
        assert!(splash.is_showing());
        assert_eq!(splash.reason(), None, "nothing has happened yet");

        splash.shell_ready();
        assert_eq!(seen(&recorded), vec![RevealReason::ShellReady]);
        assert!(!splash.is_showing());
        assert_eq!(splash.reason(), Some(RevealReason::ShellReady));

        // Every later poke is a no-op, including one that would otherwise report a failure.
        splash.shell_ready();
        splash.reveal();
        splash.failed();
        splash.shell_ready();
        assert_eq!(seen(&recorded), vec![RevealReason::ShellReady], "exactly one handoff");
        assert_eq!(splash.reason(), Some(RevealReason::ShellReady), "the first reason wins");
    }

    #[test]
    fn a_failure_before_the_shell_is_ready_reveals_with_the_failure_reason() {
        let (recorded, callback) = recorder();
        let splash = Splash::headless(0, Some(Box::new(callback)));
        splash.failed();
        assert_eq!(seen(&recorded), vec![RevealReason::Failed]);
        assert_eq!(splash.reason(), Some(RevealReason::Failed));
    }

    #[test]
    fn stop_ends_the_splash_without_a_handoff() {
        let (recorded, callback) = recorder();
        let splash = Splash::headless(0, Some(Box::new(callback)));
        assert!(splash.is_showing());
        splash.stop();
        assert!(!splash.is_showing(), "the window goes");
        assert!(seen(&recorded).is_empty(), "closing the app wants no handoff");
        // A late reveal after the stop cannot call back either.
        splash.shell_ready();
        splash.reveal();
        assert!(seen(&recorded).is_empty(), "no reveal survives the stop");
    }

    #[test]
    fn the_clock_advances_from_the_callers_reading() {
        let splash = Splash::headless(1_000, None);
        let first = splash.now();
        assert!((1_000..1_100).contains(&first), "the caller's offset is the base: {first}");
        assert!(splash.now() >= first, "and the clock only moves forward");
    }

    #[test]
    fn a_handle_from_an_older_run_cannot_touch_the_current_splash() {
        let (older_recorded, older_callback) = recorder();
        let (newer_recorded, newer_callback) = recorder();
        let older = Splash::headless(0, Some(Box::new(older_callback)));
        let newer = Splash::headless(0, Some(Box::new(newer_callback)));

        older.stop();
        older.reveal();
        older.shell_ready();
        assert!(older_recorded.lock().map(|seen| seen.is_empty()).unwrap_or(false));
        assert!(newer.is_showing(), "the stale handle left the current splash alone");
        assert_eq!(newer.reason(), None);

        newer.shell_ready();
        assert_eq!(seen(&newer_recorded), vec![RevealReason::ShellReady]);
        assert!(
            older_recorded.lock().map(|seen| seen.is_empty()).unwrap_or(false),
            "and the current splash did not call the stale callback"
        );
    }

    #[test]
    fn a_manual_reveal_reports_the_predicate_that_actually_held() {
        // Nothing holds: the caller simply asked.
        let idle = Timeline::new(0, DEADLINE_MS);
        assert_eq!(reason_for(&idle, 10, RevealReason::Manual), RevealReason::Manual);
        // The deadline alone.
        assert_eq!(reason_for(&idle, DEADLINE_MS, RevealReason::Manual), RevealReason::Deadline);
        // A ready shell outranks the deadline.
        let mut ready = Timeline::new(0, DEADLINE_MS);
        assert_eq!(ready.shell_ready(5), Reveal::Show);
        assert_eq!(reason_for(&ready, 99_999, RevealReason::Manual), RevealReason::ShellReady);
        // A failure outranks both, however late it is asked.
        let mut all = Timeline::new(0, DEADLINE_MS);
        assert_eq!(all.shell_ready(1), Reveal::Show);
        assert_eq!(all.failed(), Reveal::AlreadyRevealed);
        assert_eq!(reason_for(&all, 99_999, RevealReason::Manual), RevealReason::Failed);
        // The interesting causes are reported verbatim: they are known to the caller, not to the
        // timeline.
        for cause in [
            RevealReason::ShellReady,
            RevealReason::Deadline,
            RevealReason::Failed,
            RevealReason::LockPoisoned,
            RevealReason::WindowClosed,
        ] {
            assert_eq!(reason_for(&idle, 0, cause), cause);
        }
    }

    #[test]
    fn a_poisoned_lock_reveals_the_splash_instead_of_stranding_it() {
        let (recorded, callback) = recorder();
        let splash = Splash::headless(0, Some(Box::new(callback)));
        assert!(splash.is_showing());

        // Poison the mutex the only way Rust allows: panic while holding the guard. The panic is
        // deliberate and caught by the thread boundary, so only the mutex is harmed.
        let shared = Arc::clone(&splash.shared);
        let (ready_tx, ready_rx) = mpsc::channel();
        let poisoner = thread::spawn(move || {
            // Holding the guard across the panic is what poisons the mutex.
            let _guard = shared.lock();
            let _ = ready_tx.send(());
            panic!("deliberate: poisons the splash mutex");
        });
        ready_rx.recv().ok();
        assert!(poisoner.join().is_err(), "the poisoner must have panicked");

        assert!(!splash.is_showing(), "an unreadable state is not on screen");
        splash.shell_ready();
        assert_eq!(
            seen(&recorded),
            vec![RevealReason::LockPoisoned],
            "the app is handed the shell, once"
        );
        splash.shell_ready();
        splash.reveal();
        assert_eq!(seen(&recorded), vec![RevealReason::LockPoisoned], "still exactly once");
    }

    #[test]
    fn every_reason_has_a_stable_label() {
        let reasons = [
            RevealReason::ShellReady,
            RevealReason::Deadline,
            RevealReason::Failed,
            RevealReason::Manual,
            RevealReason::LockPoisoned,
            RevealReason::WindowClosed,
        ];
        let mut labels: Vec<&str> = reasons.iter().map(|reason| reason.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), reasons.len(), "no two reasons share a label");
        assert!(labels.iter().all(|label| !label.contains(' ')), "labels are log-safe");
    }
}
