//! Client <-> broker wire protocol: length-prefixed JSON frames. Requests carry no
//! origin field on purpose: the broker derives origin from the authenticated channel,
//! so a client cannot claim to be a human or an agent.
use crate::caps::{AdminAction, GrantDuration};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const MAX_REQUEST_BYTES: usize = 1 << 20;
pub const MAX_RESPONSE_BYTES: usize = 16 << 20;
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Gui,
    Cli,
    /// An external agent: can list sessions and stage drafts; everything else needs a grant.
    Agent,
}
impl ClientKind {
    pub fn tag(self) -> u8 {
        match self {
            Self::Gui => 1,
            Self::Cli => 2,
            Self::Agent => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Duration {
    OneMinute,
    FifteenMinutes,
    OneHour,
    OneDay,
}
impl From<Duration> for GrantDuration {
    fn from(d: Duration) -> Self {
        match d {
            Duration::OneMinute => GrantDuration::OneMinute,
            Duration::FifteenMinutes => GrantDuration::FifteenMinutes,
            Duration::OneHour => GrantDuration::OneHour,
            Duration::OneDay => GrantDuration::OneDay,
        }
    }
}

/// A fixed menu, never a client-supplied command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    Cmd,
    PowerShell,
    Pwsh,
}
impl Shell {
    /// The name used in the registry file and by `wb new --shell`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cmd => "cmd",
            Self::PowerShell => "powershell",
            Self::Pwsh => "pwsh",
        }
    }
    pub fn from_name(name: &str) -> Option<Shell> {
        match name {
            "cmd" => Some(Self::Cmd),
            "powershell" => Some(Self::PowerShell),
            "pwsh" => Some(Self::Pwsh),
            _ => None,
        }
    }
    pub fn command_line(self) -> &'static str {
        match self {
            Self::Cmd => "cmd.exe",
            Self::PowerShell => "powershell.exe -NoLogo",
            Self::Pwsh => "pwsh.exe -NoLogo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admin {
    EndAllSessions,
    StopDaemon,
}
impl From<Admin> for AdminAction {
    fn from(a: Admin) -> Self {
        match a {
            Admin::EndAllSessions => AdminAction::EndAllSessions,
            Admin::StopDaemon => AdminAction::StopDaemon,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Reply to the server's challenge. `proof` is lowercase hex of the HMAC.
    Hello { version: u32, client: ClientKind, proof: String, #[serde(default, skip_serializing_if = "Option::is_none")] agent: Option<String> },
    List {},
    NewTerminal { cols: u16, rows: u16, label: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] shell: Option<Shell>, #[serde(default, skip_serializing_if = "Option::is_none")] extended_history: Option<bool>, #[serde(default, skip_serializing_if = "Option::is_none")] cwd: Option<String> },
    /// Output bytes from `from` (an absolute offset into the session's output stream).
    Read { session: String, from: u64 },
    /// Human connections only. Output for many sessions at once: waits up to `wait_ms` (at most `MAX_WATCH_WAIT_MS`)
    /// until any session has output beyond its cursor, then answers with that output. The connection is busy while it
    /// waits, so a watcher uses a connection of its own.
    Watch { cursors: Vec<WatchCursor>, wait_ms: u64 },
    /// Human keystrokes (UTF-8 text). Origin is implied: always the human channel.
    Input { session: String, text: String },
    Resize { session: String, cols: u16, rows: u16 },
    End { session: String },
    /// Human connections only: recreate a terminal that did not survive a restart (a `lost` one) under its old id, with its old
    /// label, size, shell, start folder and history size. The old entry is replaced; nothing that was running comes back.
    /// `shell` overrides the recorded one (a terminal adopted before shells were recorded has none: the app knows its agent terminals are PowerShell).
    Restore { session: String, #[serde(default, skip_serializing_if = "Option::is_none")] shell: Option<Shell> },
    AcceptDraft { session: String },
    GrantCreate { agent: String, session: String, duration: Duration, keep_after_detach: bool, gate: String },
    GrantRevoke { agent: String, session: String },
    AdminBegin { action: Admin },
    AdminConfirm { typed: String },
    AdminCancel {},
    /// Agent connections only: stage text for a human to accept. Never typed by itself.
    Propose { session: String, text: String },
    /// Agent connections only: type into a session; allowed only with a live grant.
    AgentInput { session: String, text: String },
    GetDraft { session: String },
    DiscardDraft { session: String },
    /// Human connections only: start recording this terminal's output to a log file. The file is chosen by the
    /// service (never by the client) and the recording begins with the next byte the terminal prints; starting
    /// again while it is already recording is a no-op that answers with the same file.
    LogStart { session: String },
    /// Human connections only: stop recording. Stopping while it is not recording is fine (it answers the state).
    LogStop { session: String },
    /// Human connections only: where the log is and how far it got (also after the terminal ended).
    LogStatus { session: String },
    Grants { session: String },
    /// Change the focus-gate phrase; the current one must match.
    SetGate { current: String, new: String },
}

/// A folder a terminal may start in: an absolute drive path (`C:\...`), at most 240 characters, no control
/// characters and none of the characters Windows forbids in paths. Whether it exists is checked at spawn time.
pub fn valid_start_dir(dir: &str) -> bool {
    let b = dir.as_bytes();
    dir.chars().count() <= 240
        && b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && !dir.chars().any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '|' | '*' | '?'))
        && !dir.split('\\').any(|part| part == ".." || part == ".")
}

/// Agent names: short, printable, no spaces.
pub fn valid_agent_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// How many cursors one `Watch` may carry, and the longest it may wait.
pub const MAX_WATCH_CURSORS: usize = 64;
pub const MAX_WATCH_WAIT_MS: u64 = 2_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchCursor {
    pub session: String,
    pub from: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchItem {
    pub session: String,
    /// The cursor this output starts at (as sent in the request).
    pub from: u64,
    /// Base64 output bytes.
    pub data: String,
    pub next: u64,
    pub skipped: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DraftView {
    pub agent: String,
    pub text: String,
    pub multiline: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantView {
    pub agent: String,
    pub remaining_ms: u64,
    pub keep_after_detach: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
    pub state: String,
    pub persistent: bool,
    pub cols: u16,
    pub rows: u16,
    pub host_pid: u32,
    /// Size of the session's in-memory output history in MiB (1 standard, 32 extended).
    #[serde(default = "default_history_mib")]
    pub history_mib: u32,
}

fn default_history_mib() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    Challenge { version: u32, nonce: String },
    Ok,
    Error { code: String, message: String },
    Sessions { items: Vec<SessionInfo> },
    Created { session: SessionInfo },
    /// `data` is base64; `next` is the offset to pass to the next Read; `skipped`
    /// counts bytes lost to ring overwrite before `from`.
    Output { data: String, next: u64, skipped: u64 },
    /// Answer to `Watch`: sessions that have output (each with the cursor it started from), and cursors whose
    /// session no longer exists.
    Watch { items: Vec<WatchItem>, gone: Vec<String> },
    /// Answer to `LogStart`, `LogStop` and `LogStatus`: `on` while output is being recorded, `path` when a log exists
    /// (a stopped or ended log keeps its file), `bytes` written so far, and why it stopped when it stopped by itself
    /// (the cap, a write error) or because the terminal ended.
    Logging { on: bool, path: Option<String>, bytes: u64, stopped_reason: Option<String> },
    AdminPending { action: Admin, affected: usize, wait_ms: u64 },
    Draft { draft: Option<DraftView> },
    Grants { items: Vec<GrantView> },
}

pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` on clean end of stream before a frame starts.
pub fn read_frame<R: Read>(r: &mut R, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn send<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    write_frame(w, &serde_json::to_vec(v).map_err(io::Error::other)?)
}

pub fn recv_request<R: Read>(r: &mut R) -> io::Result<Option<Request>> {
    let Some(bytes) = read_frame(r, MAX_REQUEST_BYTES)? else { return Ok(None) };
    serde_json::from_slice(&bytes).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad request"))
}

pub fn recv_response<R: Read>(r: &mut R) -> io::Result<Option<Response>> {
    let Some(bytes) = read_frame(r, MAX_RESPONSE_BYTES)? else { return Ok(None) };
    serde_json::from_slice(&bytes).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad response"))
}

// ---- base64 (standard alphabet, padded) ----------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| B64.iter().position(|x| *x == c).and_then(|p| u32::try_from(p).ok());
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (i, q) in b.chunks(4).enumerate() {
        let last = i == b.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (j, c) in q.iter().enumerate() {
            n = n << 6 | if j >= 4 - pad { 0 } else { val(*c)? };
        }
        // The 24 bits just assembled, as three bytes.
        let [_, first, second, third] = n.to_be_bytes();
        out.push(first);
        if pad < 2 {
            out.push(second);
        }
        if pad < 1 {
            out.push(third);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn base64_round_trips_every_length_and_rejects_garbage() {
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data, "len {n}");
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
        assert!(b64_decode("TWE").is_none() && b64_decode("T===").is_none() && b64_decode("TW!=").is_none());
        assert!(b64_decode("TWE=TWFu").is_none(), "padding only at the end");
    }

    #[test]
    fn frames_round_trip_and_enforce_limits() {
        let mut buf = Vec::new();
        send(&mut buf, &Request::List {}).unwrap();
        send(&mut buf, &Request::Input { session: "s".into(), text: "ls\r".into() }).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(recv_request(&mut c).unwrap(), Some(Request::List {}));
        assert!(matches!(recv_request(&mut c).unwrap(), Some(Request::Input { .. })));
        assert_eq!(recv_request(&mut c).unwrap(), None);
        let mut huge = (MAX_REQUEST_BYTES as u32 + 1).to_le_bytes().to_vec();
        huge.extend_from_slice(b"x");
        assert!(recv_request(&mut Cursor::new(huge)).is_err());
        let mut short = 10u32.to_le_bytes().to_vec();
        short.extend_from_slice(b"abc");
        assert!(recv_request(&mut Cursor::new(short)).is_err(), "truncated frame body");
    }

    #[test]
    fn requests_cannot_carry_an_origin_or_unknown_fields() {
        for bad in [
            r#"{"op":"input","session":"s","text":"x","origin":"human"}"#,
            r#"{"op":"input","session":"s","text":"x","origin":{"agent":"a"}}"#,
            r#"{"op":"list","admin":true}"#,
            r#"{"op":"nope"}"#,
            r#"{"session":"s"}"#,
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, bad.as_bytes()).unwrap();
            assert!(recv_request(&mut Cursor::new(buf)).is_err(), "{bad}");
        }
    }

    #[test]
    fn watch_requests_and_answers_round_trip() {
        let req = Request::Watch { cursors: vec![WatchCursor { session: "0a1b2c3d".into(), from: 7 }], wait_ms: 500 };
        let mut buf = Vec::new();
        send(&mut buf, &req).unwrap();
        assert_eq!(recv_request(&mut Cursor::new(buf)).unwrap(), Some(req));
        let bad = r#"{"op":"watch","cursors":[{"session":"s","from":1,"extra":1}],"wait_ms":1}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, bad.as_bytes()).unwrap();
        // Unknown fields inside a cursor are tolerated by serde's default, but the request itself stays strict.
        let loose = r#"{"op":"watch","cursors":[],"wait_ms":1,"origin":"human"}"#;
        let mut buf2 = Vec::new();
        write_frame(&mut buf2, loose.as_bytes()).unwrap();
        assert!(recv_request(&mut Cursor::new(buf2)).is_err());
        let answer = Response::Watch { items: vec![WatchItem { session: "s".into(), from: 1, data: "TWFu".into(), next: 4, skipped: 0 }], gone: vec!["x".into()] };
        let json = serde_json::to_string(&answer).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), answer);
    }

    #[test]
    fn log_messages_round_trip_and_leave_the_rest_of_the_protocol_alone() {
        for req in [Request::LogStart { session: "s".into() }, Request::LogStop { session: "s".into() }, Request::LogStatus { session: "s".into() }] {
            let mut buf = Vec::new();
            send(&mut buf, &req).unwrap();
            assert_eq!(recv_request(&mut Cursor::new(buf)).unwrap(), Some(req));
        }
        for answer in [
            Response::Logging { on: true, path: Some(r"C:\data\logs\ab-20261025T101112Z.log".into()), bytes: 12, stopped_reason: None },
            Response::Logging { on: false, path: Some(r"C:\data\logs\ab-20261025T101112Z.log".into()), bytes: 1024, stopped_reason: Some("the 64 MiB limit was reached".into()) },
            Response::Logging { on: false, path: None, bytes: 0, stopped_reason: None },
        ] {
            let json = serde_json::to_string(&answer).unwrap();
            assert!(json.starts_with(r#"{"op":"logging","#), "{json}");
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), answer);
        }
        // The messages that were there before still decode exactly as they did.
        let sessions = r#"{"op":"sessions","items":[{"id":"ab","label":"t","state":"running","persistent":true,"cols":80,"rows":24,"host_pid":1}]}"#;
        assert!(serde_json::from_str::<Response>(sessions).is_ok());
        assert!(serde_json::from_str::<Request>(r#"{"op":"log_start","session":"ab"}"#).is_ok());
        assert!(serde_json::from_str::<Request>(r#"{"op":"log_start","session":"ab","path":"C:\\x.log"}"#).is_err(), "a client cannot choose the file");
    }

    #[test]
    fn durations_and_admin_actions_map_exactly() {
        assert_eq!(GrantDuration::from(Duration::OneDay).ms(), 86_400_000);
        assert_eq!(AdminAction::from(Admin::EndAllSessions), AdminAction::EndAllSessions);
        let r: Request = serde_json::from_str(r#"{"op":"grant_create","agent":"a","session":"s","duration":"fifteen_minutes","keep_after_detach":false,"gate":""}"#).unwrap();
        assert!(matches!(r, Request::GrantCreate { duration: Duration::FifteenMinutes, .. }));
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;

    #[test]
    fn optional_new_terminal_fields_are_omitted_when_unset_so_older_services_still_accept_the_request() {
        let plain = serde_json::to_string(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: None, cwd: None }).unwrap();
        assert!(!plain.contains("shell") && !plain.contains("extended_history") && !plain.contains("cwd"), "{plain}");
        let extended = serde_json::to_string(&Request::NewTerminal { cols: 100, rows: 30, label: None, shell: None, extended_history: Some(true), cwd: None }).unwrap();
        assert!(extended.contains("\"extended_history\":true"));
        // Both decode on the current service, and a request without the fields still decodes.
        for text in [plain.as_str(), extended.as_str(), r#"{"op":"new_terminal","cols":100,"rows":30,"label":null}"#] {
            assert!(serde_json::from_str::<Request>(text).is_ok(), "{text}");
        }
    }
}
