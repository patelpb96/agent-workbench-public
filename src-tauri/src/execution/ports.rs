//! Exact native-only store boundary. All methods run on one serialized writer,
//! off the UI thread. Implementations must not launch processes or emit UI events.
use super::*;

pub type StoreResult<T> = Result<T, Error>;

/// Immutable original result on replay; replay never returns events to dispatch.
/// Seeds are not request arguments: ignore fresh IDs/timestamps on receipt replay.
#[derive(Debug, Clone)]
pub struct MutationCommit<T> {
    pub result: T,
    pub committed_events: Vec<RunEvent>,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSeed {
    pub workspace_id: Id,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct SessionSeed {
    pub session_id: Id,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct RunSeed {
    pub run_id: Id,
    pub event_id: Id,
    pub created_at: Timestamp,
}

/// Compare-and-append. The store validates identity, sequence, transitions, budgets,
/// and derived-snapshot parity inside the same transaction as the write.
#[derive(Debug, Clone)]
pub struct AppendBatch {
    pub workspace_id: Id,
    pub run_id: Id,
    pub expected_last_seq: Sequence,
    pub expected_state: RunState,
    pub events: Vec<RunEvent>,
    pub next_snapshot: RunSnapshot,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub interrupted_runs: Vec<RunSnapshot>,
    pub committed_events: Vec<RunEvent>,
}

pub trait IdSource: Send {
    fn next_id(&mut self) -> Id;
}

pub trait Clock: Send {
    fn now_utc(&self) -> Timestamp;
}

/// Native coordinator seam. Integration owns the bounded thread/mailboxes and
/// Tauri wiring; runtime owns a step-driven engine. tick advances at most one
/// fake step, never sleeps/launches/does network I/O. A single invalidation flag
/// is taken only after durable commits, not a growing notification/event queue.
pub trait ExecutionRuntime: Send {
    fn bootstrap_workspace(&mut self, request: &BootstrapRequest)
        -> StoreResult<WorkspaceSnapshot>;
    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
    ) -> StoreResult<CreateSessionResponse>;
    fn start_fake_run(
        &mut self,
        request: &StartFakeRunRequest,
    ) -> StoreResult<StartFakeRunResponse>;
    fn cancel_run(&mut self, request: &CancelRunRequest) -> StoreResult<CancelRunResponse>;
    fn get_workspace_snapshot(
        &mut self,
        request: &WorkspaceRequest,
    ) -> StoreResult<WorkspaceSnapshot>;
    fn list_run_events(&mut self, request: &EventPageRequest) -> StoreResult<EventPage>;
    fn save_layout(&mut self, request: &SaveLayoutRequest) -> StoreResult<SaveLayoutResponse>;
    fn tick(&mut self) -> StoreResult<()>;
    fn take_changed(&mut self) -> Option<ExecutionChanged>;
    fn health(&self) -> ExecutionHealth;
}

/// Factory/open must acquire exclusive process ownership and validate/migrate the
/// schema transactionally. No mutations/dispatch until startup_recover succeeds.
/// SQLite is authoritative for layouts AND receipts; JSON is export-only in M1.
pub trait JournalStore: Send {
    /// Create or load the one app-managed simulated workspace. No browser paths.
    /// This is the sole pre-recovery initialization operation on an empty store.
    fn bootstrap(&mut self, seed: WorkspaceSeed) -> StoreResult<WorkspaceSnapshot>;

    /// Interrupt all previously nonterminal runs atomically and idempotently;
    /// then open admission. A repeated call in the same live runtime MUST NOT
    /// interrupt new work: return an empty report, including no committed events.
    fn startup_recover(&mut self, now: &str, ids: &mut dyn IdSource)
        -> StoreResult<RecoveryReport>;

    fn create_session(
        &mut self,
        request: &CreateSessionRequest,
        seed: SessionSeed,
    ) -> StoreResult<MutationCommit<SessionSnapshot>>;

    /// Atomically receipt + queued event/snapshot + private prompt/scenario.
    /// Assign unique increasing enqueue_order in SQL; never order by timestamps.
    fn enqueue_run(
        &mut self,
        request: &StartFakeRunRequest,
        seed: RunSeed,
    ) -> StoreResult<MutationCommit<RunSnapshot>>;

    /// Lookup/compare durable receipt BEFORE optimistic preconditions. None is
    /// valid only for already-terminal/cancel_requested runs; no event is added.
    /// Otherwise exactly one state-change event is allowed: queued -> cancelled,
    /// or an executing state -> cancel_requested; target IDs must match request.
    /// At ordinary receipt capacity, only that first state-changing cancellation
    /// can spend the 33-slot emergency receipt reserve, never a no-op/replay.
    fn cancel_run(
        &mut self,
        request: &CancelRunRequest,
        mutation: Option<AppendBatch>,
    ) -> StoreResult<MutationCommit<RunSnapshot>>;

    /// Internal producer append has no request receipt; replayed is always false.
    fn append_batch(&mut self, mutation: AppendBatch) -> StoreResult<MutationCommit<RunSnapshot>>;

    fn workspace_snapshot(&mut self, request: &WorkspaceRequest) -> StoreResult<WorkspaceSnapshot>;
    fn get_run(&mut self, workspace_id: &str, run_id: &str) -> StoreResult<QueuedRun>;
    fn next_queued(&mut self, workspace_id: &str) -> StoreResult<Option<QueuedRun>>;
    fn list_run_events(&mut self, request: &EventPageRequest) -> StoreResult<EventPage>;

    /// Validate session ownership and expected_revision, then commit layout,
    /// new revision and immutable receipt together. No authoritative JSON file.
    fn save_layout(&mut self, request: &SaveLayoutRequest) -> StoreResult<MutationCommit<Layout>>;
}

/// Health is also owned by the native coordinator OUTSIDE the journal. On any
/// write failure it stops producers/dispatch and rejects later mutations. It must
/// not invent terminal success when no durable event could commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionHealth {
    Starting,
    Ready,
    StorageUnavailable,
}
