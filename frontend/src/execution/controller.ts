import type * as Wire from '../execution-v2/generated.js';
import { limits } from '../execution-v2/generated.js';
import {
  createExecutionClient, type RunProgress, ExecutionClientError, isCanonicalId, isExecutionChanged,
  sanitizeExecutionError, utf8Bytes
} from './client.js';
import {
  acceptSummary, applyPage, beginCatchUp, beginEarlier, beginReconcile, beginTail,
  createTranscript, markDirty, nextLoad
} from './reconcile.js';
import type { TranscriptState } from './reconcile.js';
import type {
  Unlisten, WorkspaceController, WorkspaceControllerState, WorkspaceDependencies
} from './workspace-ports.js';

function copy<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function failure(code: string, user_message: string): ExecutionClientError {
  return new ExecutionClientError({ code, user_message, recoverable: true });
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') {
    const object = value as Record<string, unknown>;
    return '{' + Object.keys(object).sort().map(key => JSON.stringify(key) + ':' + canonical(object[key])).join(',') + '}';
  }
  return JSON.stringify(value) ?? 'undefined';
}

type Attempt =
  | { key: string; command: 'create_session'; request: Wire.CreateSessionRequest; pending: boolean }
  | { key: string; command: 'start_fake_run'; request: Wire.StartFakeRunRequest; pending: boolean }
  | { key: string; command: 'start_codex_run'; request: Wire.StartCodexRunRequest; pending: boolean }
  | { key: string; command: 'cancel_run'; request: Wire.CancelRunRequest; pending: boolean };

type Submission =
  | { command: 'create_session'; label: string; provider_id: Wire.ProviderId }
  | { command: 'start_fake_run'; session_id: string; prompt: string; scenario: Wire.FakeScenario }
  | { command: 'start_codex_run'; session_id: string; prompt: string; confirmed: boolean; context_run_ids?: readonly string[] }
  | { command: 'cancel_run'; run_id: string };

// Only definite pre-write/domain rejections release a retry DTO. Unknown codes,
// storage failures and invalid replies are deliberately treated as uncertain.
const definiteRejections = new Set([
  'validation_failed', 'provider_execution_disabled', 'request_id_conflict',
  'workspace_not_found', 'session_not_found', 'run_not_found', 'state_conflict',
  'capacity_exceeded', 'queue_full', 'context_unavailable'
]);

export function createWorkspaceController(deps: WorkspaceDependencies): WorkspaceController {
  // Reuse the frozen client validators even when the injected client is a test
  // double. Copies on both sides isolate private DTOs from injected consumers.
  const client = createExecutionClient(async (command, args) => {
    const request = copy(args.request);
    // The wire client names native v2 commands `*_v2`; the injected client port
    // keeps logical method names.
    switch (command.replace(/_v2$/, '')) {
      // The client port already unwrapped the v2 bootstrap envelope to its
      // snapshot; re-wrap it so the shared validator checks the same shape.
      case 'bootstrap_workspace': return { contract_version: 2, storage_state: 'ready', upgrade_id: null,
        snapshot: copy(await deps.client.bootstrap_workspace(request as Wire.BootstrapRequest)) };
      case 'get_workspace_snapshot': return copy(await deps.client.get_workspace_snapshot(request as Wire.WorkspaceRequest));
      case 'get_run_progress': {
        if (deps.client.get_run_progress === undefined) throw failure('invalid_execution_response', 'Progress is unavailable.');
        return copy(await deps.client.get_run_progress(request as Wire.WorkspaceRequest));
      }
      case 'list_run_events': return copy(await deps.client.list_run_events(request as Wire.EventPageRequest));
      case 'create_session': return copy(await deps.client.create_session(request as Wire.CreateSessionRequest));
      case 'start_fake_run': return copy(await deps.client.start_fake_run(request as Wire.StartFakeRunRequest));
      case 'start_codex_run': return copy(await deps.client.start_codex_run(request as Wire.StartCodexRunRequest));
      case 'cancel_run': return copy(await deps.client.cancel_run(request as Wire.CancelRunRequest));
      case 'save_layout': return copy(await deps.client.save_layout(request as Wire.SaveLayoutRequest));
      default: throw failure('validation_failed', 'The execution command is invalid.');
    }
  });

  let active = false;
  let epoch = 0;
  let startupReady = false;
  let initialRead = false;
  let status: WorkspaceControllerState['status'] = 'connecting';
  let snapshot: Wire.WorkspaceSnapshot | null = null;
  let selectedSession: string | null = null;
  const selections = new Map<string, string>();
  let transcript: TranscriptState | null = null;
  let progress: RunProgress | null = null;
  let viewport = {};
  let visible = true;
  let dirty = false;
  let blocked = false;
  let wakeVersion = {};
  let lastError: Wire.Error | null = null;
  let listenerError: Wire.Error | null = null;
  let listenerReady = false;
  let unlisten: Unlisten | null = null;
  let listenerFlight: { epoch: number } | null = null;
  let readFlight: { epoch: number } | null = null;
  let mutationFlight: { epoch: number } | null = null;
  let deferred: { handle: unknown } | null = null;
  let poll: { handle: unknown } | null = null;
  const attempts = new Map<string, Attempt>();

  function alive(mount: number): boolean {
    return active && epoch === mount;
  }

  function getState(): WorkspaceControllerState {
    try {
      const layout = copy(deps.layout.getState());
      const current = snapshot === null ? null : {
        ...snapshot,
        layout: layout.warning === null ? layout.base : null,
        layout_warning: layout.warning
      };
      return copy({
        status,
        snapshot: current,
        selected_session_id: selectedSession,
        selected_run_by_session: Object.fromEntries(selections),
        transcript,
        progress,
        layout,
        attempts: Array.from(attempts.values(), attempt => ({
          key: attempt.key, command: attempt.command,
          request_id: attempt.request.request_id, pending: attempt.pending
        })),
        last_error: lastError,
        dirty,
        in_flight: active && readFlight !== null,
        mutation_pending: active && mutationFlight !== null,
        visible,
        listener_ready: listenerReady
      });
    } catch (error: unknown) {
      throw new ExecutionClientError(sanitizeExecutionError(error));
    }
  }

  function publish(): void {
    // Observer failures cannot corrupt controller progress or become transport
    // errors. Every notification is an independent copy, including nested DTOs.
    if (deps.onChange !== undefined) {
      try { deps.onChange(getState()); } catch { /* Observer isolation. */ }
    }
  }

  function report(error: unknown): ExecutionClientError {
    const safe = new ExecutionClientError(sanitizeExecutionError(error));
    lastError = safe.error;
    publish();
    return safe;
  }

  function synchronous(action: () => void): void {
    try { action(); } catch (error: unknown) { throw report(error); }
  }

  async function asynchronous(action: () => Promise<void>): Promise<void> {
    const mount = epoch;
    try { await action(); } catch (error: unknown) {
      const safe = new ExecutionClientError(sanitizeExecutionError(error));
      if (alive(mount)) {
        lastError = safe.error;
        publish();
      }
      throw safe;
    }
  }

  function background(operation: Promise<void>, mount: number): void {
    void operation.catch((error: unknown) => {
      if (alive(mount)) {
        lastError = sanitizeExecutionError(error);
        publish();
      }
    });
  }

  function cleanUnlisten(cleanup: Unlisten): void {
    try { cleanup(); } catch { /* Teardown must remain idempotent. */ }
  }

  function clearDeferred(): void {
    const timer = deferred;
    deferred = null;
    if (timer !== null) {
      try { deps.timer.clearTimeout(timer.handle); } catch { /* Invalidate by epoch as well. */ }
    }
  }

  function clearPoll(): void {
    const timer = poll;
    poll = null;
    if (timer !== null) {
      try { deps.timer.clearInterval(timer.handle); } catch { /* Invalidate by epoch as well. */ }
    }
  }

  function needsPage(): boolean {
    return transcript !== null && (transcript.pass !== null ||
      (transcript.view === 'live' && (!transcript.loaded || transcript.known_last_seq > transcript.applied_seq)));
  }

  function hasWork(): boolean {
    return dirty || snapshot === null || needsPage();
  }

  function schedule(): void {
    if (!active || !startupReady || blocked || readFlight !== null || deferred !== null ||
        (!visible && !initialRead) || !hasWork()) return;
    const mount = epoch;
    const timer = { handle: undefined as unknown };
    deferred = timer;
    try {
      timer.handle = deps.timer.setTimeout(() => {
        if (!alive(mount) || deferred !== timer) return;
        deferred = null;
        background(pump(mount), mount);
      }, 0);
    } catch (error: unknown) {
      if (deferred === timer) deferred = null;
      blocked = true;
      lastError = sanitizeExecutionError(error);
      publish();
    }
  }

  function wake(refresh: boolean): void {
    if (!active) return;
    if (refresh) dirty = true;
    wakeVersion = {};
    blocked = false;
    schedule();
  }

  async function installListener(mount: number): Promise<void> {
    if (!alive(mount) || listenerReady || listenerFlight !== null) return;
    const token = { epoch: mount };
    listenerFlight = token;
    try {
      const cleanup = await deps.subscribe(payload => {
        if (!alive(mount)) return;
        try {
          if (!isExecutionChanged(payload) ||
              (snapshot !== null && payload.workspace_id !== snapshot.workspace.workspace_id)) return;
          if (transcript !== null) transcript = markDirty(transcript, payload);
          wake(true);
          publish();
        } catch (error: unknown) {
          lastError = sanitizeExecutionError(error);
          publish();
        }
      });
      if (!alive(mount)) {
        cleanUnlisten(cleanup);
        return;
      }
      unlisten = cleanup;
      listenerReady = true;
      if (lastError === listenerError) lastError = null;
      listenerError = null;
      publish();
    } catch (error: unknown) {
      if (alive(mount)) {
        listenerReady = false;
        listenerError = sanitizeExecutionError(error);
        lastError = listenerError;
        publish();
      }
    } finally {
      if (listenerFlight === token) listenerFlight = null;
      // A restart never overlaps a still-pending subscription. Its first read
      // remains gated until a subscription attempt for the new epoch finishes.
      if (active && epoch !== mount && !startupReady) background(startup(epoch), epoch);
    }
  }

  function recover(): void {
    if (!active) return;
    const mount = epoch;
    if (startupReady && visible) background(installListener(mount), mount);
    wake(true);
    publish();
  }

  function armPoll(): void {
    if (!active || !visible || poll !== null) return;
    const mount = epoch;
    const timer = { handle: undefined as unknown };
    poll = timer;
    try {
      timer.handle = deps.timer.setInterval(() => {
        if (!alive(mount) || !visible || poll !== timer) return;
        try { recover(); } catch (error: unknown) {
          if (alive(mount)) {
            lastError = sanitizeExecutionError(error);
            publish();
          }
        }
      }, 2000);
    } catch (error: unknown) {
      if (poll === timer) poll = null;
      throw error;
    }
  }

  function mergeSession(list: readonly Wire.SessionSnapshot[], incoming: Wire.SessionSnapshot): Wire.SessionSnapshot[] {
    const old = list.find(item => item.session_id === incoming.session_id);
    if (old !== undefined) {
      if (canonical(old) !== canonical(incoming)) {
        throw failure('invalid_execution_response', 'The session summary conflicts with retained history.');
      }
      return [...list];
    }
    if (list.length >= limits.workspace_sessions) {
      throw failure('capacity_exceeded', 'The retained session limit has been reached.');
    }
    return [...list, copy(incoming)];
  }

  function mergeRun(list: readonly Wire.RunSnapshot[], incoming: Wire.RunSnapshot): Wire.RunSnapshot[] {
    const index = list.findIndex(item => item.run_id === incoming.run_id);
    if (index >= 0) {
      // Summary identity and equal-sequence conflict rules belong to the frozen
      // reducer. This temporary state has no transcript buffer or retained cache.
      const merged = acceptSummary(createTranscript(list[index]), incoming);
      if (merged.problem !== null) throw new ExecutionClientError(merged.problem);
      const result = [...list];
      result[index] = copy(merged.summary);
      return result;
    }
    if (list.length >= limits.workspace_runs) {
      throw failure('capacity_exceeded', 'The retained run limit has been reached.');
    }
    if (list.some(item => item.enqueue_order === incoming.enqueue_order)) {
      throw failure('invalid_execution_response', 'The run ordering conflicts with retained history.');
    }
    return [...list, copy(incoming)];
  }

  function synchronizeSelection(): void {
    if (snapshot === null) return;
    for (const session of snapshot.sessions) {
      if (!selections.has(session.session_id)) {
        let latest: Wire.RunSnapshot | undefined;
        for (const run of snapshot.runs) {
          if (run.session_id === session.session_id && (latest === undefined || run.enqueue_order > latest.enqueue_order)) latest = run;
        }
        if (latest !== undefined) selections.set(session.session_id, latest.run_id);
      }
    }
    if (selectedSession === null) selectedSession = snapshot.sessions[0]?.session_id ?? null;
    const runId = selectedSession === null ? undefined : selections.get(selectedSession);
    const selected = snapshot.runs.find(run => run.run_id === runId && run.session_id === selectedSession);
    if (selected === undefined) {
      if (transcript !== null) {
        viewport = {};
        transcript = null;
      }
      return;
    }
    if (transcript === null || transcript.summary.run_id !== selected.run_id) {
      viewport = {};
      transcript = beginTail(createTranscript(selected)).state;
    } else {
      transcript = acceptSummary(transcript, selected);
      if (transcript.problem !== null) throw new ExecutionClientError(transcript.problem);
    }
  }

  /** Display-only: a failed or invalid progress read clears it and never affects the run, status or errors. */
  async function refreshProgress(incoming: Wire.WorkspaceSnapshot, mount: number): Promise<void> {
    const running = incoming.runs.some(run => run.provider_id === 'codex' &&
      (run.state === 'starting' || run.state === 'running' || run.state === 'cancel_requested'));
    if (!running) {
      progress = null;
      return;
    }
    try {
      const next = await client.get_run_progress({ contract_version: 2, workspace_id: incoming.workspace.workspace_id });
      if (alive(mount)) progress = next;
    } catch {
      if (alive(mount)) progress = null;
    }
  }

  function observeWorkspace(incoming: Wire.WorkspaceSnapshot): void {
    if (snapshot !== null && incoming.workspace.workspace_id !== snapshot.workspace.workspace_id) {
      throw failure('invalid_execution_response', 'The workspace identity changed unexpectedly.');
    }
    let sessions = snapshot?.sessions ?? [];
    let runs = snapshot?.runs ?? [];
    for (const session of incoming.sessions) sessions = mergeSession(sessions, session);
    for (const run of incoming.runs) runs = mergeRun(runs, run);
    deps.layout.observe(copy(incoming.layout), copy(incoming.layout_warning));
    snapshot = { ...copy(incoming), sessions, runs };
    synchronizeSelection();
    status = incoming.execution_available ? 'ready' : 'unavailable';
  }

  async function pump(mount: number): Promise<void> {
    if (!alive(mount) || !startupReady || readFlight !== null || blocked ||
        (!visible && !initialRead) || !hasWork()) return;
    clearDeferred();
    const token = { epoch: mount };
    const beganAt = wakeVersion;
    readFlight = token;
    const refresh = dirty || snapshot === null || initialRead;
    if (refresh) {
      dirty = false; // Consume before awaiting; later hints must survive.
      initialRead = false;
      if (transcript !== null) transcript = beginReconcile(transcript);
    }
    publish();
    try {
      if (!alive(mount)) return;
      if (refresh) {
        const currentId = snapshot?.workspace.workspace_id;
        const incoming = currentId === undefined
          ? await client.bootstrap_workspace({ contract_version: 2 })
          : await client.get_workspace_snapshot({ contract_version: 2, workspace_id: currentId });
        if (!alive(mount)) return;
        observeWorkspace(incoming);
        await refreshProgress(incoming, mount);
        if (!alive(mount)) return;
        if (listenerReady) lastError = null;
        publish();
      }
      if (!alive(mount) || !visible || transcript === null) return;
      if (transcript.pass === null && transcript.view === 'live' &&
          (!transcript.loaded || transcript.known_last_seq > transcript.applied_seq)) {
        transcript = beginCatchUp(transcript).state;
      }
      const load = nextLoad(transcript);
      if (load === null) return;
      const selectedViewport = viewport;
      publish();
      if (!alive(mount) || !visible) return;
      try {
        const page = await client.list_run_events(copy(load.request));
        if (!alive(mount) || selectedViewport !== viewport || transcript === null ||
            transcript.generation !== load.generation) return;
        transcript = applyPage(transcript, load, page);
        if (transcript.problem !== null) throw new ExecutionClientError(transcript.problem);
        if (transcript.view === 'live' && transcript.known_last_seq > transcript.summary_seq) dirty = true;
        if (listenerReady) lastError = null;
      } catch (error: unknown) {
        if (!alive(mount) || selectedViewport !== viewport || transcript === null ||
            transcript.generation !== load.generation) return;
        throw error;
      }
    } catch (error: unknown) {
      if (alive(mount)) {
        lastError = sanitizeExecutionError(error);
        if (refresh) {
          dirty = true;
          status = 'unavailable';
        }
        // No failed-read loop. A later explicit wake, hint or visible poll may
        // retry; an invalidated viewport cannot consume that later wake.
        if (wakeVersion === beganAt) blocked = true;
      }
    } finally {
      if (readFlight === token) readFlight = null;
      if (alive(mount)) {
        publish();
        schedule(); // At most one page per pump; continuation always yields.
      } else if (active) {
        schedule();
      }
    }
  }

  async function startup(mount: number): Promise<void> {
    if (!alive(mount) || startupReady || listenerFlight !== null) return;
    await installListener(mount);
    if (!alive(mount)) return;
    startupReady = true;
    armPoll();
    await pump(mount);
  }

  function requireWorkspace(): Wire.WorkspaceSnapshot {
    if (!active || !startupReady || snapshot === null) {
      throw failure('state_conflict', 'The workspace is not ready for this action.');
    }
    return snapshot;
  }

  function requireMutationSlot(): Wire.WorkspaceSnapshot {
    const current = requireWorkspace();
    if (mutationFlight !== null) throw failure('state_conflict', 'Another mutation is still pending.');
    return current;
  }

  function validateKey(key: string): void {
    if (typeof key !== 'string' || key.length === 0 || key.length > 128 || Array.from(key).length > 64) {
      throw failure('validation_failed', 'A retry key must contain 1–64 characters.');
    }
  }

  function requestId(): string {
    const id = deps.nextId();
    if (!isCanonicalId(id) || Array.from(attempts.values()).some(attempt => attempt.request.request_id === id)) {
      throw failure('validation_failed', 'The request identity source did not return a fresh canonical UUID v4.');
    }
    return id;
  }

  function buildAttempt(submission: Submission, key: string, workspaceId: string, id: string): Attempt {
    const common = { contract_version: 2 as const, workspace_id: workspaceId, request_id: id };
    switch (submission.command) {
      case 'create_session': return { key, command: submission.command, pending: false, request: Object.freeze({
        ...common, label: submission.label, provider_id: submission.provider_id
      }) };
      case 'start_codex_run':
        // The wire type only admits `confirmed: true`; never fabricate consent.
        if (submission.confirmed !== true) throw failure('validation_failed', 'Confirm sending this prompt to Codex.');
        // Carried earlier turns select the v2 profile; nothing to carry stays on v1.
        if (submission.context_run_ids !== undefined && submission.context_run_ids.length > 0) {
          return { key, command: submission.command, pending: false, request: Object.freeze({
            ...common, session_id: submission.session_id, prompt: submission.prompt,
            profile_id: 'codex_text_v2' as const, confirmed: true as const,
            context_run_ids: Object.freeze([...submission.context_run_ids]) as string[]
          }) };
        }
        return { key, command: submission.command, pending: false, request: Object.freeze({
          ...common, session_id: submission.session_id, prompt: submission.prompt,
          profile_id: 'codex_text_v1' as const, confirmed: true as const
        }) };
      case 'start_fake_run': return { key, command: submission.command, pending: false, request: Object.freeze({
        ...common, session_id: submission.session_id, provider_id: 'fake' as const,
        prompt: submission.prompt, scenario: submission.scenario
      }) };
      case 'cancel_run': return { key, command: submission.command, pending: false, request: Object.freeze({ ...common, run_id: submission.run_id }) };
    }
  }

  function prepare(submission: Submission, key: string): Attempt {
    const current = requireMutationSlot();
    validateKey(key);
    const old = attempts.get(key);
    if (old !== undefined) {
      const candidate = buildAttempt(submission, key, current.workspace.workspace_id, old.request.request_id);
      if (old.command !== candidate.command || canonical(old.request) !== canonical(candidate.request)) {
        throw failure('request_id_conflict', 'This retry key belongs to a different unresolved request.');
      }
      return old;
    }
    if (!current.execution_available) throw failure('state_conflict', 'Simulated execution is currently unavailable.');
    if (attempts.size >= 8) throw failure('capacity_exceeded', 'Resolve an existing mutation attempt before submitting another.');
    // Bound private inputs before retaining them. Full request validation still
    // goes through the existing client rather than a parallel wire validator.
    switch (submission.command) {
      case 'create_session':
        if (typeof submission.label !== 'string' || submission.label.length > 256 ||
            Array.from(submission.label).length < 1 || Array.from(submission.label).length > 128 ||
            /[\u0000-\u001f\u007f-\u009f]/.test(submission.label)) {
          throw failure('validation_failed', 'The session label is invalid.');
        }
        if (submission.provider_id !== 'fake' && submission.provider_id !== 'codex') {
          throw failure('validation_failed', 'Choose a supported agent for this session.');
        }
        break;
      case 'start_codex_run':
        // Explicit per-run consent; a missing/false confirmation never reaches native.
        if (submission.confirmed !== true) {
          throw failure('validation_failed', 'Confirm sending this prompt to Codex.');
        }
        if (current.codex_availability !== 'ready') {
          throw failure('codex_unavailable', 'Codex is not available in this workbench.');
        }
        if (!isCanonicalId(submission.session_id) || !current.sessions.some(session =>
          session.session_id === submission.session_id && session.provider_id === 'codex')) {
          throw failure('session_not_found', 'Select an existing Codex session.');
        }
        if (typeof submission.prompt !== 'string' || submission.prompt.length === 0 ||
            utf8Bytes(submission.prompt) > limits.codex_prompt_bytes ||
            /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(submission.prompt)) {
          throw failure('validation_failed', 'Codex prompts are 1 to 1024 UTF-8 bytes of text.');
        }
        if (submission.context_run_ids !== undefined) {
          // Carried runs must be earlier succeeded Codex runs of this same session, oldest first.
          const ids = submission.context_run_ids;
          let previous = 0;
          const eligible = Array.isArray(ids) && ids.length <= limits.codex_context_runs &&
            new Set(ids).size === ids.length && ids.every(id => {
              const run = current.runs.find(candidate => candidate.run_id === id);
              const ok = run !== undefined && run.session_id === submission.session_id && run.provider_id === 'codex' &&
                run.state === 'succeeded' && run.enqueue_order > previous;
              previous = run?.enqueue_order ?? previous;
              return ok;
            });
          if (!eligible) throw failure('validation_failed', 'Earlier turns to carry must be succeeded Codex runs of this session.');
        }
        break;
      case 'start_fake_run':
        if (!isCanonicalId(submission.session_id) || !current.sessions.some(session =>
          session.session_id === submission.session_id && session.provider_id === 'fake')) {
          throw failure('session_not_found', 'Select an existing simulated session.');
        }
        if (typeof submission.prompt !== 'string' || submission.prompt.length === 0 ||
            submission.prompt.length > limits.prompt_bytes || utf8Bytes(submission.prompt) > limits.prompt_bytes ||
            /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(submission.prompt) ||
            !['success', 'failure', 'wait_for_cancel'].includes(submission.scenario)) {
          throw failure('validation_failed', 'The simulated run input is invalid.');
        }
        break;
      case 'cancel_run':
        if (!isCanonicalId(submission.run_id) || !current.runs.some(run => run.run_id === submission.run_id)) {
          throw failure('run_not_found', 'Select an existing simulated run.');
        }
        break;
    }
    const attempt = buildAttempt(submission, key, current.workspace.workspace_id, requestId());
    attempts.set(key, attempt);
    return attempt;
  }

  async function executeAttempt(attempt: Attempt): Promise<void> {
    const current = requireMutationSlot();
    if (attempt.request.workspace_id !== current.workspace.workspace_id) {
      throw failure('request_id_conflict', 'The retained request belongs to another workspace.');
    }
    const mount = epoch;
    const token = { epoch: mount };
    mutationFlight = token;
    attempt.pending = true;
    publish();
    try {
      if (!alive(mount)) throw failure('state_conflict', 'The workspace controller has stopped.');
      if (attempt.command === 'create_session') {
        const result = await client.create_session(copy(attempt.request));
        if (!alive(mount)) throw failure('state_conflict', 'The workspace controller has stopped.');
        if (snapshot === null) throw failure('state_conflict', 'The workspace is unavailable.');
        snapshot = { ...snapshot, sessions: mergeSession(snapshot.sessions, result.session) };
      } else {
        const result = attempt.command === 'start_fake_run'
          ? await client.start_fake_run(copy(attempt.request))
          : attempt.command === 'start_codex_run'
            ? await client.start_codex_run(copy(attempt.request))
            : await client.cancel_run(copy(attempt.request));
        if (!alive(mount)) throw failure('state_conflict', 'The workspace controller has stopped.');
        if (snapshot === null || !snapshot.sessions.some(session => session.session_id === result.run.session_id)) {
          throw failure('invalid_execution_response', 'The run response has no retained owning session.');
        }
        snapshot = { ...snapshot, runs: mergeRun(snapshot.runs, result.run) };
      }
      synchronizeSelection();
      attempts.delete(attempt.key);
      lastError = listenerError;
      wake(true);
    } catch (error: unknown) {
      const safe = new ExecutionClientError(sanitizeExecutionError(error));
      if (alive(mount)) {
        if (definiteRejections.has(safe.code)) attempts.delete(attempt.key);
        lastError = safe.error;
        wake(true); // Read recovery only; mutations are never pump work.
      }
      throw safe;
    } finally {
      attempt.pending = false;
      if (mutationFlight === token) mutationFlight = null;
      if (alive(mount)) publish();
    }
  }

  function select(sessionId: string, runId?: string): void {
    if (snapshot === null || !snapshot.sessions.some(session => session.session_id === sessionId)) {
      throw failure('session_not_found', 'Select an existing session.');
    }
    if (runId !== undefined) {
      if (!snapshot.runs.some(run => run.session_id === sessionId && run.run_id === runId)) {
        throw failure('run_not_found', 'Select a run belonging to this session.');
      }
      selections.set(sessionId, runId);
    }
    selectedSession = sessionId;
    synchronizeSelection();
    wake(false);
    publish();
  }

  return {
    getState,
    start: () => asynchronous(async () => {
      if (active) return;
      if (epoch === Number.MAX_SAFE_INTEGER) throw failure('state_conflict', 'The controller lifecycle limit has been reached.');
      epoch += 1;
      active = true;
      startupReady = false;
      initialRead = true;
      dirty = true;
      blocked = false;
      wakeVersion = {};
      status = 'connecting';
      listenerReady = false;
      lastError = null;
      listenerError = null;
      const mount = epoch;
      publish();
      await startup(mount);
    }),
    stop: () => {
      if (!active && status === 'stopped') return;
      active = false; // Invalidates callbacks even if a cleanup port throws.
      startupReady = false;
      initialRead = false;
      status = 'stopped';
      dirty = false;
      blocked = true;
      clearDeferred();
      clearPoll();
      const cleanup = unlisten;
      unlisten = null;
      listenerReady = false;
      if (cleanup !== null) cleanUnlisten(cleanup);
      viewport = {};
      transcript = null;
      publish();
    },
    reconcile: () => synchronous(() => { wake(true); publish(); }),
    setVisibility: value => synchronous(() => {
      if (typeof value !== 'boolean') throw failure('validation_failed', 'Visibility must be a boolean.');
      if (visible === value) return;
      visible = value;
      if (!visible) {
        clearPoll();
        clearDeferred();
      } else if (active) {
        armPoll();
        recover();
      }
      publish();
    }),
    onFocus: () => synchronous(() => { recover(); }),
    selectSession: sessionId => synchronous(() => { select(sessionId); }),
    selectRun: (sessionId, runId) => synchronous(() => { select(sessionId, runId); }),
    loadEarlier: () => synchronous(() => {
      if (transcript === null) return;
      const earlier = beginEarlier(transcript);
      if (earlier === null) return;
      transcript = earlier.state;
      wake(false);
      publish();
    }),
    returnToLive: () => synchronous(() => {
      if (transcript === null) return;
      transcript = beginTail(transcript).state;
      wake(false);
      publish();
    }),
    createSession: (label, retryKey = 'create_session', providerId = 'fake') => asynchronous(async () => {
      await executeAttempt(prepare({ command: 'create_session', label, provider_id: providerId }, retryKey));
    }),
    startCodexRun: (sessionId, prompt, confirmed, retryKey = 'start_codex_run', contextRunIds) => asynchronous(async () => {
      await executeAttempt(prepare({ command: 'start_codex_run', session_id: sessionId, prompt, confirmed, context_run_ids: contextRunIds }, retryKey));
    }),
    startFakeRun: (sessionId, prompt, scenario, retryKey = 'start_fake_run') => asynchronous(async () => {
      await executeAttempt(prepare({ command: 'start_fake_run', session_id: sessionId, prompt, scenario }, retryKey));
    }),
    cancelRun: (runId, retryKey = 'cancel_run') => asynchronous(async () => {
      await executeAttempt(prepare({ command: 'cancel_run', run_id: runId }, retryKey));
    }),
    retryMutation: key => asynchronous(async () => {
      requireMutationSlot();
      validateKey(key);
      const attempt = attempts.get(key);
      if (attempt === undefined) throw failure('state_conflict', 'There is no unresolved mutation with this retry key.');
      await executeAttempt(attempt);
    }),
    editLayout: draft => synchronous(() => {
      const current = requireWorkspace();
      if (draft.workspace_id !== current.workspace.workspace_id) throw failure('layout_invalid', 'The layout belongs to another workspace.');
      deps.layout.edit(copy(draft));
      publish();
    }),
    saveLayout: () => asynchronous(async () => {
      const current = requireMutationSlot();
      const mount = epoch;
      const token = { epoch: mount };
      mutationFlight = token;
      try {
        const request = copy(deps.layout.beginSave());
        if (request.workspace_id !== current.workspace.workspace_id) throw failure('layout_invalid', 'The layout belongs to another workspace.');
        publish();
        if (!alive(mount)) throw failure('state_conflict', 'The workspace controller has stopped.');
        const result = await client.save_layout(copy(request));
        if (!alive(mount)) throw failure('state_conflict', 'The workspace controller has stopped.');
        deps.layout.acknowledge(copy(result));
        lastError = listenerError;
        wake(true);
      } catch (error: unknown) {
        const safe = new ExecutionClientError(sanitizeExecutionError(error));
        if (alive(mount)) {
          try { deps.layout.reject(copy(safe.error)); } catch (rejection: unknown) {
            lastError = sanitizeExecutionError(rejection);
            throw new ExecutionClientError(lastError);
          }
          lastError = safe.error;
          wake(true);
        }
        throw safe;
      } finally {
        if (mutationFlight === token) mutationFlight = null;
        if (alive(mount)) publish();
      }
    }),
    reapplyLayout: () => synchronous(() => {
      requireWorkspace();
      deps.layout.reapply();
      publish();
    })
  };
}
