//! Private fault seams only: no real CLI, GUI, production data or environment gates.
use super::*;
use crate::codex::config::ExecutablePin;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS},
    System::Threading::GetCurrentProcess,
};

fn fixture_spec(lease: &CodexLease) -> CodexCommandSpec {
    // The currently running libtest binary is only ever created SUSPENDED in
    // these unit tests. Every post-create case stops it before ResumeThread;
    // this never recursively executes libtest, nor starts codex.exe.
    let path = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let bytes = fs::read(&path).unwrap();
    let pin = ExecutablePin::new(
        path,
        bytes.len() as u64,
        format!("{:x}", Sha256::digest(&bytes)),
    );
    CodexCommandSpec::custom_for_verified_child(pin, lease, vec![], b"fixture".to_vec(), vec![])
        .unwrap()
}

fn duplicate_process(handle: HANDLE) -> WinHandle {
    let mut duplicate = null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_ne!(ok, 0);
    WinHandle(duplicate)
}

#[test]
fn startup_faults_prove_root_exit_before_lease_removal() {
    for fault_phase in [
        StartupPhase::Created,
        StartupPhase::Assigned,
        StartupPhase::InputReady,
    ] {
        let mut lease = CodexLease::create().unwrap();
        let root = lease.root().to_path_buf();
        let spec = fixture_spec(&lease);
        let mut process = None;
        let failure = spawn_contained_inner(
            &spec,
            &CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
            &mut |phase, handle| {
                if phase == StartupPhase::Created {
                    process = Some(duplicate_process(handle.unwrap()));
                }
                if phase == fault_phase {
                    Err(CodexError::StdinWriteFailed)
                } else {
                    Ok(())
                }
            },
        )
        .err()
        .expect("synthetic setup failure");
        assert_eq!(failure.error, CodexError::StdinWriteFailed);
        assert!(failure.tree_quiescent);
        // Independent wait on the exact duplicated kernel process handle, not
        // a reusable PID or trust in the reported boolean alone.
        assert_eq!(
            unsafe { WaitForSingleObject(process.as_ref().unwrap().0, 0) },
            WAIT_OBJECT_0
        );
        assert!(root.exists());
        let report = finish_execution(&mut lease, failure.tree_quiescent, Err(failure.error));
        assert_eq!(report.result.err(), Some(CodexError::StdinWriteFailed));
        assert_eq!(
            report.teardown,
            TeardownEvidence {
                tree_quiescent: true,
                owned_workspace_removed: true
            }
        );
        assert!(!root.exists());
    }
}

#[test]
fn cancellation_after_verification_prevents_creation_and_before_resume_stops_suspended_child() {
    for cancel_phase in [StartupPhase::Verified, StartupPhase::InputReady] {
        let mut lease = CodexLease::create().unwrap();
        let spec = fixture_spec(&lease);
        let cancel = CancelToken::new();
        let mut process = None;
        let failure = spawn_contained_inner(
            &spec,
            &cancel,
            Instant::now() + Duration::from_secs(30),
            &mut |phase, handle| {
                if phase == StartupPhase::Created {
                    process = Some(duplicate_process(handle.unwrap()));
                }
                if phase == cancel_phase {
                    cancel.cancel();
                }
                Ok(())
            },
        )
        .err()
        .expect("cancel must prevent resume");
        assert_eq!(failure.error, CodexError::Cancelled);
        assert!(failure.tree_quiescent);
        if cancel_phase == StartupPhase::Verified {
            assert!(
                process.is_none(),
                "no child may be created after cancelled verification"
            );
        } else {
            assert_eq!(
                unsafe { WaitForSingleObject(process.as_ref().unwrap().0, 0) },
                WAIT_OBJECT_0
            );
        }
        assert!(
            finish_execution(&mut lease, failure.tree_quiescent, Err(failure.error))
                .teardown
                .owned_workspace_removed
        );
    }
}

#[test]
fn expired_startup_deadline_rejects_before_creation() {
    let mut lease = CodexLease::create().unwrap();
    let spec = fixture_spec(&lease);
    let mut visited = false;
    let failure = spawn_contained_inner(&spec, &CancelToken::new(), Instant::now(), &mut |_, _| {
        visited = true;
        Ok(())
    })
    .err()
    .expect("deadline must reject");
    assert_eq!(failure.error, CodexError::TimedOut);
    assert!(failure.tree_quiescent);
    assert!(!visited);
    assert!(
        finish_execution(&mut lease, true, Err(failure.error))
            .teardown
            .owned_workspace_removed
    );
}

#[test]
fn uncertain_teardown_preserves_exact_lease_and_cannot_report_success() {
    let mut lease = CodexLease::create().unwrap();
    let marker_path = lease.root().join(crate::codex::LEASE_MARKER_FILE);
    let marker = fs::read(&marker_path).unwrap();
    // Synthetic false evidence, not an actual unkillable OS process. This tests
    // the final cleanup decision boundary without intentionally leaking a child.
    let report = finish_execution(&mut lease, false, Err(CodexError::SpawnFailed));
    assert_eq!(report.result.err(), Some(CodexError::CleanupFailed));
    assert_eq!(
        report.teardown,
        TeardownEvidence {
            tree_quiescent: false,
            owned_workspace_removed: false
        }
    );
    assert!(!lease.is_cleaned());
    assert_eq!(fs::read(marker_path).unwrap(), marker);
    assert!(lease.workspace().is_dir());
    // Test owns no child; explicitly prove that premise before final cleanup.
    lease.cleanup(true).unwrap();
}

#[test]
fn pre_spawn_failure_and_pre_cancel_reports_prove_no_child_and_cleanup() {
    for precancel in [false, true] {
        let mut lease = CodexLease::create().unwrap();
        let spec = CodexCommandSpec::custom_for_verified_child(
            ExecutablePin::new(
                PathBuf::from("definitely-not-a-real-executable.exe"),
                1,
                "0".repeat(64),
            ),
            &lease,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
        let cfg = CodexDriverConfig::default();
        if precancel {
            cfg.cancel.cancel();
        }
        let report = run_turn_with_evidence(spec, &mut lease, cfg);
        assert_eq!(
            report.result.err(),
            Some(if precancel {
                CodexError::Cancelled
            } else {
                CodexError::InvalidExecutablePin
            })
        );
        assert_eq!(
            report.teardown,
            TeardownEvidence {
                tree_quiescent: true,
                owned_workspace_removed: true
            }
        );
    }
}

#[test]
fn failed_cleanup_overrides_domain_error_without_inventing_removal_proof() {
    let mut lease = CodexLease::create().unwrap();
    let marker_path = lease.root().join(crate::codex::LEASE_MARKER_FILE);
    let marker = fs::read(&marker_path).unwrap();
    fs::write(&marker_path, b"{}").unwrap();
    let report = finish_execution(&mut lease, true, Err(CodexError::InvalidConfig));
    assert_eq!(report.result.err(), Some(CodexError::CleanupFailed));
    assert_eq!(
        report.teardown,
        TeardownEvidence {
            tree_quiescent: true,
            owned_workspace_removed: false
        }
    );
    assert!(lease.workspace().is_dir());
    fs::write(marker_path, marker).unwrap();
    lease.cleanup(true).unwrap();
}

#[test]
fn invalid_lease_is_not_deleted_and_has_no_removal_proof() {
    let mut lease = CodexLease::create().unwrap();
    let spec = fixture_spec(&lease);
    let marker_path = lease.root().join(crate::codex::LEASE_MARKER_FILE);
    let marker = fs::read(&marker_path).unwrap();
    fs::write(&marker_path, b"{}").unwrap();
    let report = run_turn_with_evidence(spec, &mut lease, CodexDriverConfig::default());
    assert_eq!(report.result.err(), Some(CodexError::LeaseError));
    assert_eq!(
        report.teardown,
        TeardownEvidence {
            tree_quiescent: true,
            owned_workspace_removed: false
        }
    );
    assert_eq!(fs::read(&marker_path).unwrap(), b"{}");
    fs::write(marker_path, marker).unwrap();
    lease.cleanup(true).unwrap();
}
