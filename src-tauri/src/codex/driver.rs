use crate::codex::{
    config::{encode_windows_command_line, encode_windows_environment_block, CodexCommandSpec},
    lease::{CodexLease, OneShotSmokeGate},
    CodexJsonlDecoder, Milestone, ParsedTurn, ProtocolError,
};
use std::{
    ffi::c_void,
    fmt,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, SetHandleInformation, ERROR_BROKEN_PIPE,
        ERROR_INVALID_PARAMETER, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, STILL_ACTIVE,
        WAIT_OBJECT_0,
    },
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
            JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::{CreatePipe, PeekNamedPipe},
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
            InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES,
            STARTUPINFOEXW,
        },
    },
};

pub const MAX_STDERR_BYTES: usize = 65_536;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
const TERMINATE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POST_EXIT_DESCENDANT_GRACE: Duration = Duration::from_millis(100);
const PIPE_BUFFER_BYTES: u32 = 4096;
/// The prompt is written BEFORE the child is resumed, with nobody reading yet, so the whole
/// prompt must fit in the pipe buffer or the write would block forever. Sized for the largest
/// stdin any profile may send (`MAX_CONTEXT_STDIN_BYTES`, 8 KiB) with headroom.
const STDIN_PIPE_BYTES: u32 = 16384;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const SW_HIDE_U16: u16 = 0;
/// Upper bound on Job members tracked during forced teardown; more fails closed.
const MAX_TRACKED_JOB_PROCESSES: usize = 256;

/// Cooperative cancellation signal for terminating the local owned Windows Job.
/// Local termination guarantees OS process-tree containment only; it does NOT
/// prove remote provider HTTP request cancellation.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Sanitized code-only adapter errors. Never embeds paths, stderr, stdout,
/// reasoning text, OS error messages, or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexError {
    SmokeGateNotArmed,
    SmokeGateConsumeFailed,
    InvalidExecutablePin,
    InvalidConfig,
    LeaseError,
    SpawnFailed,
    JobContainmentFailed,
    StdinWriteFailed,
    PipeIoFailed,
    StderrLimitExceeded,
    TimedOut,
    Cancelled,
    NonZeroExit,
    CleanupFailed,
    Protocol(ProtocolError),
}

impl CodexError {
    pub fn code(self) -> &'static str {
        match self {
            Self::SmokeGateNotArmed => "codex_smoke_not_armed",
            Self::SmokeGateConsumeFailed => "codex_smoke_gate_consume_failed",
            Self::InvalidExecutablePin => "codex_invalid_executable_pin",
            Self::InvalidConfig => "codex_invalid_config",
            Self::LeaseError => "codex_lease_error",
            Self::SpawnFailed => "codex_spawn_failed",
            Self::JobContainmentFailed => "codex_job_containment_failed",
            Self::StdinWriteFailed => "codex_stdin_write_failed",
            Self::PipeIoFailed => "codex_pipe_io_failed",
            Self::StderrLimitExceeded => "codex_stderr_limit",
            Self::TimedOut => "codex_timeout",
            Self::Cancelled => "codex_cancelled",
            Self::NonZeroExit => "codex_nonzero_exit",
            Self::CleanupFailed => "codex_cleanup_failed",
            Self::Protocol(err) => err.code(),
        }
    }
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for CodexError {}

impl From<ProtocolError> for CodexError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

#[derive(Clone)]
pub struct CodexDriverConfig {
    pub timeout: Duration,
    pub cancel: CancelToken,
}

impl Default for CodexDriverConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            cancel: CancelToken::new(),
        }
    }
}

/// Returned only after valid protocol completion, stdout EOF, stderr EOF,
/// exit code 0, zero active Job processes, and verified disposable workspace removal.
/// Deliberately omits Debug/Serialize so callers choose evidence fields explicitly.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexTurnOutcome {
    pub turn: ParsedTurn,
    pub stderr_bytes_drained: usize,
    pub total_job_processes: u32,
    pub owned_workspace_removed: bool,
}

/// Native-only teardown facts; not serialized or inferred from an error code.
/// A future supervisor must quarantine its slot if either fact is false, or if
/// the worker disconnects/panics without returning a report at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TeardownEvidence {
    pub tree_quiescent: bool,
    pub owned_workspace_removed: bool,
}

/// Selected sanitized result plus independently verified teardown facts.
/// Deliberately no Debug/Serialize: the successful turn contains private output.
pub struct CodexExecutionReport {
    pub result: Result<CodexTurnOutcome, CodexError>,
    pub teardown: TeardownEvidence,
}

fn finish_execution(
    lease: &mut CodexLease,
    tree_quiescent: bool,
    result: Result<CodexTurnOutcome, CodexError>,
) -> CodexExecutionReport {
    // Never turn an uncertain child teardown into permission to delete a lease.
    let removed = if tree_quiescent {
        lease.cleanup(true).is_ok() && lease.is_cleaned()
    } else {
        false
    };
    let result = if !tree_quiescent || !removed {
        Err(CodexError::CleanupFailed)
    } else {
        result.map(|mut outcome| {
            outcome.owned_workspace_removed = true;
            outcome
        })
    };
    CodexExecutionReport {
        result,
        teardown: TeardownEvidence {
            tree_quiescent,
            owned_workspace_removed: removed,
        },
    }
}

/// Runs the single authorized live Codex ACK smoke task after atomically consuming
/// the one-shot approval token before executable verification or process creation.
pub fn run_pinned_smoke_once(
    token_path: &Path,
    driver_cfg: CodexDriverConfig,
) -> Result<CodexTurnOutcome, CodexError> {
    let _consumed_nonce = OneShotSmokeGate::consume_from_env(token_path)?;
    let mut lease = CodexLease::create()?;
    let spec_result = CodexCommandSpec::pinned_ack_smoke(&lease);
    match spec_result {
        Ok(spec) => run_turn_with_lease(spec, &mut lease, driver_cfg),
        Err(err) => finish_execution(&mut lease, true, Err(err)).result,
    }
}

/// Executes a validated `CodexCommandSpec` inside an owned Windows Job object
/// and cleans up `lease` only after confirmed process-tree quiescence and pipe EOF.
pub fn run_turn_with_lease(
    spec: CodexCommandSpec,
    lease: &mut CodexLease,
    driver_cfg: CodexDriverConfig,
) -> Result<CodexTurnOutcome, CodexError> {
    run_turn_with_evidence(spec, lease, driver_cfg).result
}

/// Detailed entry for native supervision. On lease validation failure no child
/// exists, but removal is unproven. Never clean a lease whose identity failed.
/// Panic/unwind is NOT a report: callers must treat disconnection as uncertainty.
pub fn run_turn_with_evidence(
    spec: CodexCommandSpec,
    lease: &mut CodexLease,
    driver_cfg: CodexDriverConfig,
) -> CodexExecutionReport {
    run_turn_with_evidence_notify(spec, lease, driver_cfg, &mut || {})
}

/// As `run_turn_with_evidence`, calling `on_resumed` exactly once, only after the
/// child's main thread was resumed INSIDE the owned Job (the supervisor's spawn
/// acknowledgement). Not called if startup fails before resumption.
pub fn run_turn_with_evidence_notify(
    spec: CodexCommandSpec,
    lease: &mut CodexLease,
    driver_cfg: CodexDriverConfig,
    on_resumed: &mut dyn FnMut(),
) -> CodexExecutionReport {
    run_turn_with_progress(spec, lease, driver_cfg, on_resumed, &mut |_| {})
}

/// As `run_turn_with_evidence_notify`, additionally reporting each decoded
/// lifecycle milestone from the driver thread, in order, as the pinned CLI
/// emits it. Volatile display only: the callback must not block.
fn diagnose(error: ProtocolError, decoder: &CodexJsonlDecoder) {
    diagnose_trace(error, &decoder.trace());
}

/// Sanitized, text-free record of a rejected event order, via the native sink.
fn diagnose_trace(error: ProtocolError, trace: &str) {
    crate::codex::diagnostic(&format!("codex protocol_failure code={} events={trace}", error.code()));
}

pub fn run_turn_with_progress(
    spec: CodexCommandSpec,
    lease: &mut CodexLease,
    driver_cfg: CodexDriverConfig,
    on_resumed: &mut dyn FnMut(),
    on_progress: &mut dyn FnMut(Milestone),
) -> CodexExecutionReport {
    if let Err(error) = lease.validate() {
        return CodexExecutionReport {
            result: Err(error),
            teardown: TeardownEvidence {
                tree_quiescent: true,
                owned_workspace_removed: false,
            },
        };
    }
    if driver_cfg.timeout.is_zero() {
        return finish_execution(lease, true, Err(CodexError::InvalidConfig));
    }
    let deadline = match Instant::now().checked_add(driver_cfg.timeout) {
        Some(deadline) => deadline,
        None => return finish_execution(lease, true, Err(CodexError::InvalidConfig)),
    };
    if let Err(error) = check_startup_control(&driver_cfg.cancel, deadline) {
        return finish_execution(lease, true, Err(error));
    }

    let (mut child, stdout_pipe, stderr_pipe) =
        match spawn_contained(&spec, &driver_cfg.cancel, deadline) {
            Ok(spawned) => {
                on_resumed();
                spawned
            }
            Err(failure) => {
                return finish_execution(lease, failure.tree_quiescent, Err(failure.error));
            }
        };

    let mut stdout_read = Some(stdout_pipe);
    let mut stderr_read = Some(stderr_pipe);
    let mut decoder = CodexJsonlDecoder::new();
    let mut stderr_bytes_drained = 0usize;
    let mut scratch = [0u8; 8192];
    let mut root_exited_since: Option<Instant> = None;

    let run_result: Result<(ParsedTurn, u32), CodexError> = (|| {
        loop {
            if driver_cfg.cancel.is_cancelled() {
                return Err(CodexError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(CodexError::TimedOut);
            }

            let mut progressed = false;

            if let Some(ref pipe) = stderr_read {
                loop {
                    match peek_and_read_pipe(pipe.0, &mut scratch) {
                        PipePoll::Bytes(n) => {
                            progressed = true;
                            scratch[..n].fill(0);
                            stderr_bytes_drained = stderr_bytes_drained.saturating_add(n);
                            if stderr_bytes_drained > MAX_STDERR_BYTES {
                                return Err(CodexError::StderrLimitExceeded);
                            }
                        }
                        PipePoll::Empty => break,
                        PipePoll::Eof => {
                            progressed = true;
                            stderr_read = None;
                            break;
                        }
                        PipePoll::Error => return Err(CodexError::PipeIoFailed),
                    }
                }
            }

            if let Some(ref pipe) = stdout_read {
                loop {
                    match peek_and_read_pipe(pipe.0, &mut scratch) {
                        PipePoll::Bytes(n) => {
                            progressed = true;
                            let feed_res = decoder.feed(&scratch[..n]);
                            scratch[..n].fill(0);
                            if let Err(error) = feed_res {
                                diagnose(error, &decoder);
                                return Err(error.into());
                            }
                            for milestone in decoder.take_milestones() {
                                on_progress(milestone);
                            }
                        }
                        PipePoll::Empty => break,
                        PipePoll::Eof => {
                            progressed = true;
                            stdout_read = None;
                            break;
                        }
                        PipePoll::Error => return Err(CodexError::PipeIoFailed),
                    }
                }
            }

            if stdout_read.is_none() && stderr_read.is_none() {
                break;
            }

            if child.is_root_signaled() {
                let exited_at = *root_exited_since.get_or_insert_with(Instant::now);
                if exited_at.elapsed() >= POST_EXIT_DESCENDANT_GRACE {
                    // Root child exited and all buffered bytes were read, yet a pipe write
                    // end remains open—a lingering descendant process is holding it open.
                    return Err(CodexError::JobContainmentFailed);
                }
            }

            if !progressed {
                thread::sleep(POLL_INTERVAL);
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if !child.wait_root_until(remaining) {
            return Err(CodexError::TimedOut);
        }

        // Verify zero lingering descendant processes in the Job before accepting exit.
        let grace =
            POST_EXIT_DESCENDANT_GRACE.min(deadline.saturating_duration_since(Instant::now()));
        let accounting = child
            .wait_natural_job_quiescence(grace)
            .ok_or(CodexError::JobContainmentFailed)?;

        let exit_code = child.exit_code().ok_or(CodexError::SpawnFailed)?;
        let trace = decoder.trace();
        match decoder.finish_stdout() {
            Ok(parsed_turn) => {
                if exit_code != 0 {
                    return Err(CodexError::NonZeroExit);
                }
                Ok((parsed_turn, accounting.TotalProcesses))
            }
            Err(ProtocolError::Incomplete) if exit_code != 0 => Err(CodexError::NonZeroExit),
            Err(protocol_err) => {
                diagnose_trace(protocol_err, &trace);
                Err(CodexError::Protocol(protocol_err))
            }
        }
    })();

    // Close parent read pipes before confirming final Job cleanup.
    drop(stdout_read);
    drop(stderr_read);

    match run_result {
        Ok((turn, total_job_processes)) => {
            let quiescent = child.confirm_quiescent_and_close();
            finish_execution(
                lease,
                quiescent,
                Ok(CodexTurnOutcome {
                    turn,
                    stderr_bytes_drained,
                    total_job_processes,
                    owned_workspace_removed: false,
                }),
            )
        }
        Err(err) => {
            let quiescent = child.terminate_and_wait_quiescent();
            finish_execution(lease, quiescent, Err(err))
        }
    }
}

struct WinHandle(HANDLE);

impl WinHandle {
    fn is_valid(h: HANDLE) -> bool {
        !h.is_null() && h != INVALID_HANDLE_VALUE
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        if Self::is_valid(self.0) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
            self.0 = null_mut();
        }
    }
}

struct ProcThreadAttrList {
    buf: Vec<u8>,
}

impl ProcThreadAttrList {
    fn new_with_handles(handles: &mut [HANDLE]) -> Result<Self, CodexError> {
        let mut size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(CodexError::SpawnFailed);
        }
        let mut buf = vec![0u8; size];
        let list_ptr = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let init_ok = unsafe { InitializeProcThreadAttributeList(list_ptr, 1, 0, &mut size) };
        if init_ok == 0 {
            return Err(CodexError::SpawnFailed);
        }
        let update_ok = unsafe {
            UpdateProcThreadAttribute(
                list_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr() as *const c_void,
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        };
        if update_ok == 0 {
            unsafe {
                DeleteProcThreadAttributeList(list_ptr);
            }
            return Err(CodexError::SpawnFailed);
        }
        Ok(Self { buf })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcThreadAttrList {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            unsafe {
                DeleteProcThreadAttributeList(self.buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST);
            }
        }
    }
}

struct SpawnedContainedProcess {
    job: Option<WinHandle>,
    process: Option<WinHandle>,
    quiescent: bool,
}

impl SpawnedContainedProcess {
    fn is_root_signaled(&self) -> bool {
        let Some(ref proc_h) = self.process else {
            return true;
        };
        unsafe { WaitForSingleObject(proc_h.0, 0) == WAIT_OBJECT_0 }
    }

    fn wait_root_until(&self, timeout: Duration) -> bool {
        let Some(ref proc_h) = self.process else {
            return true;
        };
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        unsafe { WaitForSingleObject(proc_h.0, ms) == WAIT_OBJECT_0 }
    }

    fn exit_code(&self) -> Option<u32> {
        let proc_h = self.process.as_ref()?;
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(proc_h.0, &mut code) };
        if ok == 0 || code == STILL_ACTIVE as u32 {
            return None;
        }
        Some(code)
    }

    fn wait_natural_job_quiescence(
        &mut self,
        grace: Duration,
    ) -> Option<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
        let job_h = self.job.as_ref()?.0;
        let deadline = Instant::now() + grace;
        loop {
            let info = query_job_accounting(job_h)?;
            if info.ActiveProcesses == 0 && info.TotalProcesses >= 1 {
                self.quiescent = true;
                return Some(info);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn confirm_quiescent_and_close(&mut self) -> bool {
        let ok = self.quiescent && self.is_root_signaled();
        self.process = None;
        self.job = None;
        ok
    }

    /// Forced teardown proof (C29, C25 root cause): a Job's ActiveProcesses can
    /// reach 0 up to milliseconds BEFORE a member's process object is signaled.
    /// So: freeze the Job (no new members), take verified SYNCHRONIZE handles to
    /// every current member, terminate, then require root exit, zero active
    /// members AND every tracked handle signaled. Any gap fails closed.
    fn terminate_and_wait_quiescent(&mut self) -> bool {
        let tracked = self
            .job
            .as_ref()
            .map(|job_h| freeze_and_track_job_members(job_h.0));
        if let Some(ref job_h) = self.job {
            unsafe {
                let _ = TerminateJobObject(job_h.0, 1);
            }
        }
        if let Some(ref proc_h) = self.process {
            unsafe {
                let _ = TerminateProcess(proc_h.0, 1);
            }
        }
        let root_done = self.wait_root_until(TERMINATE_WAIT_TIMEOUT);
        let mut job_done = false;
        if let Some(ref job_h) = self.job {
            let deadline = Instant::now() + TERMINATE_WAIT_TIMEOUT;
            // Until the job is empty, the deadline passes, or its accounting can no longer be read.
            while let Some(info) = query_job_accounting(job_h.0) {
                if info.ActiveProcesses == 0 {
                    job_done = true;
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(POLL_INTERVAL);
            }
        }
        let members_done = match (&self.job, tracked) {
            (None, _) => true,
            (Some(_), Some(Some(handles))) => {
                let deadline = Instant::now() + TERMINATE_WAIT_TIMEOUT;
                handles.iter().all(|h| {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let ms = left.as_millis().min(u32::MAX as u128) as u32;
                    unsafe { WaitForSingleObject(h.0, ms) == WAIT_OBJECT_0 }
                })
            }
            _ => false,
        };
        self.quiescent = root_done && job_done && members_done;
        self.process = None;
        self.job = None;
        self.quiescent
    }
}

impl Drop for SpawnedContainedProcess {
    fn drop(&mut self) {
        if !self.quiescent && (self.job.is_some() || self.process.is_some()) {
            let _ = self.terminate_and_wait_quiescent();
        }
    }
}

fn create_kill_on_close_job() -> Result<WinHandle, CodexError> {
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if !WinHandle::is_valid(raw) {
        return Err(CodexError::JobContainmentFailed);
    }
    let job = WinHandle(raw);
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(CodexError::JobContainmentFailed);
    }
    Ok(job)
}

/// Freeze membership (active-process limit 1 refuses any new member while the
/// existing ones keep running) and open SYNCHRONIZE handles to every member,
/// each verified to still be in THIS Job (guards PID reuse). Members whose PID
/// can no longer be opened have no process object left, so they are gone.
/// Returns None (fail closed) if freezing, listing or verification fails.
fn freeze_and_track_job_members(job: HANDLE) -> Option<Vec<WinHandle>> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as *mut c_void,
            size,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size,
        )
    };
    if ok == 0 {
        return None;
    }
    // Header (two u32) occupies the first usize slot on x64, then the ID array.
    let mut buffer = vec![0usize; 1 + MAX_TRACKED_JOB_PROCESSES];
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buffer.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(buffer.as_slice()) as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    let header = buffer[0] as u64;
    let assigned = (header & 0xffff_ffff) as usize;
    let listed = (header >> 32) as usize;
    if listed != assigned || listed > MAX_TRACKED_JOB_PROCESSES {
        return None;
    }
    let mut handles = Vec::with_capacity(listed);
    for &pid in &buffer[1..=listed] {
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid as u32,
            )
        };
        if !WinHandle::is_valid(raw) {
            // Only "no such process object" proves the member is gone; any other
            // failure (e.g. a descendant changed its DACL) fails closed.
            if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                continue;
            }
            return None;
        }
        let handle = WinHandle(raw);
        let mut member = 0;
        if unsafe { IsProcessInJob(handle.0, job, &mut member) } == 0 {
            return None;
        }
        if member != 0 {
            handles.push(handle);
        } // else: PID reused by an unrelated process; ours is already gone
    }
    Some(handles)
}

fn query_job_accounting(job: HANDLE) -> Option<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(info)
    }
}

fn create_pipe_pair(
    child_inherits_read: bool,
    buffer_bytes: u32,
) -> Result<(WinHandle, WinHandle), CodexError> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let mut read_h: HANDLE = null_mut();
    let mut write_h: HANDLE = null_mut();
    let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, &sa, buffer_bytes) };
    if ok == 0 || !WinHandle::is_valid(read_h) || !WinHandle::is_valid(write_h) {
        return Err(CodexError::SpawnFailed);
    }
    let read_wrap = WinHandle(read_h);
    let write_wrap = WinHandle(write_h);
    let parent_handle = if child_inherits_read {
        write_wrap.0
    } else {
        read_wrap.0
    };
    let set_ok = unsafe { SetHandleInformation(parent_handle, HANDLE_FLAG_INHERIT, 0) };
    if set_ok == 0 {
        return Err(CodexError::SpawnFailed);
    }
    Ok((read_wrap, write_wrap))
}

fn write_all_handle(handle: HANDLE, mut data: &[u8]) -> Result<(), CodexError> {
    while !data.is_empty() {
        let chunk = data.len().min(STDIN_PIPE_BYTES as usize) as u32;
        let mut written = 0u32;
        let ok = unsafe { WriteFile(handle, data.as_ptr(), chunk, &mut written, null_mut()) };
        if ok == 0 || written == 0 {
            return Err(CodexError::StdinWriteFailed);
        }
        data = &data[written as usize..];
    }
    Ok(())
}

struct SpawnFailure {
    error: CodexError,
    tree_quiescent: bool,
}

// Used only by fallible setup BEFORE a child exists. After creation, failures
// must go through SpawnedContainedProcess::fail_startup, never this conversion.
impl From<CodexError> for SpawnFailure {
    fn from(error: CodexError) -> Self {
        Self {
            error,
            tree_quiescent: true,
        }
    }
}

impl SpawnedContainedProcess {
    fn fail_startup(&mut self, error: CodexError) -> SpawnFailure {
        SpawnFailure {
            error,
            tree_quiescent: self.terminate_and_wait_quiescent(),
        }
    }
}

fn check_startup_control(cancel: &CancelToken, deadline: Instant) -> Result<(), CodexError> {
    if cancel.is_cancelled() {
        Err(CodexError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(CodexError::TimedOut)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupPhase {
    Verified,
    Created,
    Assigned,
    InputReady,
}

type SpawnResult = Result<(SpawnedContainedProcess, WinHandle, WinHandle), SpawnFailure>;

fn spawn_contained(
    spec: &CodexCommandSpec,
    cancel: &CancelToken,
    deadline: Instant,
) -> SpawnResult {
    spawn_contained_inner(spec, cancel, deadline, &mut |_, _| Ok(()))
}

// The private callback supports deterministic unit faults; no public/API/env
// startup fault controls or alternate production executable selection are added.
fn spawn_contained_inner(
    spec: &CodexCommandSpec,
    cancel: &CancelToken,
    deadline: Instant,
    checkpoint: &mut impl FnMut(StartupPhase, Option<HANDLE>) -> Result<(), CodexError>,
) -> SpawnResult {
    use std::os::windows::ffi::OsStrExt;

    check_startup_control(cancel, deadline)?;
    // 1. Verify pinned executable while holding a FILE_SHARE_READ lock across CreateProcessW.
    let verified_exe = spec.pin().verify_and_lock()?;
    checkpoint(StartupPhase::Verified, None)?;
    check_startup_control(cancel, deadline)?;

    let app_wide: Vec<u16> = verified_exe
        .canonical_path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut cmd_wide = encode_windows_command_line(verified_exe.canonical_path(), spec.args())?;
    let env_wide = encode_windows_environment_block(spec.env_vars())?;
    let cwd_wide: Vec<u16> = spec
        .workspace_dir()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    // 2. Create owned kill-on-close Job object before child creation.
    let job = create_kill_on_close_job()?;

    // 3. Create stdin/stdout/stderr pipes and restrict inheritance to the 3 child ends.
    let (stdin_read, stdin_write) = create_pipe_pair(true, STDIN_PIPE_BYTES)?;
    let (stdout_read, stdout_write) = create_pipe_pair(false, PIPE_BUFFER_BYTES)?;
    let (stderr_read, stderr_write) = create_pipe_pair(false, PIPE_BUFFER_BYTES)?;

    let mut inherited_handles = [stdin_read.0, stdout_write.0, stderr_write.0];
    let mut attr_list = ProcThreadAttrList::new_with_handles(&mut inherited_handles)?;

    let mut startup_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
    startup_ex.StartupInfo.wShowWindow = SW_HIDE_U16;
    startup_ex.StartupInfo.hStdInput = stdin_read.0;
    startup_ex.StartupInfo.hStdOutput = stdout_write.0;
    startup_ex.StartupInfo.hStdError = stderr_write.0;
    startup_ex.lpAttributeList = attr_list.as_mut_ptr();

    let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let flags = CREATE_SUSPENDED
        | CREATE_NO_WINDOW
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT;

    // Cancellation/deadline can change while preparing handles/argv as well.
    check_startup_control(cancel, deadline)?;
    let created = unsafe {
        CreateProcessW(
            app_wide.as_ptr(),
            cmd_wide.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            env_wide.as_ptr() as *const c_void,
            cwd_wide.as_ptr(),
            &startup_ex.StartupInfo,
            &mut proc_info,
        )
    };
    if created == 0 {
        return Err(CodexError::SpawnFailed.into());
    }

    let child_process = WinHandle(proc_info.hProcess);
    let child_thread = WinHandle(proc_info.hThread);

    // Image section is mapped; release read lock and close child pipe ends in parent.
    drop(verified_exe);
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);
    drop(attr_list);

    let mut contained = SpawnedContainedProcess {
        job: Some(job),
        process: Some(child_process),
        quiescent: false,
    };

    if !WinHandle::is_valid(proc_info.hProcess) || !WinHandle::is_valid(proc_info.hThread) {
        return Err(contained.fail_startup(CodexError::SpawnFailed));
    }
    if let Err(error) = checkpoint(StartupPhase::Created, Some(proc_info.hProcess)) {
        return Err(contained.fail_startup(error));
    }

    // 4. Assign suspended child to the kill-on-close Job BEFORE resuming execution.
    let assigned = unsafe {
        AssignProcessToJobObject(
            contained.job.as_ref().unwrap().0,
            contained.process.as_ref().unwrap().0,
        )
    };
    if assigned == 0 {
        return Err(contained.fail_startup(CodexError::JobContainmentFailed));
    }
    let Some(accounting) = query_job_accounting(contained.job.as_ref().unwrap().0) else {
        return Err(contained.fail_startup(CodexError::JobContainmentFailed));
    };
    if accounting.ActiveProcesses != 1 || accounting.TotalProcesses != 1 {
        return Err(contained.fail_startup(CodexError::JobContainmentFailed));
    }
    if let Err(error) = checkpoint(StartupPhase::Assigned, Some(proc_info.hProcess)) {
        return Err(contained.fail_startup(error));
    }

    // 5. Write bounded prompt to child stdin pipe and immediately close parent write handle.
    // It must fit the pipe buffer entirely: nothing reads until the child is resumed (step 6).
    if spec.stdin_prompt().len() > STDIN_PIPE_BYTES as usize {
        drop(stdin_write);
        return Err(contained.fail_startup(CodexError::InvalidConfig));
    }
    if let Err(err) = write_all_handle(stdin_write.0, spec.stdin_prompt()) {
        drop(stdin_write);
        return Err(contained.fail_startup(err));
    }
    drop(stdin_write);
    if let Err(error) = checkpoint(StartupPhase::InputReady, Some(proc_info.hProcess))
        .and_then(|_| check_startup_control(cancel, deadline))
    {
        return Err(contained.fail_startup(error));
    }

    // 6. Resume the main thread now that Job containment and stdin EOF are armed.
    let prev_suspend = unsafe { ResumeThread(child_thread.0) };
    if prev_suspend != 1 {
        return Err(contained.fail_startup(CodexError::JobContainmentFailed));
    }
    drop(child_thread);

    Ok((contained, stdout_read, stderr_read))
}

#[cfg(test)]
mod tests;

enum PipePoll {
    Bytes(usize),
    Empty,
    Eof,
    Error,
}

fn peek_and_read_pipe(pipe: HANDLE, scratch: &mut [u8]) -> PipePoll {
    let mut avail = 0u32;
    let peek_ok = unsafe { PeekNamedPipe(pipe, null_mut(), 0, null_mut(), &mut avail, null_mut()) };
    if peek_ok == 0 {
        let err = unsafe { GetLastError() };
        return if err == ERROR_BROKEN_PIPE {
            PipePoll::Eof
        } else {
            PipePoll::Error
        };
    }
    if avail == 0 {
        return PipePoll::Empty;
    }
    let to_read = (avail as usize).min(scratch.len()) as u32;
    let mut bytes_read = 0u32;
    let read_ok = unsafe {
        ReadFile(
            pipe,
            scratch.as_mut_ptr(),
            to_read,
            &mut bytes_read,
            null_mut(),
        )
    };
    if read_ok == 0 {
        let err = unsafe { GetLastError() };
        return if err == ERROR_BROKEN_PIPE {
            PipePoll::Eof
        } else {
            PipePoll::Error
        };
    }
    if bytes_read == 0 {
        PipePoll::Eof
    } else {
        PipePoll::Bytes(bytes_read as usize)
    }
}
