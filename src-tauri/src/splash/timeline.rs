//! The splash lifecycle: a pure state machine that decides when the splash gives way to
//! the shell. No Win32, no Tauri types, no threads and no clock — every entry point takes
//! the caller's monotonic millisecond timestamp — so the whole lifecycle is unit-tested
//! headlessly.
//!
//! The predicate: **reveal when `(shell_ready && the minimum show time has passed) ||
//! deadline_expired || failed`**, with a hard deadline of `DEADLINE_MS` (4000 ms). The shell
//! being ready is what ends the splash, but not before the animation has been seen: in the
//! app the minimum show time is the whole animation (`MIN_SHOW_MS`, 2800 ms; a second for
//! reduced motion), because the page usually loads in under 100 ms and without a hold the
//! splash would be gone before anyone saw it (observed on the first real launch, 2026-10-23;
//! the first design revealed the instant the shell was ready). A `Timeline` built with
//! `Timeline::new` has no minimum, which is what the pure tests below use; the app asks for
//! one with `with_min_show`. Finished artwork alone still never reveals, and a failure or the
//! deadline always does.
//!
//! Every reveal is idempotent and generation-scoped: `Reveal::Show` is emitted at most once
//! per generation, and a callback carrying an older generation can neither reveal nor
//! complete nor fail the current one, so a late timer or paint from a closed splash cannot
//! resurrect anything.
//!
//! `now` is a monotonic millisecond value on one clock, for example
//! `start.elapsed().as_millis() as u64` from the caller's start instant. Times are compared,
//! never subtracted across different clocks, and additions saturate.

/// The hard deadline after `started_at`: the splash may not outlive it, whatever the shell
/// is doing.
pub const DEADLINE_MS: u64 = 4000;

/// How long the splash is shown at least, once the shell is ready: the length of the animation
/// (`art::DURATION_MS`, checked by a test). Set to 0 to go back to "reveal the moment the shell
/// is ready".
pub const MIN_SHOW_MS: u64 = 2800;

/// The same for reduced motion: the final composition is a static frame, shown for a second.
pub const REDUCED_MIN_SHOW_MS: u64 = 1000;

/// What the caller should do with the splash after handing over an event or a tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// Not yet: keep the splash up and keep ticking.
    Wait,
    /// Reveal now — hide the splash and let the shell take over. Emitted exactly once per
    /// generation.
    Show,
    /// A reveal already happened earlier (this generation or an older one). Nothing to do:
    /// no second window transition, no second focus change.
    AlreadyRevealed,
}

/// The splash lifecycle. Create it when the splash appears, feed it every interesting event,
/// and act on the returned `Reveal`.
#[derive(Clone, Copy, Debug)]
pub struct Timeline {
    generation: u64,
    started_at: u64,
    deadline_at: u64,
    budget_ms: u64,
    /// The shortest the splash stays up once the shell is ready (0: reveal at once).
    min_show_ms: u64,
    art_done: bool,
    shell_ready: bool,
    failed: bool,
    reduced_motion: bool,
    revealed: bool,
}

impl Timeline {
    /// Start a new timeline at `now`. `deadline_ms` is a budget measured from `now`, so the
    /// reveal fires no later than `now + deadline_ms`; pass `DEADLINE_MS` for the ruled
    /// 4000 ms. `now` must come from the same monotonic clock as every later call.
    pub fn new(now: u64, deadline_ms: u64) -> Self {
        Self {
            generation: 1,
            started_at: now,
            deadline_at: now.saturating_add(deadline_ms),
            budget_ms: deadline_ms,
            min_show_ms: 0,
            art_done: false,
            shell_ready: false,
            failed: false,
            reduced_motion: false,
            revealed: false,
        }
    }

    /// The generation this timeline is on. It starts at 1 and only `restart` changes it.
    /// Require the splash to stay up at least `ms` after it started, even when the shell is
    /// ready sooner.
    pub fn with_min_show(mut self, ms: u64) -> Self {
        self.min_show_ms = ms;
        self
    }

    /// When the minimum show time ends.
    pub fn min_show_until(&self) -> u64 {
        self.started_at.saturating_add(self.min_show_ms)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True when `generation` belongs to an older run than the current one. Callers holding
    /// a generation (a timer, a paint callback) check this, or use the `*_from` entry points
    /// which check it for them.
    pub fn is_stale(&self, generation: u64) -> bool {
        generation != self.generation
    }

    /// When the animation began.
    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    /// The absolute time of the hard deadline.
    pub fn deadline_at(&self) -> u64 {
        self.deadline_at
    }

    /// The deadline budget this timeline was created with.
    pub fn budget_ms(&self) -> u64 {
        self.budget_ms
    }

    /// Whether the artwork reported its final frame. Informational: art never gates the
    /// reveal, it only tells the caller whether more frames would change anything.
    pub fn is_art_done(&self) -> bool {
        self.art_done
    }

    /// Whether the shell reported that it can take over.
    pub fn is_shell_ready(&self) -> bool {
        self.shell_ready
    }

    /// Whether the splash was reported as failed (and therefore skipped).
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Whether a reveal has already been emitted for the current generation.
    pub fn is_revealed(&self) -> bool {
        self.revealed
    }

    /// The reduced-motion input as last set.
    pub fn reduced_motion(&self) -> bool {
        self.reduced_motion
    }

    /// Record that the artwork reached its final frame and re-evaluate. Art alone never
    /// reveals: the shell must be ready or the deadline must pass.
    pub fn art_done(&mut self, now: u64) -> Reveal {
        self.art_done = true;
        self.tick(now)
    }

    /// Record that the shell can take over and re-evaluate. This is the fast path: it
    /// reveals immediately, even if the animation is still running.
    pub fn shell_ready(&mut self, now: u64) -> Reveal {
        self.shell_ready = true;
        self.tick(now)
    }

    /// Record that the splash failed, for a caller with no useful timestamp. Failure reveals
    /// immediately: a broken splash must never stand between the user and the shell.
    pub fn failed(&mut self) -> Reveal {
        self.failed = true;
        self.reveal()
    }

    /// As `failed`, for a caller that does have a timestamp.
    pub fn fail_now(&mut self, now: u64) -> Reveal {
        self.failed = true;
        self.tick(now)
    }

    /// Re-evaluate at `now`. Returns `Wait` while the predicate is false and `Show` once it
    /// holds, after which every further call returns `AlreadyRevealed`.
    pub fn tick(&mut self, now: u64) -> Reveal {
        if self.revealed {
            return Reveal::AlreadyRevealed;
        }
        if self.should_reveal_at(now) {
            self.reveal()
        } else {
            Reveal::Wait
        }
    }

    /// Reveal unconditionally (the user pressed skip, or a caller decided on its own). The
    /// second and every later call is a no-op.
    pub fn reveal(&mut self) -> Reveal {
        if self.revealed {
            Reveal::AlreadyRevealed
        } else {
            self.revealed = true;
            Reveal::Show
        }
    }

    /// The exact predicate, readable from outside for logging and testing: reveal when
    /// `(shell_ready && now >= the end of the minimum show time) || deadline_expired ||
    /// failed`. Artwork completion is deliberately absent: the minimum show time is what
    /// holds the splash, and it is measured from the start, not from the art finishing.
    pub fn should_reveal_at(&self, now: u64) -> bool {
        (self.shell_ready && now >= self.min_show_until()) || now >= self.deadline_at || self.failed
    }

    /// Whether the splash should still be on screen. A reveal is final for the generation,
    /// so this turns false exactly once per run.
    pub fn should_draw_splash(&self) -> bool {
        !self.revealed
    }

    /// `should_draw_splash`, evaluated against `now` as well: drawing also stops as soon as
    /// the reveal predicate holds, even if `tick` has not run yet.
    pub fn should_draw_splash_at(&self, now: u64) -> bool {
        !self.revealed && !self.should_reveal_at(now)
    }

    /// Feed the reduced-motion system setting. It is an input, never a bypass: when it is on,
    /// the artwork counts as already complete (a static final frame is the whole animation),
    /// but the reveal predicate is untouched, so a static splash still waits for the shell or
    /// the deadline. Turning it off again does not un-finish completed artwork.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
        if reduced {
            self.art_done = true;
            // A static frame needs less time on screen than an animation (never more than was asked).
            self.min_show_ms = self.min_show_ms.min(REDUCED_MIN_SHOW_MS);
        }
    }

    /// Begin a NEW splash generation at `now` (a replay, or a second attempt). The flags
    /// reset to a clean run, the deadline is re-armed from `now`, and the generation counter
    /// advances, which invalidates every outstanding callback from the previous run. The
    /// reduced-motion setting survives, because it is a system setting and not run state.
    /// Returns the new generation.
    pub fn restart(&mut self, now: u64) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.started_at = now;
        self.deadline_at = now.saturating_add(self.budget_ms);
        self.art_done = self.reduced_motion;
        self.shell_ready = false;
        self.failed = false;
        self.revealed = false;
        self.generation
    }

    /// What a caller from a superseded generation is told: nothing it did took effect, and
    /// the only interesting fact is whether a reveal already happened.
    fn stale_response(&self) -> Reveal {
        if self.revealed {
            Reveal::AlreadyRevealed
        } else {
            Reveal::Wait
        }
    }

    /// Generation-scoped `tick`: a stale caller cannot reveal the current run.
    pub fn tick_from(&mut self, generation: u64, now: u64) -> Reveal {
        if self.is_stale(generation) {
            return self.stale_response();
        }
        self.tick(now)
    }

    /// Generation-scoped `art_done`.
    pub fn art_done_from(&mut self, generation: u64, now: u64) -> Reveal {
        if self.is_stale(generation) {
            return self.stale_response();
        }
        self.art_done(now)
    }

    /// Generation-scoped `shell_ready`.
    pub fn shell_ready_from(&mut self, generation: u64, now: u64) -> Reveal {
        if self.is_stale(generation) {
            return self.stale_response();
        }
        self.shell_ready(now)
    }

    /// Generation-scoped `fail_now`.
    pub fn fail_from(&mut self, generation: u64, now: u64) -> Reveal {
        if self.is_stale(generation) {
            return self.stale_response();
        }
        self.fail_now(now)
    }

    /// Generation-scoped `reveal`.
    pub fn reveal_from(&mut self, generation: u64) -> Reveal {
        if self.is_stale(generation) {
            return self.stale_response();
        }
        self.reveal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_is_exactly_4000_ms() {
        assert_eq!(DEADLINE_MS, 4000, "the ruled hard deadline");
        let t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.started_at(), 0);
        assert_eq!(t.deadline_at(), 4000);
        assert_eq!(t.budget_ms(), 4000);
        assert_eq!(Timeline::new(1_000, DEADLINE_MS).deadline_at(), 5_000);
        assert_eq!(
            Timeline::new(u64::MAX, DEADLINE_MS).deadline_at(),
            u64::MAX,
            "a saturating add, never a wrap"
        );
    }

    #[test]
    fn reveal_fires_on_shell_ready_before_the_art_is_done() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.tick(10), Reveal::Wait);
        assert_eq!(t.shell_ready(120), Reveal::Show, "a ready shell wins over unfinished art");
        assert!(t.is_shell_ready());
        assert!(!t.is_art_done(), "the reveal did not wait for the animation");
        assert!(!t.should_draw_splash());
        assert_eq!(t.tick(121), Reveal::AlreadyRevealed);
    }

    #[test]
    fn reveal_fires_at_the_deadline_and_not_one_millisecond_earlier() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.tick(3_999), Reveal::Wait, "still inside the budget");
        assert!(t.should_draw_splash_at(3_999));
        assert_eq!(t.tick(4_000), Reveal::Show, "the deadline is the backstop");
        let mut offset = Timeline::new(1_000, DEADLINE_MS);
        assert_eq!(offset.tick(4_999), Reveal::Wait);
        assert_eq!(offset.tick(5_000), Reveal::Show);
    }

    #[test]
    fn reveal_fires_on_failure() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.failed(), Reveal::Show);
        assert!(t.is_failed());
        assert!(!t.should_draw_splash());
        let mut timestamped = Timeline::new(0, DEADLINE_MS);
        assert_eq!(timestamped.fail_now(7), Reveal::Show);
        assert_eq!(timestamped.fail_now(8), Reveal::AlreadyRevealed, "failure reveals once");
    }

    #[test]
    fn art_done_alone_does_not_reveal_before_the_shell_is_ready() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.art_done(2_800), Reveal::Wait, "the animation finished, the shell did not");
        assert!(t.is_art_done());
        assert!(!t.is_shell_ready());
        assert!(t.should_draw_splash(), "the final frame stays on screen");
        assert!(t.should_draw_splash_at(3_500));
        assert_eq!(t.tick(3_999), Reveal::Wait);
        assert_eq!(t.tick(4_000), Reveal::Show, "the deadline is still the backstop");
    }

    #[test]
    fn a_ready_shell_waits_for_the_minimum_show_time_and_then_reveals_without_another_event() {
        let mut t = Timeline::new(0, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        assert_eq!(t.min_show_until(), 2_800);
        assert_eq!(t.shell_ready(80), Reveal::Wait, "a ready shell does not cut the animation short");
        assert!(t.is_shell_ready());
        assert!(t.should_draw_splash_at(2_799), "still drawing one millisecond before the end");
        assert_eq!(t.tick(2_799), Reveal::Wait);
        assert_eq!(t.tick(2_800), Reveal::Show, "the next tick after the minimum reveals it");
        assert!(!t.should_draw_splash());
        assert_eq!(t.tick(2_801), Reveal::AlreadyRevealed);
    }

    #[test]
    fn a_shell_that_is_ready_after_the_minimum_reveals_at_once() {
        let mut t = Timeline::new(0, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        assert_eq!(t.tick(2_000), Reveal::Wait);
        assert_eq!(t.shell_ready(3_100), Reveal::Show);
    }

    #[test]
    fn the_minimum_never_outlasts_the_deadline_failure_or_a_manual_reveal() {
        let mut slow = Timeline::new(0, DEADLINE_MS).with_min_show(5_000);
        assert_eq!(slow.shell_ready(10), Reveal::Wait);
        assert_eq!(slow.tick(3_999), Reveal::Wait);
        assert_eq!(slow.tick(4_000), Reveal::Show, "even an over-long minimum is cut off by the deadline");
        let mut failed = Timeline::new(0, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        assert_eq!(failed.failed(), Reveal::Show, "a failure ignores the minimum");
        let mut manual = Timeline::new(0, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        assert_eq!(manual.reveal(), Reveal::Show, "so does an explicit reveal");
    }

    #[test]
    fn the_minimum_is_measured_from_the_start_and_survives_a_restart() {
        let mut t = Timeline::new(1_000, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        assert_eq!(t.min_show_until(), 3_800);
        t.restart(10_000);
        assert_eq!(t.min_show_until(), 12_800, "re-armed from the restart");
        assert_eq!(t.shell_ready(10_050), Reveal::Wait);
        assert_eq!(t.tick(12_800), Reveal::Show);
    }

    #[test]
    fn reduced_motion_shortens_the_minimum_but_never_lengthens_it() {
        let mut t = Timeline::new(0, DEADLINE_MS).with_min_show(MIN_SHOW_MS);
        t.set_reduced_motion(true);
        assert_eq!(t.min_show_until(), REDUCED_MIN_SHOW_MS);
        assert_eq!(t.shell_ready(5), Reveal::Wait);
        assert_eq!(t.tick(REDUCED_MIN_SHOW_MS), Reveal::Show);
        let mut none = Timeline::new(0, DEADLINE_MS);
        none.set_reduced_motion(true);
        assert_eq!(none.min_show_until(), 0, "a timeline with no minimum keeps none");
    }

    #[test]
    fn the_minimum_is_the_length_of_the_animation() {
        assert_eq!(MIN_SHOW_MS, crate::splash::art::DURATION_MS);
        // Both are constants, so this is checked when the test compiles.
        const _: () = assert!(MIN_SHOW_MS < DEADLINE_MS, "the animation fits inside the hard deadline");
    }

    #[test]
    fn a_second_reveal_is_a_no_op() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert_eq!(t.reveal(), Reveal::Show);
        assert_eq!(t.reveal(), Reveal::AlreadyRevealed);
        assert_eq!(t.shell_ready(1), Reveal::AlreadyRevealed);
        assert_eq!(t.fail_now(2), Reveal::AlreadyRevealed);
        assert_eq!(t.tick(4_000), Reveal::AlreadyRevealed);
        assert_eq!(t.generation(), 1, "a reveal never changes the generation");
        assert!(!t.should_draw_splash());
        assert!(!t.should_draw_splash_at(1), "and it never starts drawing again");
    }

    #[test]
    fn a_stale_generation_cannot_resurrect_the_splash() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        let old = t.generation();
        let fresh = t.restart(100);
        assert_eq!(fresh, old + 1, "a restart is a new generation");
        assert!(t.is_stale(old));
        assert!(!t.is_stale(fresh));
        assert_eq!(t.shell_ready_from(old, 150), Reveal::Wait, "the late callback is ignored");
        assert!(!t.is_shell_ready());
        assert!(t.should_draw_splash(), "nothing was revealed by the stale callback");
        assert_eq!(t.reveal_from(old), Reveal::Wait);
        assert!(t.should_draw_splash(), "a stale reveal cannot reveal");
        assert_eq!(t.art_done_from(old, 200), Reveal::Wait);
        assert!(!t.is_art_done(), "a stale art_done cannot complete the artwork");
        assert_eq!(t.fail_from(old, 200), Reveal::Wait);
        assert!(!t.is_failed(), "a stale failure cannot skip the current run");
        assert_eq!(t.tick_from(old, 99_999), Reveal::Wait, "not even a passed deadline");
        assert_eq!(t.tick_from(fresh, 4_100), Reveal::Show, "the current generation still works");
    }

    #[test]
    fn a_stale_callback_after_the_reveal_reports_already_revealed() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        let old = t.generation();
        t.restart(0);
        assert_eq!(t.shell_ready(5), Reveal::Show);
        assert_eq!(t.shell_ready_from(old, 6), Reveal::AlreadyRevealed);
        assert_eq!(t.tick_from(old, 6), Reveal::AlreadyRevealed);
        assert_eq!(t.reveal_from(old), Reveal::AlreadyRevealed);
    }

    #[test]
    fn reduced_motion_completes_the_art_but_still_waits_for_the_shell() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        t.set_reduced_motion(true);
        assert!(t.reduced_motion());
        assert!(t.is_art_done(), "a static final frame is already the whole animation");
        assert_eq!(t.tick(1), Reveal::Wait, "accessibility does not bypass readiness");
        assert_eq!(t.tick(4_000), Reveal::Show);
        let mut off = Timeline::new(0, DEADLINE_MS);
        off.set_reduced_motion(false);
        assert!(!off.is_art_done(), "without reduced motion the art is not done yet");
    }

    #[test]
    fn restart_clears_the_run_but_keeps_the_system_settings() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        t.set_reduced_motion(true);
        assert_eq!(t.shell_ready(10), Reveal::Show);
        let old = t.generation();
        let fresh = t.restart(50);
        assert_eq!(fresh, old + 1);
        assert!(t.is_stale(old));
        assert!(!t.is_shell_ready());
        assert!(!t.is_failed());
        assert!(!t.is_revealed());
        assert!(t.reduced_motion(), "the OS setting outlives a replay");
        assert!(t.is_art_done(), "still reduced motion, so still instantly complete");
        assert!(t.should_draw_splash(), "the new run draws again");
        assert_eq!(t.deadline_at(), 50 + DEADLINE_MS, "the deadline is re-armed from the restart");
    }

    #[test]
    fn the_splash_is_drawn_exactly_until_the_reveal_fires() {
        let mut t = Timeline::new(0, DEADLINE_MS);
        assert!(t.should_draw_splash());
        assert!(t.should_draw_splash_at(0));
        assert!(t.should_draw_splash_at(3_999));
        assert!(!t.should_draw_splash_at(4_000), "the predicate alone stops drawing");
        assert_eq!(t.tick(3_000), Reveal::Wait);
        assert!(t.should_draw_splash());
        assert_eq!(t.tick(4_000), Reveal::Show);
        assert!(!t.should_draw_splash());
        assert!(!t.should_draw_splash_at(1), "a revealed splash is never drawn again");
    }
}
