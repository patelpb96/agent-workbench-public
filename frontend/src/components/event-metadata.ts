// UI-only projection, not a new event store or execution/IPC contract.
import type { RunEvent, RunSnapshot } from '../execution-v2/generated.js';
import { limits } from '../execution-v2/generated.js';
import { omission } from '../execution/reconcile.js';
import type { Omission, TranscriptState } from '../execution/reconcile.js';

export type EventMetadata = Pick<RunEvent, 'event_id' | 'seq' | 'kind' | 'source' | 'occurred_at'>;
export interface EventWindow {
  readonly run_id: string;
  readonly loaded: boolean;
  readonly view: 'live' | 'history';
  readonly known_last_seq: number;
  readonly summary_seq: number;
  readonly pending: boolean;
  readonly problem_code: string | null;
  readonly omitted: Omission;
  readonly headers: readonly EventMetadata[];
}

export function projectEventWindow(transcript: TranscriptState | null, selected: RunSnapshot | undefined): EventWindow | null {
  // Closed provider set; the summary and every event must match the selection's provider.
  if (!transcript || !selected || (selected.provider_id !== 'fake' && selected.provider_id !== 'codex')) return null;
  const matches = (event: Pick<RunSnapshot, 'workspace_id' | 'session_id' | 'run_id' | 'provider_id'>) =>
    event.workspace_id === selected.workspace_id && event.session_id === selected.session_id &&
    event.run_id === selected.run_id && event.provider_id === selected.provider_id;
  if (!matches(transcript.summary) || transcript.events.some(event => !matches(event))) return null;
  // The controller already owns the 200-event/text-byte budget. This additional
  // count cap protects the metadata DOM without retaining another payload cache.
  const events = transcript.events.slice(-limits.visible_events);
  return {
    run_id: selected.run_id, loaded: transcript.loaded, view: transcript.view,
    known_last_seq: transcript.known_last_seq, summary_seq: transcript.summary_seq,
    pending: transcript.pass !== null, problem_code: transcript.problem?.code ?? null,
    omitted: omission({ ...transcript, events }),
    headers: events.map(({ event_id, seq, kind, source, occurred_at }) => ({ event_id, seq, kind, source, occurred_at }))
  };
}
