//! Opt-in per-session output logging (the tmux `pipe-pane` idea): a terminal's raw output bytes, appended to a file
//! the broker chose, from the moment logging starts. Design: `docs/design/session-daemon.md` ("Scrollback and
//! secrets": persisting output is opt-in per session, with a visible indicator).
//!
//! The file is written by the session host, the only place that sees every output byte exactly once (the broker
//! reads output only when a client asks, and the ring overwrites the oldest bytes). This module is the writer and
//! the naming rules; the Win32 file creation (user-only DACL) is in `win`, the op that turns it on in `host`.
//!
//! Rules this file keeps: no `unwrap`/`expect` on a runtime path, the file never grows past the cap, and a failed
//! write stops logging instead of touching the session (the terminal keeps running; only the log ends).

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

/// Folder under the data directory that holds the logs. Created user-only, like the data directory itself.
pub const LOG_DIR: &str = "logs";
/// Hard cap on one logging run: at 64 MiB the writer stops itself (a session that never stops printing cannot fill
/// the disk).
pub const LOG_CAP_BYTES: u64 = 64 << 20;

/// Why logging stopped. `Stopped` is the normal case (the user asked); the other two are decided by the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Asked to stop.
    Stopped,
    /// The cap was reached.
    Cap,
    /// A write failed (disk full, file closed, permissions).
    WriteError,
    /// The terminal ended (its shell exited or its output closed), so nothing more can be recorded.
    Ended,
}
impl StopReason {
    /// The short code a host reports to the broker (never shown to a person as it is: the broker words it).
    pub fn code(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Cap => "cap",
            Self::WriteError => "write_error",
            Self::Ended => "ended",
        }
    }
    pub fn from_code(code: &str) -> Option<StopReason> {
        match code {
            "stopped" => Some(Self::Stopped),
            "cap" => Some(Self::Cap),
            "write_error" => Some(Self::WriteError),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// `unix_ms` as `yyyymmddThhmmssZ` (UTC), the timestamp part of a log file's name. Without a date crate.
pub fn stamp_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant), as the audit timestamps use.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let (y, doy) = (yoe + era * 400, doe - (365 * yoe + yoe / 4 - yoe / 100));
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `<session id>-<stamp>.log`. The id is sanitised to `[A-Za-z0-9_-]`, so a crafted session id can never name a file
/// outside the logs folder or a device.
pub fn file_name(id: &str, unix_ms: u64) -> String {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(64).collect();
    format!("{safe}-{}.log", stamp_utc(unix_ms))
}

/// The folder a data directory keeps its logs in.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR)
}

/// The name a log started at `unix_ms` gets. A name already on disk is never reused (each start begins a fresh log),
/// so two starts in the same second become `...-2.log`, `...-3.log`.
pub fn path(data_dir: &Path, id: &str, unix_ms: u64) -> PathBuf {
    let base = file_name(id, unix_ms);
    let stem = base.strip_suffix(".log").unwrap_or(&base);
    let mut candidate = dir(data_dir).join(&base);
    let mut n = 2u32;
    while candidate.exists() && n < 100 {
        candidate = dir(data_dir).join(format!("{stem}-{n}.log"));
        n = n.saturating_add(1);
    }
    candidate
}

/// Appends output bytes to one log file, and stops itself on the cap or on a write error. Every update is a single
/// step, so a panic elsewhere cannot leave it half-written in a way that matters: the file has what was written and
/// `bytes` says how much.
pub struct LogWriter {
    file: File,
    cap: u64,
    bytes: u64,
    stop: Option<StopReason>,
}

impl LogWriter {
    /// Wrap a file that was just created for this run.
    pub fn new(file: File, cap: u64) -> LogWriter {
        LogWriter { file, cap, bytes: 0, stop: None }
    }

    /// True until it is stopped (by request, the cap, or a write error).
    pub fn on(&self) -> bool {
        self.stop.is_none()
    }

    /// Bytes written by this run.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop
    }

    /// Stop on request. Stopping when already stopped keeps the first reason (the cap is what ended it, not the
    /// later request).
    pub fn stop(&mut self) {
        if self.stop.is_none() {
            self.stop = Some(StopReason::Stopped);
        }
    }

    /// The terminal ended: nothing more will come. Like `stop`, an earlier reason is kept.
    pub fn end(&mut self) {
        if self.stop.is_none() {
            self.stop = Some(StopReason::Ended);
        }
    }

    /// Append output. Whatever happens here, the terminal is unaffected: an error ends the log and nothing else.
    /// Each chunk goes straight to the OS (`File` is unbuffered), so a log read while the terminal runs is complete up
    /// to the host's last read; it is not forced to disk (no `FlushFileBuffers`), which a power cut could cost.
    pub fn append(&mut self, bytes: &[u8]) {
        if self.stop.is_some() || bytes.is_empty() {
            return;
        }
        let room = self.cap.saturating_sub(self.bytes);
        if room == 0 {
            self.stop = Some(StopReason::Cap);
            return;
        }
        let wanted = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let take = usize::try_from(wanted.min(room)).unwrap_or(bytes.len()).min(bytes.len());
        if take == 0 {
            self.stop = Some(StopReason::Cap);
            return;
        }
        if self.file.write_all(bytes.get(..take).unwrap_or(bytes)).and_then(|()| self.file.flush()).is_err() {
            self.stop = Some(StopReason::WriteError);
            return;
        }
        self.bytes = self.bytes.saturating_add(take as u64);
        // Short of the whole chunk (the cap cut it) or exactly at the cap: this log is finished.
        if take < bytes.len() || self.bytes >= self.cap {
            self.stop = Some(StopReason::Cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::OpenOptions, io::Read};

    /// A fresh temp file for one test (the tests' own folder: nothing here touches a real install).
    fn temp_file(tag: &str) -> (PathBuf, File) {
        let id = format!("wblog-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let path = std::env::temp_dir().join(format!("wbtest-{id}.log"));
        let file = OpenOptions::new().create_new(true).write(true).open(&path).expect("create the test log");
        (path, file)
    }
    fn contents(path: &Path) -> Vec<u8> {
        let mut v = Vec::new();
        std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut v)).expect("read the test log");
        v
    }

    #[test]
    fn output_is_appended_in_order_and_counted() {
        let (path, file) = temp_file("append");
        let mut w = LogWriter::new(file, 1024);
        assert!(w.on());
        w.append(b"first ");
        w.append(b"second");
        w.append(b"");
        assert_eq!((w.bytes(), w.on(), w.stop_reason()), (12, true, None));
        assert_eq!(contents(&path), b"first second");
        w.stop();
        w.append(b"after stop");
        assert_eq!(contents(&path), b"first second", "a stopped writer appends nothing");
        assert_eq!((w.bytes(), w.stop_reason()), (12, Some(StopReason::Stopped)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_cap_stops_the_log_exactly_at_the_limit_and_later_output_is_dropped() {
        let (path, file) = temp_file("cap");
        let mut w = LogWriter::new(file, 8);
        w.append(b"12345");
        assert!(w.on(), "still under the cap");
        w.append(b"6789A"); // only three of these fit
        assert_eq!((w.bytes(), w.stop_reason()), (8, Some(StopReason::Cap)));
        assert_eq!(contents(&path), b"12345678");
        w.append(b"more");
        w.stop();
        assert_eq!(contents(&path), b"12345678");
        assert_eq!(w.stop_reason(), Some(StopReason::Cap), "the cap is the reason it ended, not the later request");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_zero_cap_stops_before_writing_anything() {
        let (path, file) = temp_file("zero");
        let mut w = LogWriter::new(file, 0);
        w.append(b"x");
        assert_eq!((w.bytes(), w.stop_reason()), (0, Some(StopReason::Cap)));
        assert!(contents(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_write_error_stops_logging_instead_of_panicking() {
        let (path, _file) = temp_file("readonly");
        // A read-only handle: every write fails, as it would on a full or protected disk.
        let readonly = std::fs::File::open(&path).expect("open the test log read-only");
        let mut w = LogWriter::new(readonly, 1024);
        w.append(b"cannot be written");
        assert_eq!((w.bytes(), w.on(), w.stop_reason()), (0, false, Some(StopReason::WriteError)));
        assert!(contents(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn names_are_utc_stamped_sanitised_and_never_reuse_a_file() {
        assert_eq!(stamp_utc(0), "19700101T000000Z");
        assert_eq!(stamp_utc(951_782_400_000), "20000229T000000Z", "leap day");
        assert_eq!(stamp_utc(1_792_000_000_000), "20261014T174640Z");
        assert_eq!(file_name("0a1b2c3d", 0), "0a1b2c3d-19700101T000000Z.log");
        // A crafted id cannot escape the folder or name a file directly.
        assert_eq!(file_name(r"..\..\evil", 0), "______evil-19700101T000000Z.log");
        let dir = std::env::temp_dir().join(format!("wbtest-lognames-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
        let logs = super::dir(&dir);
        assert_eq!(logs, dir.join(LOG_DIR));
        assert!(path(&dir, "ab", 0).ends_with("ab-19700101T000000Z.log"), "the folder need not exist yet");
        std::fs::create_dir_all(&logs).expect("create the test folder");
        let first = path(&dir, "ab", 0);
        std::fs::write(&first, b"existing").expect("write a file at the first name");
        let second = path(&dir, "ab", 0);
        assert_eq!(second.file_name().and_then(|n| n.to_str()), Some("ab-19700101T000000Z-2.log"), "a name already on disk is never reused");
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_reason_codes_round_trip() {
        for r in [StopReason::Stopped, StopReason::Cap, StopReason::WriteError, StopReason::Ended] {
            assert_eq!(StopReason::from_code(r.code()), Some(r), "{r:?}");
        }
        assert_eq!(StopReason::from_code("nonsense"), None);
        assert_eq!(StopReason::from_code(""), None);
    }
}
