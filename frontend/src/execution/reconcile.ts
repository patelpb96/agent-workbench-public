import type * as Wire from '../execution-v2/generated.js';
import { limits } from '../execution-v2/generated.js';
import {
  ExecutionClientError, isCounter, isEventPage, isExecutionChanged, isRunSnapshot,
  isSaveLayoutResponse, pageMatchesRequest, utf8Bytes
} from './client.js';

export interface HistoryRange {
  readonly after_seq: number;
  readonly through_seq: number;
}

export interface Omission {
  readonly before: HistoryRange | null;
  readonly after: HistoryRange | null;
  readonly message: string | null;
}

export interface PageLoad {
  readonly generation: number;
  readonly request: Wire.EventPageRequest;
}

interface Pass {
  readonly next_after_seq: number;
  readonly through_seq: number | null;
  readonly replace: boolean;
}

export interface TranscriptState {
  readonly summary: Wire.RunSnapshot;
  /** Latest accepted native summary; never a transcript loading cursor. */
  readonly summary_seq: number;
  /** Upper bound observed in summaries/pages, including history not loaded. */
  readonly known_last_seq: number;
  /** Last event applied to the selected viewport. Explicit history selection can move this backward. */
  readonly applied_seq: number;
  readonly loaded: boolean;
  readonly events: readonly Wire.RunEvent[];
  readonly view: 'live' | 'history';
  readonly generation: number;
  readonly pass: Pass | null;
  readonly dirty: boolean;
  readonly problem: Wire.Error | null;
}

export interface LayoutState {
  readonly layout: Wire.Layout | null;
  readonly warning: Wire.Error | null;
  readonly dirty: boolean;
}

function copy<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') {
    const object = value as Record<string, unknown>;
    return '{' + Object.keys(object).sort().map(key => JSON.stringify(key) + ':' + canonical(object[key])).join(',') + '}';
  }
  return JSON.stringify(value) ?? 'undefined';
}

function same(left: unknown, right: unknown): boolean {
  return canonical(left) === canonical(right);
}

function historyError(): Wire.Error {
  return {
    code: 'event_history_unavailable',
    user_message: 'Transcript history could not be reconciled. Refresh this run’s history.',
    recoverable: true
  };
}

function problem(state: TranscriptState): TranscriptState {
  return { ...state, dirty: true, pass: null, problem: historyError() };
}

/** Plain text only. Controls are made visible; HTML escaping belongs to the text renderer. */
export function displayText(text: string): string {
  return text.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g,
    character => '\\u' + character.charCodeAt(0).toString(16).padStart(4, '0'));
}

export function eventText(event: Wire.RunEvent): string {
  let result: string;
  switch (event.kind) {
    case 'text_delta': result = event.payload.text; break;
    case 'run_created': result = 'Run queued.'; break;
    case 'run_state_changed':
      result = event.payload.from + ' → ' + event.payload.to +
        (event.payload.error === null ? '' : '\n' + event.payload.error.code + ': ' + event.payload.error.user_message);
      break;
    case 'tool_activity': result = 'Simulated tool activity: ' + event.payload.title + ' (' + event.payload.status + ')'; break;
    case 'usage_updated':
      result = (event.provider_id === 'codex' ? 'Reported usage: input ' : 'Simulated usage: input ') +
        (event.payload.input_tokens ?? 'unknown') +
        ', output ' + (event.payload.output_tokens ?? 'unknown') + (event.payload.estimated ? ' (estimated)' : '') +
        (event.provider_id === 'codex'
          ? ', cached ' + (event.payload.cached_input_tokens ?? 'unknown') + ', reasoning ' + (event.payload.reasoning_output_tokens ?? 'unknown')
          : '');
      break;
    case 'output_truncated':
      result = 'Output truncated (' + event.payload.scope + '): first discarded batch ' +
        event.payload.discarded_events + ' events, ' + (event.payload.discarded_bytes ?? 'unknown') + ' bytes.';
      break;
    case 'run_recovered': result = 'Run interrupted after restart; previous state: ' + event.payload.previous_state + '.'; break;
  }
  return displayText(result);
}

/** Reserve nontext_event_bytes for omission/status text and sequence labels in the renderer. */
export const renderedEventByteBudget = limits.frontend_text_bytes - limits.nontext_event_bytes;

export function boundedEvents(events: readonly Wire.RunEvent[]): Wire.RunEvent[] {
  const recent = events.slice(-limits.visible_events);
  const sizes = recent.map(event => utf8Bytes(eventText(event)));
  let total = sizes.reduce((sum, size) => sum + size, 0);
  let first = 0;
  while (total > renderedEventByteBudget && first < recent.length) {
    total -= sizes[first];
    first += 1;
  }
  return recent.slice(first);
}

export function createTranscript(summary: Wire.RunSnapshot): TranscriptState {
  if (!isRunSnapshot(summary)) throw new ExecutionClientError(historyError());
  return {
    summary: copy(summary), summary_seq: summary.last_seq, known_last_seq: summary.last_seq,
    applied_seq: 0, loaded: false, events: [], view: 'live', generation: 0,
    pass: null, dirty: true, problem: null
  };
}

/** Consume dirty BEFORE beginning asynchronous work. Responses must never clear later hints. */
export function beginReconcile(state: TranscriptState): TranscriptState {
  return { ...state, dirty: false };
}

export function markDirty(state: TranscriptState, hint: unknown): TranscriptState {
  return isExecutionChanged(hint) && hint.workspace_id === state.summary.workspace_id
    ? { ...state, dirty: true } : state;
}

export function acceptSummary(state: TranscriptState, incoming: Wire.RunSnapshot): TranscriptState {
  if (!isRunSnapshot(incoming) || incoming.workspace_id !== state.summary.workspace_id ||
      incoming.session_id !== state.summary.session_id || incoming.run_id !== state.summary.run_id ||
      incoming.provider_id !== state.summary.provider_id || incoming.created_at !== state.summary.created_at ||
      incoming.enqueue_order !== state.summary.enqueue_order) return problem(state);
  if (incoming.last_seq < state.summary_seq) return state;
  if (incoming.last_seq === state.summary_seq && !same(incoming, state.summary)) return problem(state);
  return {
    ...state, summary: copy(incoming), summary_seq: incoming.last_seq,
    known_last_seq: Math.max(state.known_last_seq, incoming.last_seq)
  };
}

/** Receipts are immutable originals. They cannot advance the loaded cursor or replace newer summaries. */
export function acceptRunResult(
  state: TranscriptState,
  result: Wire.StartFakeRunResponse | Wire.CancelRunResponse
): TranscriptState {
  if (result.contract_version !== 2 || typeof result.replayed !== 'boolean') return problem(state);
  return { ...acceptSummary(state, result.run), dirty: true };
}

export function acceptLayoutResult(state: LayoutState, result: Wire.SaveLayoutResponse): LayoutState {
  if (!isSaveLayoutResponse(result)) throw new ExecutionClientError({
    code: 'layout_invalid', user_message: 'The saved layout response could not be validated.', recoverable: true
  });
  // Preserve unsupported/invalid durable-layout warnings until an authoritative read resolves them.
  if (state.warning !== null) return { ...state, dirty: true };
  if (state.layout !== null) {
    if (state.layout.workspace_id !== result.layout.workspace_id) throw new ExecutionClientError({
      code: 'layout_invalid', user_message: 'The saved layout belongs to a different workspace.', recoverable: true
    });
    if (result.layout.revision < state.layout.revision) return { ...state, dirty: true };
    if (result.layout.revision === state.layout.revision) {
      if (!same(state.layout, result.layout)) throw new ExecutionClientError({
        code: 'layout_revision_conflict', user_message: 'The saved layout revision conflicts with the current layout.', recoverable: true
      });
      return { ...state, dirty: true };
    }
  }
  return { ...state, layout: copy(result.layout), dirty: true };
}

export function nextLoad(state: TranscriptState): PageLoad | null {
  if (state.pass === null) return null;
  return {
    generation: state.generation,
    request: {
      contract_version: 2, workspace_id: state.summary.workspace_id, run_id: state.summary.run_id,
      after_seq: state.pass.next_after_seq, through_seq: state.pass.through_seq, limit: limits.page_events
    }
  };
}

export function beginLoad(
  state: TranscriptState,
  range: { after_seq: number; through_seq: number | null; mode: 'replace' | 'append'; view?: 'live' | 'history' }
): { state: TranscriptState; load: PageLoad } {
  if (!isCounter(range.after_seq) || !(range.through_seq === null ||
      (isCounter(range.through_seq) && range.after_seq <= range.through_seq)) ||
      (range.mode === 'append' && range.after_seq !== state.applied_seq) ||
      state.generation === Number.MAX_SAFE_INTEGER) throw new ExecutionClientError(historyError());
  const next: TranscriptState = {
    ...state, generation: state.generation + 1, view: range.view ?? state.view, problem: null,
    pass: { next_after_seq: range.after_seq, through_seq: range.through_seq, replace: range.mode === 'replace' }
  };
  return { state: next, load: nextLoad(next)! };
}

export function beginTail(state: TranscriptState): { state: TranscriptState; load: PageLoad } {
  return beginLoad(state, {
    after_seq: Math.max(0, state.known_last_seq - limits.visible_events),
    through_seq: state.known_last_seq, mode: 'replace', view: 'live'
  });
}

export function beginCatchUp(state: TranscriptState): { state: TranscriptState; load: PageLoad } {
  if (!state.loaded || state.view === 'history') return beginTail(state);
  return beginLoad(state, { after_seq: state.applied_seq, through_seq: null, mode: 'append', view: 'live' });
}

export function omission(state: TranscriptState): Omission {
  if (!state.loaded) return {
    before: null,
    after: state.known_last_seq > 0 ? { after_seq: 0, through_seq: state.known_last_seq } : null,
    message: 'Transcript history has not been loaded.'
  };
  const first = state.events.length > 0 ? state.events[0].seq : state.applied_seq + 1;
  const last = state.events.length > 0 ? state.events[state.events.length - 1].seq : state.applied_seq;
  const before = first > 1 ? { after_seq: 0, through_seq: first - 1 } : null;
  const after = last < state.known_last_seq ? { after_seq: last, through_seq: state.known_last_seq } : null;
  const messages: string[] = [];
  if (before !== null) messages.push('Earlier history omitted: events 1–' + before.through_seq + '. Load an earlier bounded page.');
  if (after !== null) messages.push('Later history omitted: events ' + (after.after_seq + 1) + '–' + after.through_seq + '.');
  return { before, after, message: messages.length > 0 ? messages.join(' ') : null };
}

export function beginEarlier(state: TranscriptState): { state: TranscriptState; load: PageLoad } | null {
  const earlier = omission(state).before;
  if (earlier === null) return null;
  return beginLoad(state, {
    after_seq: Math.max(0, earlier.through_seq - limits.page_events),
    through_seq: earlier.through_seq, mode: 'replace', view: 'history'
  });
}

export function applyPage(state: TranscriptState, load: PageLoad, page: Wire.EventPage): TranscriptState {
  // A response from a superseded explicit history selection cannot change the active viewport.
  if (load.generation !== state.generation) return state;
  if (!isEventPage(page) || !pageMatchesRequest(load.request, page) ||
      page.workspace_id !== state.summary.workspace_id || page.run_id !== state.summary.run_id ||
      page.events.some(event => event.session_id !== state.summary.session_id)) return problem(state);

  const bySequence = new Map(state.events.map(event => [event.seq, event]));
  const byId = new Map(state.events.map(event => [event.event_id, event]));
  for (const event of page.events) {
    const sameSequence = bySequence.get(event.seq);
    const sameId = byId.get(event.event_id);
    if ((sameSequence !== undefined && !same(sameSequence, event)) ||
        (sameId !== undefined && !same(sameId, event))) return problem(state);
  }

  const pass = state.pass;
  if (pass === null || load.request.after_seq !== pass.next_after_seq) {
    // Verify retained duplicates by full identity/content. Evicted duplicates are unverifiable:
    // request reconciliation instead of retaining an unbounded fingerprint/event history.
    if (page.events.length > 0 && page.next_after_seq <= state.applied_seq &&
        page.events.every(event => {
          const old = bySequence.get(event.seq);
          return old !== undefined && same(old, event);
        })) return state;
    if (pass === null && page.events.length === 0 && page.next_after_seq === state.applied_seq) return state;
    return problem(state);
  }
  if (load.request.through_seq !== pass.through_seq ||
      (!pass.replace && load.request.after_seq !== state.applied_seq)) return problem(state);

  const events = boundedEvents(pass.replace ? copy(page.events) : [...state.events, ...copy(page.events)]);
  return {
    ...state, events, loaded: true, applied_seq: page.next_after_seq,
    known_last_seq: Math.max(state.known_last_seq, page.last_seq),
    pass: page.has_more ? { next_after_seq: page.next_after_seq, through_seq: page.through_seq, replace: false } : null,
    // A later observed high-water means another pass is required, never a cursor jump.
    dirty: state.dirty || page.last_seq > page.through_seq || state.summary_seq > page.through_seq,
    problem: null
  };
}
