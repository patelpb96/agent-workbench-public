//! Codex provider for the supervisor. Production uses the pinned executable and
//! the fixed `codex_text_v1` spec; a native/test-only constructor injects a spec
//! builder (e.g. a compiled fixture child). No browser-selected executable, env
//! toggle or fake-live switch exists.
use super::{Cancel, Progress, ProviderFactory, ReportedUsage, WorkerOutcome, WorkerReport};
use crate::codex::{
    run_turn_with_progress, CancelToken, CodexCommandSpec, CodexDriverConfig, CodexError,
    CodexLease, Milestone, DEFAULT_TIMEOUT,
};
use crate::execution_v2::CodexProfileId;
use std::{sync::Arc, time::Duration};

type SpecBuilder = dyn Fn(&CodexLease, &str) -> Result<CodexCommandSpec, CodexError> + Send + Sync;

pub struct CodexFactory {
    /// `codex_text_v1`: the prompt only (<= 1024 B).
    spec: Arc<SpecBuilder>,
    /// `codex_text_v2`: the prompt plus carried same-session turns (<= 8192 B).
    spec_v2: Arc<SpecBuilder>,
    timeout: Duration,
}

impl CodexFactory {
    /// Production: pinned Codex CLI, fixed profile, 45 s deadline.
    pub fn pinned() -> Self {
        Self {
            spec: Arc::new(CodexCommandSpec::codex_text_v1),
            spec_v2: Arc::new(CodexCommandSpec::codex_text_v2),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Native/test-only injection of a verified child spec builder.
    pub fn with_spec_builder(
        spec: impl Fn(&CodexLease, &str) -> Result<CodexCommandSpec, CodexError> + Send + Sync + 'static,
        timeout: Duration,
    ) -> Self {
        let spec: Arc<SpecBuilder> = Arc::new(spec);
        Self {
            spec_v2: Arc::clone(&spec),
            spec,
            timeout,
        }
    }
}

impl ProviderFactory for CodexFactory {
    fn run(&self, prompt: &str, cancel: &Cancel, started: &mut dyn FnMut()) -> WorkerReport {
        self.run_reporting(prompt, cancel, started, &mut |_| {})
    }

    fn run_reporting(
        &self,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        self.run_with(&self.spec, prompt, cancel, started, progress)
    }

    fn run_profiled(
        &self,
        profile: CodexProfileId,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        let spec = match profile {
            CodexProfileId::CodexTextV1 => &self.spec,
            CodexProfileId::CodexTextV2 => &self.spec_v2,
        };
        self.run_with(spec, prompt, cancel, started, progress)
    }
}

impl CodexFactory {
    fn run_with(
        &self,
        spec: &Arc<SpecBuilder>,
        prompt: &str,
        cancel: &Cancel,
        started: &mut dyn FnMut(),
        progress: &mut dyn FnMut(Progress),
    ) -> WorkerReport {
        let mut lease = match CodexLease::create() {
            Ok(lease) => lease,
            // No lease exists, so nothing is owned and nothing ran.
            Err(error) => {
                return WorkerReport {
                    outcome: WorkerOutcome::Failed { code: error.code() },
                    teardown_proven: true,
                }
            }
        };
        let spec = match spec(&lease, prompt) {
            Ok(spec) => spec,
            Err(error) => {
                // No child was created; remove only this verified owned lease.
                let removed = lease.cleanup(true).is_ok() && lease.is_cleaned();
                return WorkerReport {
                    outcome: WorkerOutcome::Failed { code: error.code() },
                    teardown_proven: removed,
                };
            }
        };
        // Bridge the provider-agnostic cancel into the driver's token.
        let token = CancelToken::new();
        let watcher_token = token.clone();
        let watcher_cancel = cancel.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher_done = done.clone();
        let watcher = std::thread::Builder::new()
            .name("provider-cancel-bridge".into())
            .spawn(move || {
                while !watcher_done.load(std::sync::atomic::Ordering::SeqCst) {
                    if watcher_cancel.is_signalled() {
                        watcher_token.cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        if watcher.is_err() {
            let removed = lease.cleanup(true).is_ok() && lease.is_cleaned();
            return WorkerReport {
                outcome: WorkerOutcome::Failed {
                    code: CodexError::SpawnFailed.code(),
                },
                teardown_proven: removed,
            };
        }
        let report = run_turn_with_progress(
            spec,
            &mut lease,
            CodexDriverConfig {
                timeout: self.timeout,
                cancel: token,
            },
            started,
            &mut |milestone| {
                progress(match milestone {
                    Milestone::ThreadStarted => Progress::ThreadStarted,
                    Milestone::TurnStarted => Progress::TurnStarted,
                    Milestone::ReasoningObserved => Progress::ReasoningObserved,
                    Milestone::MessageComplete => Progress::MessageComplete,
                })
            },
        );
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(handle) = watcher {
            let _ = handle.join();
        }
        let teardown_proven =
            report.teardown.tree_quiescent && report.teardown.owned_workspace_removed;
        let outcome = match report.result {
            Ok(outcome) => {
                let u = outcome.turn.usage;
                WorkerOutcome::Completed {
                    message: outcome.turn.message,
                    usage: ReportedUsage {
                        input_tokens: u.input_tokens,
                        cached_input_tokens: u.cached_input_tokens,
                        output_tokens: u.output_tokens,
                        reasoning_output_tokens: u.reasoning_output_tokens,
                        cache_write_input_tokens: u.cache_write_input_tokens,
                    },
                }
            }
            Err(error) => WorkerOutcome::Failed { code: error.code() },
        };
        WorkerReport {
            outcome,
            teardown_proven,
        }
    }
}
