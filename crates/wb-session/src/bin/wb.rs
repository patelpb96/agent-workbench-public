//! `wb`: the command line for scripts and agents. All of it lives in `wb_session::cli` (docs/design/wb-automation.md).
#[cfg(windows)]
fn main() {
    std::process::exit(wb_session::cli::run(std::env::args().skip(1).collect()));
}
#[cfg(not(windows))]
fn main() {}
