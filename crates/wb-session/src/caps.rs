//! Capability tiers, input origin, agent-input grants, the focus gate, "propose"
//! staging and the multi-step admin flow. Pure logic with an injected clock, so every
//! rule is unit-testable. See docs/design/session-daemon.md sections 5, 9, 11, 13.
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// View output and state.
    Observe,
    /// Send keystrokes to a terminal or messages to an agent.
    Input,
    /// Create sessions.
    Spawn,
    /// Stage a draft for a human to accept (agent connections only).
    Propose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapSet(u8);
impl CapSet {
    pub const NONE: CapSet = CapSet(0);
    pub const USER: CapSet = CapSet(0b0111); // observe + input + spawn
    /// Agent connections: list sessions and stage drafts. Reading output and typing need a live grant.
    pub const AGENT: CapSet = CapSet(0b1_0001); // observe + propose
    pub fn has(self, c: Capability) -> bool {
        self.0 & Self::bit(c) != 0
    }
    pub fn with(self, c: Capability) -> CapSet {
        CapSet(self.0 | Self::bit(c))
    }
    fn bit(c: Capability) -> u8 {
        match c {
            Capability::Observe => 1,
            Capability::Input => 2,
            Capability::Spawn => 4,
            Capability::Propose => 16,
        }
    }
}

/// Where input came from. The broker derives this from the authenticated channel,
/// never from a field a client sends.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Human,
    Agent(String),
}

// ---- Grants --------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantDuration {
    OneMinute,
    FifteenMinutes,
    OneHour,
    OneDay,
}
impl GrantDuration {
    pub fn ms(self) -> u64 {
        match self {
            Self::OneMinute => 60_000,
            Self::FifteenMinutes => 15 * 60_000,
            Self::OneHour => 60 * 60_000,
            Self::OneDay => 24 * 60 * 60_000,
        }
    }
}

#[derive(Clone, Debug)]
struct Grant {
    expires_ms: u64,
    keep_after_detach: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantError {
    GateFailed,
    AgentIdInvalid,
}

#[derive(Default)]
pub struct GrantTable {
    grants: HashMap<(String, String), Grant>,
}
impl GrantTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// `gate_ok` must come from [`Gate::verify`] run natively on a focused dialog.
    pub fn create(
        &mut self,
        agent: &str,
        session: &str,
        duration: GrantDuration,
        keep_after_detach: bool,
        gate_ok: bool,
        now_ms: u64,
    ) -> Result<(), GrantError> {
        if !gate_ok {
            return Err(GrantError::GateFailed);
        }
        if agent.is_empty() || agent.len() > 128 || session.is_empty() || session.len() > 128 {
            return Err(GrantError::AgentIdInvalid);
        }
        self.grants.insert(
            (agent.to_owned(), session.to_owned()),
            Grant { expires_ms: now_ms.saturating_add(duration.ms()), keep_after_detach },
        );
        Ok(())
    }

    /// Per agent and per session; lapses at expiry, and (unless "keep") while no
    /// human client is attached.
    pub fn permits(&self, agent: &str, session: &str, now_ms: u64, human_attached: bool) -> bool {
        match self.grants.get(&(agent.to_owned(), session.to_owned())) {
            Some(g) => now_ms < g.expires_ms && (human_attached || g.keep_after_detach),
            None => false,
        }
    }
    pub fn remaining_ms(&self, agent: &str, session: &str, now_ms: u64) -> Option<u64> {
        self.grants.get(&(agent.to_owned(), session.to_owned())).and_then(|g| g.expires_ms.checked_sub(now_ms)).filter(|r| *r > 0)
    }
    /// Instant and gate-free by design.
    pub fn revoke(&mut self, agent: &str, session: &str) -> bool {
        self.grants.remove(&(agent.to_owned(), session.to_owned())).is_some()
    }
    pub fn revoke_session(&mut self, session: &str) {
        self.grants.retain(|(_, s), _| s != session);
    }
    /// (agent, remaining ms, keep-after-detach) for live grants on `session`.
    pub fn list_session(&self, session: &str, now_ms: u64) -> Vec<(String, u64, bool)> {
        let mut v: Vec<_> = self
            .grants
            .iter()
            .filter(|((_, s), g)| s == session && now_ms < g.expires_ms)
            .map(|((a, _), g)| (a.clone(), g.expires_ms - now_ms, g.keep_after_detach))
            .collect();
        v.sort();
        v
    }

    pub fn revoke_all(&mut self) {
        self.grants.clear();
    }
    pub fn expire(&mut self, now_ms: u64) {
        self.grants.retain(|_, g| now_ms < g.expires_ms);
    }
    pub fn len(&self) -> usize {
        self.grants.len()
    }
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

/// Decide whether `origin` may type into `session`: humans always (given the Input
/// capability); agents only with a live grant.
pub fn input_allowed(caps: CapSet, origin: &Origin, grants: &GrantTable, session: &str, now_ms: u64, human_attached: bool) -> bool {
    if !caps.has(Capability::Input) {
        return false;
    }
    match origin {
        Origin::Human => true,
        Origin::Agent(id) => grants.permits(id, session, now_ms, human_attached),
    }
}

// ---- Focus gate ----------------------------------------------------------

pub const MAX_GATE_CHARS: usize = 256;
const GATE_SYMBOLS: &str = "!@#$%^&*()";

/// Letters, digits and the ten shifted digit-row symbols, by Unicode scalar value.
pub fn gate_char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || GATE_SYMBOLS.contains(c)
}
pub fn gate_text_ok(s: &str) -> bool {
    s.chars().count() <= MAX_GATE_CHARS && s.chars().all(gate_char_ok)
}

/// Anti-misfire phrase, not a security feature. Stored salted-hashed only to avoid
/// keeping a readable phrase around. The empty phrase means "just press Enter".
pub struct Gate {
    salt: [u8; 16],
    hash: [u8; 32],
}
impl Gate {
    pub fn new(phrase: &str, salt: [u8; 16]) -> Option<Gate> {
        gate_text_ok(phrase).then(|| Gate { salt, hash: Self::digest(&salt, phrase) })
    }
    fn digest(salt: &[u8; 16], phrase: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(phrase.as_bytes());
        let mut out: [u8; 32] = h.finalize().into();
        for _ in 0..1000 {
            let mut h = Sha256::new();
            h.update(out);
            h.update(salt);
            out = h.finalize().into();
        }
        out
    }
    /// Stored form: salt and hash, for persisting the phrase across restarts.
    pub fn parts(&self) -> ([u8; 16], [u8; 32]) {
        (self.salt, self.hash)
    }
    pub fn from_parts(salt: [u8; 16], hash: [u8; 32]) -> Gate {
        Gate { salt, hash }
    }
    /// Input outside the allowed set is rejected outright (the dialog filters it too).
    pub fn verify(&self, candidate: &str) -> bool {
        gate_text_ok(candidate) && ct_eq(&Self::digest(&self.salt, candidate), &self.hash)
    }
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Propose staging -----------------------------------------------------

pub const MAX_DRAFT_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub agent: String,
    pub text: String,
    pub multiline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftError {
    Empty,
    TooLong,
    /// Control characters other than newline and tab are never staged.
    ControlCharacter,
}

/// Agent text staged in a separate box for a human to accept. Escapes and other
/// control characters are rejected; newlines make the draft multi-line (sent only
/// bracketed-paste wrapped).
pub fn stage_draft(agent: &str, text: &str) -> Result<Draft, DraftError> {
    if text.is_empty() {
        return Err(DraftError::Empty);
    }
    if text.len() > MAX_DRAFT_BYTES {
        return Err(DraftError::TooLong);
    }
    if text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') || text.contains('\r') {
        return Err(DraftError::ControlCharacter);
    }
    Ok(Draft { agent: agent.to_owned(), text: text.to_owned(), multiline: text.contains('\n') })
}

/// The exact bytes sent to the terminal when a human accepts a draft: single-line
/// drafts as typed text without Enter (the human presses it), multi-line drafts
/// bracketed-paste wrapped so a shell shows them instead of running them.
pub fn accept_bytes(d: &Draft) -> Vec<u8> {
    if d.multiline {
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(d.text.replace('\n', "\r").as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        d.text.as_bytes().to_vec()
    }
}

// ---- Admin flow ----------------------------------------------------------
// The admin requests (end all, stop daemon) need `Capability::Spawn`, which only human connections have; there is no
// separate admin capability (an unused one used to exist and gave a false sense of a second gate).

pub const ADMIN_MIN_DELAY_MS: u64 = 3_000;
pub const ADMIN_WINDOW_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminAction {
    EndAllSessions,
    StopDaemon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminError {
    NoPendingRequest,
    TooEarly,
    Expired,
    ConfirmationMismatch,
}

/// Begin, wait out a short countdown, then confirm by typing the number of sessions
/// affected. Window close can never produce any of these requests.
///
/// The sessions to be ended are recorded at `begin` and returned by `confirm`: what the human confirms is exactly
/// the set that was counted for them, so a session created during the countdown is never ended by a confirmation
/// that did not include it (review finding F6).
#[derive(Default)]
pub struct AdminFlow {
    pending: Option<Pending>,
}
struct Pending {
    action: AdminAction,
    began_ms: u64,
    /// The session ids counted at `begin` (empty for actions that end no session).
    targets: Vec<String>,
}
impl AdminFlow {
    pub fn new() -> Self {
        Self::default()
    }
    /// `targets` are the sessions the action will end; their count is what the user has to type back.
    pub fn begin(&mut self, action: AdminAction, targets: Vec<String>, now_ms: u64) {
        self.pending = Some(Pending { action, began_ms: now_ms, targets });
    }
    pub fn cancel(&mut self) {
        self.pending = None;
    }
    /// On success: the action and the sessions that were counted when it began.
    pub fn confirm(&mut self, typed: &str, now_ms: u64) -> Result<(AdminAction, Vec<String>), AdminError> {
        let Some(pending) = self.pending.as_ref() else { return Err(AdminError::NoPendingRequest) };
        let age = now_ms.saturating_sub(pending.began_ms);
        if age < ADMIN_MIN_DELAY_MS {
            return Err(AdminError::TooEarly);
        }
        if age > ADMIN_WINDOW_MS {
            self.pending = None;
            return Err(AdminError::Expired);
        }
        if typed.trim() != pending.targets.len().to_string() {
            return Err(AdminError::ConfirmationMismatch);
        }
        let done = self.pending.take();
        Ok(done.map_or((AdminAction::StopDaemon, Vec::new()), |p| (p.action, p.targets)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_caps_exclude_admin_and_humans_type_without_a_grant() {
        assert!(CapSet::USER.has(Capability::Input) && !CapSet::AGENT.has(Capability::Spawn));
        let g = GrantTable::new();
        assert!(input_allowed(CapSet::USER, &Origin::Human, &g, "s", 0, false));
        assert!(!input_allowed(CapSet::NONE.with(Capability::Observe), &Origin::Human, &g, "s", 0, true));
    }

    #[test]
    fn agents_need_a_live_grant_for_that_agent_and_session() {
        let mut g = GrantTable::new();
        let agent = Origin::Agent("a1".into());
        assert!(!input_allowed(CapSet::USER, &agent, &g, "s1", 0, true));
        g.create("a1", "s1", GrantDuration::OneMinute, false, true, 1_000).unwrap();
        assert!(input_allowed(CapSet::USER, &agent, &g, "s1", 1_000, true));
        assert!(!input_allowed(CapSet::USER, &agent, &g, "s2", 1_000, true), "per session");
        assert!(!input_allowed(CapSet::USER, &Origin::Agent("a2".into()), &g, "s1", 1_000, true), "per agent");
        assert!(input_allowed(CapSet::USER, &agent, &g, "s1", 60_999, true));
        assert!(!input_allowed(CapSet::USER, &agent, &g, "s1", 61_000, true), "expires exactly at the end");
        assert_eq!(g.remaining_ms("a1", "s1", 31_000), Some(30_000));
    }

    #[test]
    fn grant_requires_the_gate_and_lapses_without_a_human_unless_kept() {
        let mut g = GrantTable::new();
        assert_eq!(g.create("a", "s", GrantDuration::OneHour, false, false, 0), Err(GrantError::GateFailed));
        assert!(g.is_empty());
        g.create("a", "s", GrantDuration::OneHour, false, true, 0).unwrap();
        g.create("b", "s", GrantDuration::OneDay, true, true, 0).unwrap();
        assert!(g.permits("a", "s", 10, true));
        assert!(!g.permits("a", "s", 10, false), "unattended agent cannot type");
        assert!(g.permits("b", "s", 10, false), "explicit keep-after-detach");
        assert!(g.revoke("a", "s") && !g.permits("a", "s", 10, true), "revoke is instant");
        g.expire(GrantDuration::OneDay.ms());
        assert!(g.is_empty());
        assert_eq!(GrantDuration::FifteenMinutes.ms(), 900_000);
    }

    #[test]
    fn gate_charset_empty_phrase_and_hashing() {
        assert!(gate_text_ok("") && gate_text_ok("aZ09!@#$%^&*()"));
        for bad in ["a b", "a-b", "é", "a_b", "[x]", "a\n"] {
            assert!(!gate_text_ok(bad), "{bad:?}");
        }
        let salt = [7u8; 16];
        let empty = Gate::new("", salt).unwrap();
        assert!(empty.verify("") && !empty.verify("x"));
        let g = Gate::new("Ab1!", salt).unwrap();
        assert!(g.verify("Ab1!") && !g.verify("ab1!") && !g.verify("") && !g.verify("Ab1! "));
        assert!(Gate::new("no spaces", salt).is_none());
        assert!(!Gate::new("x".repeat(MAX_GATE_CHARS).as_str(), salt).is_none());
        assert!(Gate::new(&"x".repeat(MAX_GATE_CHARS + 1), salt).is_none());
    }

    #[test]
    fn drafts_reject_controls_and_wrap_multiline_in_bracketed_paste() {
        assert_eq!(stage_draft("a", ""), Err(DraftError::Empty));
        assert_eq!(stage_draft("a", "ls\x03"), Err(DraftError::ControlCharacter));
        assert_eq!(stage_draft("a", "rm\r"), Err(DraftError::ControlCharacter));
        assert_eq!(stage_draft("a", "\x1b[2J"), Err(DraftError::ControlCharacter));
        assert_eq!(stage_draft("a", &"x".repeat(MAX_DRAFT_BYTES + 1)), Err(DraftError::TooLong));
        let one = stage_draft("a", "echo\thi").unwrap();
        assert!(!one.multiline);
        assert_eq!(accept_bytes(&one), b"echo\thi", "no Enter is ever appended");
        let two = stage_draft("a", "echo a\necho b").unwrap();
        let bytes = accept_bytes(&two);
        assert!(bytes.starts_with(b"\x1b[200~") && bytes.ends_with(b"\x1b[201~"));
    }

    #[test]
    fn admin_flow_needs_delay_window_and_the_typed_count() {
        let mut f = AdminFlow::new();
        assert_eq!(f.confirm("3", 0), Err(AdminError::NoPendingRequest));
        let ids = || vec!["a".to_string(), "b".to_string(), "c".to_string()];
        f.begin(AdminAction::EndAllSessions, ids(), 10_000);
        assert_eq!(f.confirm("3", 10_500), Err(AdminError::TooEarly), "countdown");
        assert_eq!(f.confirm("4", 14_000), Err(AdminError::ConfirmationMismatch));
        assert_eq!(f.confirm(" 3 ", 14_000), Ok((AdminAction::EndAllSessions, ids())), "the confirmed set is the one counted at begin");
        assert_eq!(f.confirm("3", 14_001), Err(AdminError::NoPendingRequest), "single use");
        f.begin(AdminAction::StopDaemon, Vec::new(), 0);
        assert_eq!(f.confirm("0", ADMIN_WINDOW_MS + 1), Err(AdminError::Expired));
        f.begin(AdminAction::StopDaemon, Vec::new(), 0);
        f.cancel();
        assert_eq!(f.confirm("0", 5_000), Err(AdminError::NoPendingRequest));
    }
}
