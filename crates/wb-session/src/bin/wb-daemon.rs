//! Broker daemon: `wb-daemon [--data-dir D] [--pipe-name N] [--gate PHRASE]`.
#[cfg(windows)]
fn main() {
    use std::io::Write;
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let data_dir = get("--data-dir").map(Into::into).unwrap_or_else(wb_session::win::default_data_dir);
    let pipe_name = get("--pipe-name").unwrap_or_else(wb_session::win::default_pipe_name);
    let Some(host_exe) = std::env::current_exe().ok().map(|exe| exe.with_file_name("wb-host.exe")) else {
        eprintln!("wb-daemon: cannot locate its own executable, so it cannot find wb-host.exe");
        std::process::exit(1);
    };
    let cfg = wb_session::broker::BrokerConfig { data_dir, pipe_name: pipe_name.clone(), host_exe, gate_phrase: get("--gate").unwrap_or_default() };
    let result = wb_session::broker::run(cfg, |shared| {
        println!("READY {pipe_name} sessions={} lost={}", shared.session_count(), shared.lost_sessions().len());
        let _ = std::io::stdout().flush();
    });
    match result {
        Ok(()) => {}
        Err(wb_session::broker::StartError::AlreadyRunning) => {
            println!("ALREADY_RUNNING");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("wb-daemon failed: {e:?}");
            std::process::exit(1);
        }
    }
}
#[cfg(not(windows))]
fn main() {}
