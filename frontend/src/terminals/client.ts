// Thin, strictly validated client for the native terminal commands. The webview never
// talks to the broker or a host; every reply is checked before use.

export interface TerminalInfo {
  readonly id: string;
  readonly label: string;
  readonly state: 'running' | 'exited' | 'lost' | 'unresponsive';
  readonly persistent: boolean;
  readonly cols: number;
  readonly rows: number;
  readonly host_pid: number;
  /** In-memory output history in MiB (1 standard, 32 extended); absent from older services. */
  readonly history_mib?: number;
}
export interface DraftView { readonly agent: string; readonly text: string; readonly multiline: boolean }
export interface GrantView { readonly agent: string; readonly remaining_ms: number; readonly keep_after_detach: boolean }
export type GrantDuration = 'one_minute' | 'fifteen_minutes' | 'one_hour' | 'one_day';
export const grantDurations: readonly { id: GrantDuration; label: string }[] = [
  { id: 'one_minute', label: '1 minute' }, { id: 'fifteen_minutes', label: '15 minutes' },
  { id: 'one_hour', label: '1 hour' }, { id: 'one_day', label: '1 day' }
];
export interface TerminalOutput {
  readonly bytes: Uint8Array;
  readonly next: number;
  readonly skipped: number;
}
export interface TerminalError { readonly code: string; readonly message: string }
/** A terminal's output log (tmux pipe-pane): `path` once a log exists, `stopped_reason` when it stopped by itself. */
export interface LogState { readonly on: boolean; readonly path: string | null; readonly bytes: number; readonly stopped_reason: string | null }
export type LogAction = 'start' | 'stop' | 'status';

export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const idPattern = /^[0-9a-f]{8}$/;

function record(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function counter(v: unknown): v is number {
  return typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
}

export function isTerminalInfo(v: unknown): v is TerminalInfo {
  return record(v) && typeof v.id === 'string' && idPattern.test(v.id) && typeof v.label === 'string' && v.label.length <= 256 &&
    (v.state === 'running' || v.state === 'exited' || v.state === 'lost' || v.state === 'unresponsive') && typeof v.persistent === 'boolean' &&
    counter(v.cols) && counter(v.rows) && counter(v.host_pid) && (v.history_mib === undefined || (counter(v.history_mib) && v.history_mib <= 1024));
}

const phrasePattern = /[^A-Za-z0-9!@#$%^&*()]/g;
/** The confirmation phrase allows letters, digits and the shifted digit-row symbols only (max 256). */
export function filterPhrase(text: string): string {
  return text.replace(phrasePattern, '').slice(0, 256);
}
export function phraseOk(text: string): boolean {
  // A non-global pattern: global regexes keep state between .test calls.
  return text.length <= 256 && !/[^A-Za-z0-9!@#$%^&*()]/.test(text);
}
export function validAgentName(name: string): boolean {
  return /^[A-Za-z0-9._-]{1,64}$/.test(name);
}
export function fmtRemaining(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 90) return `${s} s`;
  const m = Math.round(s / 60);
  if (m < 90) return `${m} min`;
  const h = Math.round(m / 60);
  return h < 36 ? `${h} h` : `${Math.round(h / 24)} d`;
}
function isDraft(v: unknown): v is DraftView {
  return record(v) && typeof v.agent === 'string' && v.agent.length <= 64 && typeof v.text === 'string' && v.text.length <= 4096 && typeof v.multiline === 'boolean';
}
export function isLogState(v: unknown): v is LogState & { op: 'logging' } {
  return record(v) && v.op === 'logging' && typeof v.on === 'boolean' && counter(v.bytes) &&
    (v.path === null || (typeof v.path === 'string' && v.path.length > 0 && v.path.length <= 1024)) &&
    (v.stopped_reason === null || (typeof v.stopped_reason === 'string' && v.stopped_reason.length <= 200)) &&
    Object.keys(v).length === 5;
}
function isGrant(v: unknown): v is GrantView {
  return record(v) && typeof v.agent === 'string' && v.agent.length <= 64 && counter(v.remaining_ms) && typeof v.keep_after_detach === 'boolean';
}

export function decodeBase64(text: string): Uint8Array | null {
  if (text.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(text)) return null;
  try {
    const raw = atob(text);
    const out = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

export function sanitizeError(e: unknown): TerminalError {
  if (record(e) && typeof e.code === 'string' && typeof e.user_message === 'string') {
    return { code: e.code.slice(0, 64), message: e.user_message.slice(0, 300) };
  }
  return { code: 'terminal_failed', message: 'The terminal service did not respond.' };
}

export function createTerminalClient(call: Invoke) {
  const wrap = async <T>(command: string, args: Record<string, unknown> | undefined, check: (v: unknown) => T | null): Promise<T> => {
    let reply: unknown;
    try {
      reply = await call(command, args);
    } catch (e) {
      throw sanitizeError(e);
    }
    const ok = check(reply);
    if (ok === null) throw { code: 'invalid_response', message: 'The terminal reply could not be validated.' } satisfies TerminalError;
    return ok;
  };
  return {
    list: () => wrap('terminal_list', undefined, v =>
      record(v) && v.op === 'sessions' && Array.isArray(v.items) && v.items.length <= 256 && v.items.every(isTerminalInfo) ? (v.items as TerminalInfo[]) : null),
    /** `folder` (the Files panel's "Open terminal here") starts it there; otherwise the Properties start folder is used. */
    create: (cols: number, rows: number, label: string, extendedHistory = false, folder?: string) => wrap('terminal_new', folder === undefined ? { cols, rows, label, extendedHistory } : { cols, rows, label, extendedHistory, folder }, v =>
      record(v) && v.op === 'created' && isTerminalInfo(v.session) ? (v.session as TerminalInfo) : null),
    read: (session: string, from: number) => wrap('terminal_read', { session, from }, v => {
      if (!record(v) || v.op !== 'output' || typeof v.data !== 'string' || !counter(v.next) || !counter(v.skipped)) return null;
      const bytes = decodeBase64(v.data);
      return bytes === null ? null : { bytes, next: v.next, skipped: v.skipped };
    }),
    /** Output for many terminals at once; waits (up to `waitMs`) until one has output beyond its cursor. */
    watch: (cursors: readonly { session: string; from: number }[], waitMs: number) =>
      wrap('terminal_watch', { cursors: cursors.map(c => [c.session, c.from]), waitMs }, v => {
        if (!record(v) || v.op !== 'watch' || !Array.isArray(v.items) || v.items.length > 64 || !Array.isArray(v.gone) || v.gone.length > 64) return null;
        const items: { session: string; from: number; bytes: Uint8Array; next: number; skipped: number }[] = [];
        for (const item of v.items) {
          if (!record(item) || typeof item.session !== 'string' || !idPattern.test(item.session) || !counter(item.from) || typeof item.data !== 'string' || !counter(item.next) || !counter(item.skipped)) return null;
          const bytes = decodeBase64(item.data);
          if (bytes === null) return null;
          items.push({ session: item.session, from: item.from, bytes, next: item.next, skipped: item.skipped });
        }
        if (!v.gone.every(g => typeof g === 'string' && idPattern.test(g))) return null;
        return { items, gone: v.gone as string[] };
      }),
    input: (session: string, text: string) => wrap('terminal_input', { session, text }, () => true),
    resize: (session: string, cols: number, rows: number) => wrap('terminal_resize', { session, cols, rows }, () => true),
    end: (session: string) => wrap('terminal_end', { session }, () => true),
    /** Recreate a terminal that did not survive a restart, under its old id. */
    restore: (session: string) => wrap('terminal_restore', { session }, v =>
      record(v) && v.op === 'created' && isTerminalInfo(v.session) ? (v.session as TerminalInfo) : null),
    draft: (session: string) => wrap('terminal_draft', { session }, v =>
      record(v) && v.op === 'draft' && (v.draft === null || isDraft(v.draft)) ? { draft: v.draft as DraftView | null } : null),
    /** Start, stop or look at a terminal's output log. The service chooses the file; no path is ever sent. */
    log: (session: string, action: LogAction) => wrap('terminal_log', { session, action }, v =>
      isLogState(v) ? { on: v.on, path: v.path, bytes: v.bytes, stopped_reason: v.stopped_reason } as LogState : null),
    draftAccept: (session: string) => wrap('terminal_draft_accept', { session }, () => true),
    draftDiscard: (session: string) => wrap('terminal_draft_discard', { session }, () => true),
    grants: (session: string) => wrap('terminal_grants', { session }, v =>
      record(v) && v.op === 'grants' && Array.isArray(v.items) && v.items.length <= 64 && v.items.every(isGrant) ? (v.items as GrantView[]) : null),
    grantCreate: (agent: string, session: string, duration: GrantDuration, keepAfterDetach: boolean, gate: string) =>
      wrap('terminal_grant_create', { agent, session, duration, keepAfterDetach, gate }, () => true),
    grantRevoke: (agent: string, session: string) => wrap('terminal_grant_revoke', { agent, session }, () => true),
    gateSet: (current: string, next: string) => wrap('terminal_gate_set', { current, new: next }, () => true),
    autostartGet: () => wrap('terminal_autostart_get', undefined, v => typeof v === 'boolean' ? v : null),
    autostartSet: (enabled: boolean) => wrap('terminal_autostart_set', { enabled }, v => typeof v === 'boolean' ? v : null)
  };
}
