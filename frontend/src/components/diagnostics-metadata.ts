// UI-only allowlist. Never spread controller state, request DTOs or shell provider
// discovery into diagnostics: keys/messages/paths/payloads are not diagnostic data.
import type { WorkspaceControllerState } from '../execution/workspace-ports.js';
import type { RuntimeInfo } from '../native.js';
import { runTransitions } from '../execution-v2/generated.js';

const id = (value: string) => /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value) ? value : 'unrecognized_id';
const code = (value: string | undefined) => value === undefined ? null : /^[a-z][a-z0-9_]{0,63}$/.test(value) ? value : 'unrecognized_error';
const shellLabel = (value: string) => /^[A-Za-z0-9 ._+-]{1,64}$/.test(value) ? value : 'unavailable';

export function projectDiagnostics(state: WorkspaceControllerState, runtime?: Pick<RuntimeInfo, 'os' | 'app_version'>) {
  const snapshot = state.snapshot, layout = state.layout, transcript = state.transcript;
  const runs = snapshot?.runs;
  return {
    status: state.status, loaded: snapshot !== null,
    // A retained native flag is not a fresh availability promise after failure.
    execution: snapshot === null ? 'unavailable' : state.status !== 'ready' ? 'unverified' : snapshot.execution_available ? 'available' : 'unavailable',
    listener_ready: state.listener_ready, visible: state.visible, mutation_pending: state.mutation_pending,
    workspace_id: snapshot ? id(snapshot.workspace.workspace_id) : null,
    sessions: snapshot?.sessions.length ?? null, runs: runs?.length ?? null,
    queued: runs?.filter(run => run.state === 'queued').length ?? null,
    active: runs?.filter(run => run.state !== 'queued' && runTransitions[run.state].length > 0).length ?? null,
    terminal: runs?.filter(run => runTransitions[run.state].length === 0).length ?? null,
    layout: {
      saved_revision: layout.base?.revision ?? null, draft_revision: layout.draft?.revision ?? null,
      dirty: layout.dirty, conflicted: layout.conflicted,
      draft_panels: layout.draft?.panels.length ?? null, draft_containers: layout.draft?.containers.length ?? null,
      warning_code: code(layout.warning?.code), problem_code: code(layout.problem?.code),
      pending_save: layout.attempt ? { request_id: id(layout.attempt.request.request_id), expected_revision: layout.attempt.request.expected_revision } : null
    },
    // Eight is the existing frontend attempt bound, not a new wire limit.
    attempts: state.attempts.slice(0, 8).map(attempt => ({ command: attempt.command, request_id: id(attempt.request_id), pending: attempt.pending })),
    history: transcript ? {
      run_id: id(transcript.summary.run_id), loaded: transcript.loaded, view: transcript.view,
      known_last_seq: transcript.known_last_seq, summary_seq: transcript.summary_seq,
      first_seq: transcript.loaded ? transcript.events[0]?.seq ?? null : null,
      last_seq: transcript.loaded ? transcript.events.at(-1)?.seq ?? null : null,
      count: transcript.loaded ? transcript.events.length : 0,
      problem_code: code(transcript.problem?.code)
    } : null,
    last_error_code: code(state.last_error?.code),
    shell: runtime ? { os: shellLabel(runtime.os), app_version: shellLabel(runtime.app_version) } : null
    // Intentionally omit in_flight/dirty read-pump flags: normal polling must not
    // churn visible rows. The caller exposes in_flight only through aria-busy.
  };
}
export type DiagnosticsMetadata = ReturnType<typeof projectDiagnostics>;
