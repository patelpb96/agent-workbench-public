//! Append-only sanitized lifecycle log: `<app_data>/logs/lifecycle.log`.
//! Records process/window/run-loop events, a 10 s heartbeat and panics, so an
//! unexplained exit leaves evidence. Never logs prompts, paths, credentials,
//! provider output or media. Best-effort: logging failures never affect the app.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const LIMIT: u64 = 1024 * 1024;
static FILE: OnceLock<Mutex<PathBuf>> = OnceLock::new();

fn now() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

/// Called once from setup with the natively resolved app-data directory.
pub fn init(app_data: PathBuf) {
    let directory = app_data.join("logs");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("lifecycle.log");
    // Bounded: keep one previous generation of this app-owned log only.
    if fs::metadata(&path)
        .map(|m| m.len() > LIMIT)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, directory.join("lifecycle.1.log"));
    }
    if FILE.set(Mutex::new(path)).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        record(&format!(
            "panic thread={} at={location}",
            thread.name().unwrap_or("unnamed")
        ));
        previous(info);
    }));
    record(&format!(
        "start pid={} version={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ));
    let _ = std::thread::Builder::new()
        .name("lifecycle-heartbeat".into())
        .spawn(|| {
            let mut beat = 0u64;
            loop {
                std::thread::sleep(Duration::from_secs(10));
                beat += 1;
                record(&format!("heartbeat n={beat}"));
            }
        });
}

pub fn record(event: &str) {
    let Some(lock) = FILE.get() else { return };
    let Ok(path) = lock.lock() else { return };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&*path) {
        let _ = writeln!(file, "{} pid={} {event}", now(), std::process::id());
    }
}
