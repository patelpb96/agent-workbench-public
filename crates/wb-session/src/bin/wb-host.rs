//! Session host: `wb-host <id> <nonce> <cols> <rows> [shell command line]`.
#[cfg(windows)]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    // wb-host <id> <nonce> <cols> <rows> <ring MiB> [shell command line]
    if a.len() < 6 {
        std::process::exit(64);
    }
    // An optional `cwd:<hex>` token (the start folder) may precede the shell command line.
    let start_dir = a.get(6).and_then(|t| t.strip_prefix("cwd:")).and_then(wb_session::auth::unhex).and_then(|b| String::from_utf8(b).ok());
    let first_shell = if start_dir.is_some() { 7 } else { 6 };
    let shell = if a.len() > first_shell { a[first_shell..].join(" ") } else { "cmd.exe".to_owned() };
    let ring_mib: usize = a[5].parse().unwrap_or(1);
    wb_session::host::run_host(wb_session::host::HostArgs {
        id: a[1].clone(),
        nonce: a[2].clone(),
        cols: a[3].parse().unwrap_or(120),
        rows: a[4].parse().unwrap_or(30),
        shell,
        start_dir,
        ring_bytes: ring_mib.saturating_mul(1 << 20),
    });
}
#[cfg(not(windows))]
fn main() {}
