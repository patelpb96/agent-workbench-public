//! Deterministic Windows process, Job-tree containment, executable pin, lease,
//! and one-shot token tests for `agent_workbench::codex`.
//!
//! Uses `harness = false` so `std::env::current_exe()` invoked with
//! `--codex-mock-child <mode>` acts as a clean JSONL child/grandchild fixture
//! without `libtest` banner noise on stdout, while normal `cargo test` runs
//! the full deterministic test suite without ever invoking real `codex.exe`.

use agent_workbench::codex::{
    build_pinned_ack_args, run_pinned_smoke_once, run_turn_with_evidence, run_turn_with_lease,
    CancelToken, CodexCommandSpec, CodexDriverConfig, CodexError, CodexLease, ExecutablePin,
    OneShotSmokeGate, ProtocolError, LOCKDOWN_CONFIG_OVERRIDES, MAX_LINE_BYTES, MAX_STDERR_BYTES,
    SMOKE_MODEL, SMOKE_PROMPT, SMOKE_PURPOSE,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    },
};

const MOCK_FLAG: &str = "--codex-mock-child";

fn main() {
    let mut args = std::env::args();
    let _bin = args.next();
    if let Some(first) = args.next() {
        if first == MOCK_FLAG {
            let mode = args.next().unwrap_or_default();
            std::process::exit(run_mock_child(&mode));
        }
    }
    // Opt-in C25 root-cause probe; never part of the default unarmed suite.
    if let Ok(n) = std::env::var("AW_C25_PROBE") {
        c25_probe(n.parse().unwrap_or(50));
        return;
    }
    run_test_suite();
}

// The grandchild modes leave a process running on purpose: the tests check that the driver's Job ends it.
#[allow(clippy::zombie_processes)]
fn run_mock_child(mode: &str) -> i32 {
    match mode {
        "sleeper" => {
            thread::sleep(Duration::from_secs(60));
            0
        }
        "happy_ack" => {
            // 1. Verify current_dir matches -C argument.
            let args: Vec<String> = std::env::args().collect();
            let Some(c_pos) = args.iter().position(|a| a == "-C") else {
                return 11;
            };
            let expected_cwd = PathBuf::from(&args[c_pos + 1]);
            let actual_cwd = std::env::current_dir().unwrap();
            if fs::canonicalize(&actual_cwd).unwrap() != fs::canonicalize(&expected_cwd).unwrap() {
                return 12;
            }
            // 2. Verify unallowlisted parent env var was scrubbed.
            if std::env::var_os("WORKBENCH_UNALLOWLISTED_SECRET").is_some() {
                return 13;
            }
            // 3. Verify parent's inheritable decoy file handle was NOT inherited due to
            // PROC_THREAD_ATTRIBUTE_HANDLE_LIST restriction.
            if let Ok(raw_str) = std::env::var("MOCK_DECOY_HANDLE") {
                if let Ok(raw_val) = raw_str.parse::<usize>() {
                    let h = raw_val as HANDLE;
                    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
                    let valid = unsafe { GetFileInformationByHandle(h, &mut info) };
                    if valid != 0 {
                        return 14;
                    }
                }
            }
            // 4. Verify stdin contains the exact prompt and reaches EOF immediately.
            let mut stdin_bytes = Vec::new();
            std::io::stdin().read_to_end(&mut stdin_bytes).unwrap();
            if stdin_bytes != SMOKE_PROMPT.as_bytes() {
                return 15;
            }
            // 5. Emit diagnostic noise on stderr (must be drained and never exposed).
            let mut err = std::io::stderr();
            err.write_all(b"PRIVATE_STDERR_DIAGNOSTIC_PATH=C:\\secret\\token\n")
                .unwrap();
            err.flush().unwrap();
            // 6. Emit pinned 0.163.0 JSONL lifecycle with reasoning + agent_message + explicit cache_write_input_tokens=0.
            emit_valid_turn("ACK", true);
            0
        }
        "large_stdin_ack" => {
            // The whole 8 KiB prompt must arrive intact (it was written before this child ran).
            let mut stdin_bytes = Vec::new();
            std::io::stdin().read_to_end(&mut stdin_bytes).unwrap();
            if stdin_bytes != "x".repeat(8192).as_bytes() {
                return 16;
            }
            emit_valid_turn("ACK", true);
            0
        }
        "forbidden_tool" => {
            emit_line(&json!({"type":"thread.started","thread_id":"thread_tool_1"}));
            emit_line(&json!({"type":"turn.started"}));
            emit_line(&json!({
                "type":"item.completed",
                "item":{
                    "id":"item_cmd_1",
                    "type":"command_execution",
                    "command":"whoami",
                    "aggregated_output":"PRIVATE_OUTPUT",
                    "exit_code":0,
                    "status":"completed"
                }
            }));
            thread::sleep(Duration::from_secs(30));
            0
        }
        "post_message_tool" => {
            emit_line(&json!({"type":"thread.started","thread_id":"thread_tool_2"}));
            emit_line(&json!({"type":"turn.started"}));
            emit_line(&json!({
                "type":"item.completed",
                "item":{"id":"msg_1","type":"agent_message","text":"ACK"}
            }));
            emit_line(&json!({
                "type":"item.completed",
                "item":{"id":"patch_1","type":"file_change","changes":[],"status":"completed"}
            }));
            0
        }
        "turn_failed" => {
            emit_line(&json!({"type":"thread.started","thread_id":"thread_fail_1"}));
            emit_line(&json!({"type":"turn.started"}));
            emit_line(&json!({
                "type":"turn.failed",
                "error":{"message":"C:\\Users\\private\\auth.json secret failure"}
            }));
            1
        }
        "nonzero_after_complete" => {
            emit_valid_turn("ACK", false);
            2
        }
        "nonzero_early_crash" => {
            let _ = std::io::stderr().write_all(b"CRASH_SECRET\n");
            3
        }
        "stderr_overflow" => {
            let mut err = std::io::stderr();
            let buf = vec![b'E'; MAX_STDERR_BYTES + 4096];
            let _ = err.write_all(&buf);
            let _ = err.flush();
            thread::sleep(Duration::from_secs(30));
            0
        }
        "stdout_line_overflow" => {
            let mut out = std::io::stdout();
            let buf = vec![b'A'; MAX_LINE_BYTES + 1024];
            let _ = out.write_all(&buf);
            let _ = out.flush();
            thread::sleep(Duration::from_secs(30));
            0
        }
        "grandchild_hang" => {
            let grandchild = spawn_sleeper_grandchild(true);
            record_grandchild_pid(grandchild.id());
            emit_line(&json!({"type":"thread.started","thread_id":"thread_hang_1"}));
            thread::sleep(Duration::from_secs(60));
            0
        }
        "grandchild_linger_pipes_closed" => {
            let grandchild = spawn_sleeper_grandchild(false);
            record_grandchild_pid(grandchild.id());
            emit_valid_turn("ACK", false);
            0
        }
        "grandchild_linger_pipes_open" => {
            let grandchild = spawn_sleeper_grandchild(true);
            record_grandchild_pid(grandchild.id());
            emit_valid_turn("ACK", false);
            0
        }
        _ => 99,
    }
}

fn emit_line(val: &serde_json::Value) {
    let mut out = std::io::stdout();
    writeln!(out, "{val}").unwrap();
    out.flush().unwrap();
}

fn emit_valid_turn(message: &str, include_reasoning: bool) {
    emit_line(&json!({"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}));
    emit_line(&json!({"type":"turn.started"}));
    if include_reasoning {
        emit_line(&json!({
            "type":"item.completed",
            "item":{"id":"item_0","type":"reasoning","text":"PRIVATE_CHAIN_OF_THOUGHT"}
        }));
    }
    emit_line(&json!({
        "type":"item.completed",
        "item":{"id":"item_1","type":"agent_message","text":message}
    }));
    emit_line(&json!({
        "type":"turn.completed",
        "usage":{
            "input_tokens":18,
            "cached_input_tokens":0,
            "cache_write_input_tokens":0,
            "output_tokens":1,
            "reasoning_output_tokens":4
        }
    }));
}

fn spawn_sleeper_grandchild(inherit_stdio: bool) -> std::process::Child {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.arg(MOCK_FLAG).arg("sleeper");
    if inherit_stdio {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    cmd.spawn().expect("grandchild sleeper must spawn")
}

fn record_grandchild_pid(pid: u32) {
    if let Ok(path) = std::env::var("MOCK_GRANDCHILD_PID_FILE") {
        let _ = fs::write(path, pid.to_string());
    }
}

fn current_exe_pin() -> ExecutablePin {
    let path = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = format!("{:x}", hasher.finalize());
    ExecutablePin::new(path, bytes.len() as u64, digest)
}

fn make_mock_spec(
    pin: &ExecutablePin,
    lease: &CodexLease,
    mode: &str,
    extra_env: Vec<(OsString, OsString)>,
) -> CodexCommandSpec {
    let args = vec![
        OsString::from(MOCK_FLAG),
        OsString::from(mode),
        OsString::from("-C"),
        lease.workspace().as_os_str().to_os_string(),
    ];
    CodexCommandSpec::custom_for_verified_child(
        pin.clone(),
        lease,
        args,
        SMOKE_PROMPT.as_bytes().to_vec(),
        extra_env,
    )
    .unwrap()
}

fn run_test_suite() {
    let tests: &[(&str, fn())] = &[
        (
            "pin_verification_locks_file_against_concurrent_writes_and_rejects_bad_pins",
            test_pin_verification_and_write_lock,
        ),
        (
            "pinned_ack_smoke_spec_validates_all_isolation_flags_and_preserves_permission_instructions",
            test_pinned_ack_smoke_spec,
        ),
        (
            "lease_rejects_unconfirmed_quiescence_tampered_nonce_and_general_temp",
            test_lease_guards,
        ),
        (
            "one_shot_smoke_gate_requires_env_and_valid_token_and_consumes_atomically_once",
            test_one_shot_smoke_gate,
        ),
        (
            "happy_path_mock_child_verifies_stdin_cwd_env_scrub_handle_restriction_and_explicit_zero_cache_write",
            test_happy_path_and_handle_restriction,
        ),
        (
            "protocol_violations_and_nonzero_exit_fail_closed_and_clean_up",
            test_protocol_violations_and_nonzero_exit,
        ),
        (
            "stdout_and_stderr_overflows_kill_job_immediately_and_clean_up",
            test_stdout_and_stderr_overflows,
        ),
        (
            "timeout_and_cancellation_terminate_entire_child_and_grandchild_process_tree",
            test_timeout_and_cancel_kill_grandchild_tree,
        ),
        (
            "lingering_grandchild_after_root_exit_is_rejected_and_terminated",
            test_lingering_grandchild_after_root_exit,
        ),
        (
            "supervised_fixture_runs_complete_cancel_time_out_contain_and_refuse_bad_pins",
            test_supervised_fixture_runs,
        ),
        (
            "codex_text_v1_spec_reuses_locked_down_smoke_args_and_bounds_prompt_bytes",
            test_codex_text_v1_spec,
        ),
        (
            "codex_text_v2_spec_has_the_same_lockdown_and_a_larger_bounded_stdin",
            test_codex_text_v2_spec,
        ),
        (
            "an_8_kib_stdin_is_written_before_resume_without_blocking_the_launch",
            test_large_stdin_does_not_block_launch,
        ),
    ];

    println!("\nrunning {} tests", tests.len());
    let started = Instant::now();
    for (name, func) in tests {
        func();
        println!("test codex_process::{name} ... ok");
    }
    println!(
        "\ntest result: ok. {} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in {:.2}s\n",
        tests.len(),
        started.elapsed().as_secs_f64()
    );
}

fn test_pin_verification_and_write_lock() {
    let pin = current_exe_pin();
    let verified = pin.verify_and_lock().unwrap();
    assert_eq!(verified.canonical_path(), pin.path.as_path());
    drop(verified);

    // Create a temporary .exe in an owned lease to prove FILE_SHARE_READ blocks concurrent writers.
    let mut lease = CodexLease::create().unwrap();
    let sample_exe = lease.workspace().join("sample.exe");
    let sample_bytes = b"MZ_TEST_BINARY_PAYLOAD";
    fs::write(&sample_exe, sample_bytes).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(sample_bytes);
    let sample_sha = format!("{:x}", hasher.finalize());

    let sample_pin = ExecutablePin::new(
        sample_exe.clone(),
        sample_bytes.len() as u64,
        sample_sha.clone(),
    );
    let lock_guard = sample_pin.verify_and_lock().unwrap();

    // While lock_guard is alive, opening sample.exe for write or append must fail!
    assert!(OpenOptions::new().write(true).open(&sample_exe).is_err());
    drop(lock_guard);
    assert!(OpenOptions::new().write(true).open(&sample_exe).is_ok());

    // Wrong size, wrong hash, and non-.exe extension must all reject with InvalidExecutablePin.
    let bad_size = ExecutablePin::new(
        sample_exe.clone(),
        sample_bytes.len() as u64 + 1,
        sample_sha.clone(),
    );
    assert_eq!(
        bad_size.verify_and_lock().err(),
        Some(CodexError::InvalidExecutablePin)
    );

    let bad_hash = ExecutablePin::new(sample_exe, sample_bytes.len() as u64, "0".repeat(64));
    assert_eq!(
        bad_hash.verify_and_lock().err(),
        Some(CodexError::InvalidExecutablePin)
    );

    let cmd_shim = lease.workspace().join("codex.cmd");
    fs::write(&cmd_shim, sample_bytes).unwrap();
    let bad_ext = ExecutablePin::new(cmd_shim, sample_bytes.len() as u64, sample_sha);
    assert_eq!(
        bad_ext.verify_and_lock().err(),
        Some(CodexError::InvalidExecutablePin)
    );

    lease.cleanup(true).unwrap();
}

fn test_pinned_ack_smoke_spec() {
    let mut lease = CodexLease::create().unwrap();
    std::env::set_var(
        "WORKBENCH_UNALLOWLISTED_SECRET",
        "TOP_SECRET_DO_NOT_INHERIT",
    );
    let spec = CodexCommandSpec::pinned_ack_smoke(&lease).unwrap();
    std::env::remove_var("WORKBENCH_UNALLOWLISTED_SECRET");

    let args: Vec<String> = spec
        .args()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &args[..4],
        &["--ask-for-approval", "never", "--no-daemon", "exec"]
    );
    for required in [
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "--skip-git-repo-check",
        "--strict-config",
        "--sandbox",
        "read-only",
        "--model",
        SMOKE_MODEL,
    ] {
        assert!(args.iter().any(|a| a == required), "missing {required}");
    }
    for forbidden in [
        "--ignore-rules",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--approve-for-me",
        "--full-auto",
    ] {
        assert!(
            !args.iter().any(|a| a == forbidden),
            "forbidden {forbidden}"
        );
    }
    assert!(
        !args
            .iter()
            .any(|a| a.contains("include_permissions_instructions")),
        "permission instructions must never be suppressed"
    );
    for &(k, v) in LOCKDOWN_CONFIG_OVERRIDES {
        let expected = format!("{k}={v}");
        assert!(args.iter().any(|a| a == &expected), "missing -c {expected}");
    }
    for legacy_alias in [
        "experimental_use_unified_exec_tool",
        "features.connectors=",
        "features.request_permissions=",
        "features.web_search=",
        "features.collab=",
        "features.memory_tool=",
        "features.codex_hooks=",
    ] {
        assert!(
            !args.iter().any(|a| a.starts_with(legacy_alias)),
            "legacy alias {legacy_alias} must not be passed"
        );
    }
    assert!(args
        .iter()
        .any(|a| a == "suppress_unstable_features_warning=true"));
    assert!(args
        .iter()
        .any(|a| a == "features.exec_permission_approvals=false"));
    assert_eq!(args.last().map(String::as_str), Some("-"));
    assert_eq!(spec.stdin_prompt(), SMOKE_PROMPT.as_bytes());
    assert!(!spec
        .env_vars()
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("WORKBENCH_UNALLOWLISTED_SECRET")));

    let direct_args = build_pinned_ack_args(lease.workspace()).unwrap();
    assert_eq!(direct_args, spec.args());
    lease.cleanup(true).unwrap();
}

fn test_lease_guards() {
    let mut lease = CodexLease::create().unwrap();
    let root = lease.root().to_path_buf();
    assert!(root.exists());

    // Cleanup must refuse when tree_quiescent is false and preserve the directory.
    assert_eq!(lease.cleanup(false), Err(CodexError::CleanupFailed));
    assert!(root.exists());
    assert!(!lease.is_cleaned());

    // Corrupting the ownership marker nonce must cause validation and cleanup to refuse.
    let marker_path = root.join("codex-lease.json");
    let original_marker = fs::read(&marker_path).unwrap();
    let tampered = json!({
        "root": root,
        "nonce": Uuid::new_v4().to_string()
    });
    fs::write(&marker_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    assert_eq!(lease.validate(), Err(CodexError::LeaseError));
    assert_eq!(lease.cleanup(true), Err(CodexError::CleanupFailed));

    // Restoring the exact marker allows verified cleanup.
    fs::write(&marker_path, original_marker).unwrap();
    lease.cleanup(true).unwrap();
    assert!(!root.exists());
    assert!(lease.is_cleaned());
}

fn test_one_shot_smoke_gate() {
    let mut lease = CodexLease::create().unwrap();
    let token_path = lease.workspace().join("approval-token.json");
    let nonce = Uuid::new_v4().to_string();
    let valid_token = json!({
        "schema_version": 1,
        "purpose": SMOKE_PURPOSE,
        "model": SMOKE_MODEL,
        "nonce": nonce
    });
    fs::write(&token_path, serde_json::to_vec(&valid_token).unwrap()).unwrap();

    // Without env="1", token is untouched and gate refuses.
    assert_eq!(
        OneShotSmokeGate::consume_with_env(&token_path, None),
        Err(CodexError::SmokeGateNotArmed)
    );
    assert_eq!(
        OneShotSmokeGate::consume_with_env(&token_path, Some(OsString::from("0").as_os_str())),
        Err(CodexError::SmokeGateNotArmed)
    );
    assert!(token_path.exists());

    // First armed consumption succeeds and removes the token file.
    let consumed =
        OneShotSmokeGate::consume_with_env(&token_path, Some(OsString::from("1").as_os_str()))
            .unwrap();
    assert_eq!(consumed, nonce);
    assert!(!token_path.exists());

    // Second armed attempt fails because the token was already consumed.
    assert_eq!(
        OneShotSmokeGate::consume_with_env(&token_path, Some(OsString::from("1").as_os_str())),
        Err(CodexError::SmokeGateNotArmed)
    );

    // A malformed token is still atomically deleted on first attempt so it cannot be retried.
    fs::write(&token_path, b"{\"schema_version\":99}").unwrap();
    assert_eq!(
        OneShotSmokeGate::consume_with_env(&token_path, Some(OsString::from("1").as_os_str())),
        Err(CodexError::SmokeGateConsumeFailed)
    );
    assert!(!token_path.exists());

    // run_pinned_smoke_once refuses before lease/process creation when unarmed.
    assert_eq!(
        run_pinned_smoke_once(&token_path, CodexDriverConfig::default()).err(),
        Some(CodexError::SmokeGateNotArmed)
    );

    lease.cleanup(true).unwrap();
}

fn test_happy_path_and_handle_restriction() {
    use std::os::windows::io::AsRawHandle;

    let pin = current_exe_pin();
    let mut lease = CodexLease::create().unwrap();
    let root = lease.root().to_path_buf();

    // Open an inheritable decoy file in the parent to verify PROC_THREAD_ATTRIBUTE_HANDLE_LIST
    // blocks it from leaking into the child process.
    let decoy_path = std::env::temp_dir().join(format!("aw-decoy-{}.tmp", Uuid::new_v4()));
    let decoy_file = File::create(&decoy_path).unwrap();
    let raw_decoy = decoy_file.as_raw_handle() as usize;
    unsafe {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        let _ = SetHandleInformation(
            decoy_file.as_raw_handle() as HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        );
    }

    std::env::set_var("WORKBENCH_UNALLOWLISTED_SECRET", "DO_NOT_LEAK");
    let spec = make_mock_spec(
        &pin,
        &lease,
        "happy_ack",
        vec![(
            OsString::from("MOCK_DECOY_HANDLE"),
            OsString::from(raw_decoy.to_string()),
        )],
    );
    std::env::remove_var("WORKBENCH_UNALLOWLISTED_SECRET");

    let report = run_turn_with_evidence(spec, &mut lease, CodexDriverConfig::default());
    assert!(report.teardown.tree_quiescent && report.teardown.owned_workspace_removed);
    let outcome = report.result.unwrap();
    drop(decoy_file);
    let _ = fs::remove_file(&decoy_path);

    assert_eq!(outcome.turn.message, "ACK");
    assert_eq!(
        outcome.turn.thread_id,
        "0199a213-81c0-7800-8aa1-bbab2a035a53"
    );
    assert_eq!(outcome.turn.reasoning_items_discarded, 1);
    assert_eq!(outcome.turn.usage.input_tokens, 18);
    assert_eq!(outcome.turn.usage.cache_write_input_tokens, Some(0));
    assert_eq!(outcome.turn.usage.output_tokens, 1);
    assert!(outcome.stderr_bytes_drained > 0);
    // On Windows, CREATE_NO_WINDOW for a console binary spawns the child plus at most
    // one headless conhost.exe inside the Job, both of which must exit (ActiveProcesses == 0).
    assert!((1..=2).contains(&outcome.total_job_processes));
    assert!(outcome.owned_workspace_removed);
    assert!(!root.exists());
}

fn test_protocol_violations_and_nonzero_exit() {
    let pin = current_exe_pin();
    let cases: &[(&str, CodexError)] = &[
        (
            "forbidden_tool",
            CodexError::Protocol(ProtocolError::ForbiddenActivity),
        ),
        (
            "post_message_tool",
            CodexError::Protocol(ProtocolError::ForbiddenActivity),
        ),
        (
            "turn_failed",
            CodexError::Protocol(ProtocolError::ProviderFailure),
        ),
        ("nonzero_after_complete", CodexError::NonZeroExit),
        ("nonzero_early_crash", CodexError::NonZeroExit),
    ];

    for &(mode, expected_err) in cases {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let spec = make_mock_spec(&pin, &lease, mode, Vec::new());
        let started = Instant::now();
        let report = run_turn_with_evidence(spec, &mut lease, CodexDriverConfig::default());
        assert!(report.teardown.tree_quiescent && report.teardown.owned_workspace_removed);
        let err = report.result.err().expect("must fail closed");
        assert_eq!(err, expected_err, "mode {mode}");
        // Even when forbidden_tool tries to sleep 30s after emitting the tool event,
        // the driver kills the Job immediately (< 3s).
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "mode {mode} hung"
        );
        assert!(!format!("{err:?}").contains("PRIVATE"));
        assert!(!format!("{err}").contains("PRIVATE"));
        assert!(lease.is_cleaned(), "lease must be cleaned for {mode}");
        assert!(!root.exists(), "root must be removed for {mode}");
    }
}

fn test_stdout_and_stderr_overflows() {
    let pin = current_exe_pin();
    let cases: &[(&str, CodexError)] = &[
        ("stderr_overflow", CodexError::StderrLimitExceeded),
        (
            "stdout_line_overflow",
            CodexError::Protocol(ProtocolError::LimitExceeded),
        ),
    ];
    for &(mode, expected_err) in cases {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let spec = make_mock_spec(&pin, &lease, mode, Vec::new());
        let started = Instant::now();
        let err = run_turn_with_lease(spec, &mut lease, CodexDriverConfig::default())
            .err()
            .expect("overflow must fail closed");
        assert_eq!(err, expected_err, "mode {mode}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "mode {mode} hung"
        );
        assert!(lease.is_cleaned());
        assert!(!root.exists());
    }
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for grandchild PID"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn open_process_sync(pid: u32) -> usize {
    let h = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(!h.is_null(), "OpenProcess failed for grandchild PID {pid}");
    h as usize
}

fn assert_process_signaled_and_close(raw: usize) {
    let h = raw as HANDLE;
    let wait = unsafe { WaitForSingleObject(h, 0) };
    unsafe {
        let _ = CloseHandle(h);
    }
    assert_eq!(
        wait, WAIT_OBJECT_0,
        "grandchild process must already be terminated by Job before driver returns"
    );
}

fn test_timeout_and_cancel_kill_grandchild_tree() {
    let pin = current_exe_pin();

    // 1. Wall-clock timeout kills both child and grandchild sleeper.
    {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let pid_file = std::env::temp_dir().join(format!("aw-gc-timeout-{}.pid", Uuid::new_v4()));
        let spec = make_mock_spec(
            &pin,
            &lease,
            "grandchild_hang",
            vec![(
                OsString::from("MOCK_GRANDCHILD_PID_FILE"),
                pid_file.as_os_str().to_os_string(),
            )],
        );
        let pid_file_bg = pid_file.clone();
        let opener = thread::spawn(move || {
            let pid = wait_for_pid_file(&pid_file_bg);
            open_process_sync(pid)
        });
        let cfg = CodexDriverConfig {
            timeout: Duration::from_millis(350),
            cancel: CancelToken::new(),
        };
        let err = run_turn_with_lease(spec, &mut lease, cfg).err().unwrap();
        let gc_handle = opener.join().unwrap();
        let _ = fs::remove_file(&pid_file);

        assert_eq!(err, CodexError::TimedOut);
        assert_process_signaled_and_close(gc_handle);
        assert!(lease.is_cleaned());
        assert!(!root.exists());
    }

    // 2. Explicit CancelToken kills both child and grandchild sleeper immediately.
    {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let pid_file = std::env::temp_dir().join(format!("aw-gc-cancel-{}.pid", Uuid::new_v4()));
        let spec = make_mock_spec(
            &pin,
            &lease,
            "grandchild_hang",
            vec![(
                OsString::from("MOCK_GRANDCHILD_PID_FILE"),
                pid_file.as_os_str().to_os_string(),
            )],
        );
        let cancel = CancelToken::new();
        let cancel_bg = cancel.clone();
        let pid_file_bg = pid_file.clone();
        let canceller = thread::spawn(move || {
            let pid = wait_for_pid_file(&pid_file_bg);
            let h = open_process_sync(pid);
            cancel_bg.cancel();
            h
        });
        let cfg = CodexDriverConfig {
            timeout: Duration::from_secs(30),
            cancel,
        };
        let err = run_turn_with_lease(spec, &mut lease, cfg).err().unwrap();
        let gc_handle = canceller.join().unwrap();
        let _ = fs::remove_file(&pid_file);

        assert_eq!(err, CodexError::Cancelled);
        assert_process_signaled_and_close(gc_handle);
        assert!(lease.is_cleaned());
        assert!(!root.exists());
    }
}

fn test_lingering_grandchild_after_root_exit() {
    let pin = current_exe_pin();
    for mode in [
        "grandchild_linger_pipes_closed",
        "grandchild_linger_pipes_open",
    ] {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let pid_file = std::env::temp_dir().join(format!("aw-gc-linger-{}.pid", Uuid::new_v4()));
        let spec = make_mock_spec(
            &pin,
            &lease,
            mode,
            vec![(
                OsString::from("MOCK_GRANDCHILD_PID_FILE"),
                pid_file.as_os_str().to_os_string(),
            )],
        );
        let pid_file_bg = pid_file.clone();
        let opener = thread::spawn(move || {
            let pid = wait_for_pid_file(&pid_file_bg);
            open_process_sync(pid)
        });
        let err = run_turn_with_lease(spec, &mut lease, CodexDriverConfig::default())
            .err()
            .expect("lingering grandchild must fail closed");
        let gc_handle = opener.join().unwrap();
        let _ = fs::remove_file(&pid_file);

        assert_eq!(err, CodexError::JobContainmentFailed, "mode {mode}");
        assert_process_signaled_and_close(gc_handle);
        assert!(lease.is_cleaned());
        assert!(!root.exists());
    }
}

/// C25 root-cause probe. Repeats the lingering-grandchild scenario and, when the
/// independent handle is NOT already signaled, classifies why:
/// - `wrong_process`: handle image/PID is not the recorded mock grandchild
///   (non-atomic PID file read or PID reuse in the test harness);
/// - `late_signal`: correct grandchild, signaled shortly after the driver
///   returned (Job ActiveProcesses reached 0 before the process object signal);
/// - `never_signaled`: correct grandchild still alive (real containment failure).
fn c25_probe(iterations: usize) {
    use windows_sys::Win32::System::Threading::{
        GetProcessId, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    };
    let pin = current_exe_pin();
    let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let image = |h: HANDLE| -> Option<PathBuf> {
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        (ok != 0)
            .then(|| fs::canonicalize(String::from_utf16_lossy(&buf[..len as usize])).ok())
            .flatten()
    };
    let (mut ok, mut wrong, mut late, mut never, mut open_failed) = (0, 0, 0, 0, 0);
    let mut late_ms: Vec<u128> = Vec::new();
    for i in 0..iterations {
        let mode = if i % 2 == 0 {
            "grandchild_linger_pipes_closed"
        } else {
            "grandchild_linger_pipes_open"
        };
        let mut lease = CodexLease::create().unwrap();
        let pid_file = std::env::temp_dir().join(format!("aw-c25-probe-{}.pid", Uuid::new_v4()));
        let spec = make_mock_spec(
            &pin,
            &lease,
            mode,
            vec![(
                OsString::from("MOCK_GRANDCHILD_PID_FILE"),
                pid_file.as_os_str().to_os_string(),
            )],
        );
        let pf = pid_file.clone();
        let opener = thread::spawn(move || {
            let pid = wait_for_pid_file(&pf);
            let h = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            (pid, h as usize)
        });
        let err = run_turn_with_lease(spec, &mut lease, CodexDriverConfig::default()).err();
        let returned = Instant::now();
        let (read_pid, raw) = opener.join().unwrap();
        let final_pid: Option<u32> = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse().ok());
        let _ = fs::remove_file(&pid_file);
        assert_eq!(err, Some(CodexError::JobContainmentFailed));
        let h = raw as HANDLE;
        if h.is_null() {
            open_failed += 1;
            println!("iter {i} {mode}: open_failed read_pid={read_pid} final_pid={final_pid:?}");
            continue;
        }
        // PID identity only; image lookup can fail for a process mid-exit.
        let pid_ok = Some(read_pid) == final_pid && unsafe { GetProcessId(h) } == read_pid;
        let first = unsafe { WaitForSingleObject(h, 0) };
        if first == WAIT_OBJECT_0 {
            ok += 1;
        } else {
            let image_now = image(h);
            let later = unsafe { WaitForSingleObject(h, 2000) } == WAIT_OBJECT_0;
            let after = returned.elapsed().as_micros();
            let image_after = image(h);
            if !pid_ok || image_after.as_deref().is_some_and(|p| p != exe.as_path()) {
                wrong += 1;
                println!("iter {i} {mode}: WRONG_PROCESS read_pid={read_pid} final_pid={final_pid:?} image_now={image_now:?} image_after={image_after:?} later={later}");
            } else if later {
                late += 1;
                late_ms.push(after);
                println!("iter {i} {mode}: LATE_SIGNAL after {after}us image_now={image_now:?} image_after={image_after:?}");
            } else {
                never += 1;
                println!("iter {i} {mode}: NEVER_SIGNALED pid={read_pid} image={image_now:?}");
            }
        }
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    println!("C25_PROBE iterations={iterations} signaled={ok} wrong_process={wrong} late_signal={late} never_signaled={never} open_failed={open_failed} late_us={late_ms:?}");
}

fn supervised_arm(prompt: &str) -> (String, agent_workbench::supervisor::DispatchArm) {
    use agent_workbench::execution_v2 as v2;
    use agent_workbench::supervisor::Supervisor;
    let now = "2026-10-21T00:00:00.000Z".to_string();
    let run_id = Uuid::new_v4().to_string();
    let snapshot = v2::RunSnapshot {
        contract_version: 2,
        workspace_id: Uuid::new_v4().to_string(),
        session_id: Uuid::new_v4().to_string(),
        run_id: run_id.clone(),
        provider_id: v2::ProviderId::Codex,
        profile_id: Some(v2::CodexProfileId::CodexTextV1),
        model_id: Some(v2::CodexModelId::Gpt61Sol),
        state: v2::RunState::Queued,
        last_seq: 1,
        created_at: now.clone(),
        updated_at: now.clone(),
        enqueue_order: 1,
        usage: v2::Usage {
            input_tokens: None,
            output_tokens: None,
            estimated: false,
            cached_input_tokens: None,
            cache_write_input_tokens: None,
            reasoning_output_tokens: None,
        },
        terminal_error: None,
    };
    let event = v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: Uuid::new_v4().to_string(),
            workspace_id: snapshot.workspace_id.clone(),
            session_id: snapshot.session_id.clone(),
            run_id: run_id.clone(),
            provider_id: v2::ProviderId::Codex,
            source: v2::EventSource::System,
            seq: 1,
            occurred_at: now,
        },
        v2::EventPayload::RunCreated(v2::RunCreatedPayload {
            state: "queued".into(),
        }),
    );
    let commit = v2::ports::MutationCommit {
        result: snapshot,
        committed_events: vec![event],
        replayed: false,
    };
    let arm = Supervisor::<agent_workbench::supervisor::CodexFactory>::mint_arm(&commit, prompt)
        .expect("fresh admission mints an arm");
    (run_id, arm)
}

/// Progress codes observed by `drain_until_done`, in order (volatile display data).
static PROGRESS_SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

fn drain_until_done(
    s: &mut agent_workbench::supervisor::Supervisor<agent_workbench::supervisor::CodexFactory>,
    on_started: &mut dyn FnMut(),
) -> (bool, Option<agent_workbench::supervisor::SupervisorEvent>) {
    use agent_workbench::supervisor::SupervisorEvent;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut started = false;
    loop {
        for event in s.poll() {
            match event {
                SupervisorEvent::Started { .. } => {
                    started = true;
                    on_started();
                }
                SupervisorEvent::Progress { progress, .. } => {
                    PROGRESS_SEEN.lock().unwrap().push(progress.code());
                }
                done => return (started, Some(done)),
            }
        }
        assert!(Instant::now() < deadline, "supervised run did not finish");
        thread::sleep(Duration::from_millis(2));
    }
}

fn test_supervised_fixture_runs() {
    use agent_workbench::supervisor::{CodexFactory, Supervisor, SupervisorEvent, WorkerOutcome};
    let pin = current_exe_pin();
    let factory = |mode: &'static str, pin: ExecutablePin, timeout: Duration| {
        CodexFactory::with_spec_builder(
            move |lease: &CodexLease, prompt: &str| {
                let args = vec![
                    OsString::from(MOCK_FLAG),
                    OsString::from(mode),
                    OsString::from("-C"),
                    lease.workspace().as_os_str().to_os_string(),
                ];
                CodexCommandSpec::custom_for_verified_child(
                    pin.clone(),
                    lease,
                    args,
                    prompt.as_bytes().to_vec(),
                    vec![],
                )
            },
            timeout,
        )
    };
    let outcome_code = |event: Option<SupervisorEvent>| -> Result<String, &'static str> {
        match event {
            Some(SupervisorEvent::Finished {
                outcome: WorkerOutcome::Completed { message, usage },
                ..
            }) => {
                assert_eq!(usage.input_tokens, 18);
                Ok(message)
            }
            Some(SupervisorEvent::Finished {
                outcome: WorkerOutcome::Failed { code },
                ..
            }) => Err(code),
            Some(SupervisorEvent::Quarantined { code, .. }) => {
                panic!("unexpected quarantine {code}")
            }
            _ => panic!("no terminal supervisor event"),
        }
    };

    // 1. Success: Started only after the child resumed in the Job, then output.
    let mut s = Supervisor::new(factory("happy_ack", pin.clone(), Duration::from_secs(30)));
    let (run_id, arm) = supervised_arm(SMOKE_PROMPT);
    s.dispatch(arm).unwrap();
    assert_eq!(s.active_run().map(|(r, _)| r.to_string()), Some(run_id));
    PROGRESS_SEEN.lock().unwrap().clear();
    let (started, done) = drain_until_done(&mut s, &mut || {});
    assert!(started);
    assert_eq!(outcome_code(done), Ok("ACK".to_string()));
    assert!(s.is_available());
    // Real driver milestones reached the supervisor in protocol order, before Finished.
    let seen = PROGRESS_SEEN.lock().unwrap().clone();
    assert_eq!(seen.first(), Some(&"thread_started"), "{seen:?}");
    assert_eq!(seen.get(1), Some(&"turn_started"), "{seen:?}");
    assert_eq!(seen.last(), Some(&"message_complete"), "{seen:?}");

    // 2. Cancellation after start kills the tree with proven teardown.
    let mut s = Supervisor::new(factory(
        "grandchild_hang",
        pin.clone(),
        Duration::from_secs(30),
    ));
    let (run_id, arm) = supervised_arm(SMOKE_PROMPT);
    s.dispatch(arm).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if s.poll()
            .iter()
            .any(|e| matches!(e, SupervisorEvent::Started { .. }))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert!(s.cancel(&run_id));
    let (_, done) = drain_until_done(&mut s, &mut || {});
    assert_eq!(outcome_code(done), Err("codex_cancelled"));
    assert!(s.is_available());

    // 3. Deadline.
    let mut s = Supervisor::new(factory(
        "grandchild_hang",
        pin.clone(),
        Duration::from_millis(400),
    ));
    s.dispatch(supervised_arm(SMOKE_PROMPT).1).unwrap();
    let (_, done) = drain_until_done(&mut s, &mut || {});
    assert_eq!(outcome_code(done), Err("codex_timeout"));
    assert!(s.is_available());

    // 4. Lingering descendant: rejected, terminated, proven; slot reusable.
    for mode in [
        "grandchild_linger_pipes_closed",
        "grandchild_linger_pipes_open",
    ] {
        let mut s = Supervisor::new(factory(mode, pin.clone(), Duration::from_secs(30)));
        s.dispatch(supervised_arm(SMOKE_PROMPT).1).unwrap();
        let (_, done) = drain_until_done(&mut s, &mut || {});
        assert_eq!(
            outcome_code(done),
            Err("codex_job_containment_failed"),
            "{mode}"
        );
        assert!(s.is_available());
    }

    // 5. Bad pin: refused before any spawn, so no Started event.
    let bad = ExecutablePin::new(
        fs::canonicalize(std::env::current_exe().unwrap()).unwrap(),
        1,
        "0".repeat(64),
    );
    let mut s = Supervisor::new(factory("happy_ack", bad, Duration::from_secs(30)));
    s.dispatch(supervised_arm(SMOKE_PROMPT).1).unwrap();
    let (started, done) = drain_until_done(&mut s, &mut || {});
    assert!(!started);
    assert_eq!(outcome_code(done), Err("codex_invalid_executable_pin"));
    assert!(s.is_available());
}

fn test_codex_text_v1_spec() {
    let mut lease = CodexLease::create().unwrap();
    let smoke = CodexCommandSpec::pinned_ack_smoke(&lease).unwrap();
    let prompt = "😀".repeat(256); // exactly 1024 UTF-8 bytes
    let spec = CodexCommandSpec::codex_text_v1(&lease, &prompt).unwrap();
    assert_eq!(spec.args(), smoke.args());
    assert_eq!(spec.workspace_dir(), smoke.workspace_dir());
    assert_eq!(spec.stdin_prompt(), prompt.as_bytes());
    assert!(spec.args().iter().any(|a| a == "read-only"));
    assert_eq!(
        spec.args().last().map(|a| a.as_os_str()),
        Some(std::ffi::OsStr::new("-"))
    );
    assert!(CodexCommandSpec::codex_text_v1(&lease, "").is_err());
    assert!(CodexCommandSpec::codex_text_v1(&lease, &(prompt + "x")).is_err());
    lease.cleanup(true).unwrap();
}

fn test_codex_text_v2_spec() {
    let mut lease = CodexLease::create().unwrap();
    let v1 = CodexCommandSpec::codex_text_v1(&lease, "x").unwrap();
    let text = "😀".repeat(2048); // exactly 8192 UTF-8 bytes
    let spec = CodexCommandSpec::codex_text_v2(&lease, &text).unwrap();
    // Identical lockdown: only the stdin text differs from v1.
    assert_eq!(spec.args(), v1.args());
    assert_eq!(spec.workspace_dir(), v1.workspace_dir());
    assert_eq!(spec.env_vars(), v1.env_vars());
    assert_eq!(spec.stdin_prompt(), text.as_bytes());
    assert!(CodexCommandSpec::codex_text_v2(&lease, "").is_err());
    assert!(CodexCommandSpec::codex_text_v2(&lease, &(text + "x")).is_err());
    // v1 keeps its own 1024-byte cap.
    assert!(CodexCommandSpec::codex_text_v1(&lease, &"x".repeat(1025)).is_err());
    lease.cleanup(true).unwrap();
}

/// Regression: the prompt is written to the stdin pipe before the child is resumed, so a pipe
/// smaller than the prompt blocked the launch forever (found by review of `codex_text_v2`).
fn test_large_stdin_does_not_block_launch() {
    use agent_workbench::supervisor::{CodexFactory, Supervisor, SupervisorEvent, WorkerOutcome};
    let pin = current_exe_pin();
    let factory = CodexFactory::with_spec_builder(
        move |lease: &CodexLease, prompt: &str| {
            let args = vec![
                OsString::from(MOCK_FLAG),
                OsString::from("large_stdin_ack"),
                OsString::from("-C"),
                lease.workspace().as_os_str().to_os_string(),
            ];
            CodexCommandSpec::custom_for_verified_child(
                pin.clone(),
                lease,
                args,
                prompt.as_bytes().to_vec(),
                vec![],
            )
        },
        Duration::from_secs(30),
    );
    let mut s = Supervisor::new(factory);
    let (_, arm) = supervised_arm(&"x".repeat(8192));
    s.dispatch(arm).unwrap();
    let (started, done) = drain_until_done(&mut s, &mut || {});
    assert!(started, "the child must have been resumed");
    assert!(
        matches!(
            done,
            Some(SupervisorEvent::Finished {
                outcome: WorkerOutcome::Completed { .. },
                ..
            })
        ),
        "an 8 KiB prompt must launch and complete"
    );
}
