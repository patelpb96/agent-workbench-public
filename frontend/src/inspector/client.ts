// Strictly validated client for the read-only agent inspector commands. Pure (no Tauri
// import) so it is unit-tested; the Tauri wiring lives in tauri.ts.

export type AgentName = 'claude' | 'pi' | 'codex';
export type SessionStateName = 'empty' | 'working' | 'waiting_for_human' | 'idle';

export interface SourceInfo {
  readonly id: string;
  readonly agent: AgentName;
  readonly label: string;
  readonly modified_ms: number;
  readonly bytes: number;
  readonly active: boolean;
  /** Claude Code only: reported as running (in the background, or in another terminal window). */
  readonly running: 'background' | 'interactive' | null;
  /** The model named by the latest assistant message, when the transcript names one. */
  readonly model: string | null;
  /** The last line of the last assistant message: the only transcript text the inspector shows. */
  readonly last_line: string | null;
  /** The name of the folder the session works in (not its path). */
  readonly folder: string | null;
  /** `session`, or `subagent` for a Claude subagent transcript. */
  readonly kind: 'session' | 'subagent';
  /** Opaque id of the parent session when it exists on disk; null otherwise. */
  readonly parent: string | null;
}
/** What the agent map needs besides the list: tool names and counts per source, and who reports to whom (all opaque source ids). */
export interface MapPayload {
  readonly generated_ms: number;
  readonly sessions: readonly { readonly id: string; readonly open_calls: number; readonly open_tools: readonly string[]; readonly tools: readonly { readonly name: string; readonly calls: number; readonly errors: number }[] }[];
  readonly reports: readonly { readonly manager: string; readonly report: string }[];
}
export interface ToolRow {
  readonly name: string;
  readonly calls: number;
  readonly completed: number;
  readonly errors: number;
  readonly unknown_outcome: number;
  readonly total_ms: number;
  readonly max_ms: number;
  readonly median_ms: number | null;
  readonly last_used_ms: number | null;
}
export interface CallView {
  readonly tool: string;
  readonly started_ms: number | null;
  readonly duration_ms: number | null;
  readonly ok: boolean | null;
}
export interface Usage {
  readonly input: number | null;
  readonly output: number | null;
  readonly cache_read: number | null;
  readonly cache_write: number | null;
  readonly reasoning: number | null;
  readonly records: number;
}
export interface Bucket { readonly calls: number; readonly errors: number; readonly tool_ms: number }
export interface Timeline { readonly start_ms: number; readonly bucket_ms: number; readonly buckets: readonly Bucket[] }
export interface Snapshot {
  readonly state: SessionStateName;
  readonly first_event_ms: number | null;
  readonly last_event_ms: number | null;
  readonly turns: number;
  readonly open_calls: number;
  readonly usage: Usage;
  readonly breakdown: { readonly tool_ms: number; readonly model_ms: number; readonly human_ms: number };
  readonly tools: readonly ToolRow[];
  readonly tools_omitted: number;
  readonly recent: readonly CallView[];
  readonly timeline: Timeline | null;
  readonly orphan_results: number;
}
export interface Peek { readonly last_line: string | null; readonly model: string | null }
export interface InspectorReply {
  readonly source: Pick<SourceInfo, 'id' | 'agent' | 'label' | 'modified_ms' | 'bytes'>;
  readonly now_ms: number;
  readonly snapshot: Snapshot;
  readonly peek: Peek;
}
/** Colour tags: opaque source id -> palette colour name. */
export interface Tags { readonly version: 1; readonly tags: Readonly<Record<string, string>> }
export interface InspectorError { readonly code: string; readonly message: string }
export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const idPattern = /^[0-9a-f]{16}$/;
/** The native list holds up to 60 sessions + 20 subagents + 10 parents pulled in for them; leave room. */
const MAX_SOURCES_LISTED = 128;
const agents = ['claude', 'pi', 'codex'];
const kindList = ['session', 'subagent'];
const states = ['empty', 'working', 'waiting_for_human', 'idle'];

function rec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function num(v: unknown): v is number {
  return typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
}
function numOrNull(v: unknown): v is number | null {
  return v === null || num(v);
}
function text(v: unknown, max: number): v is string {
  return typeof v === 'string' && v.length <= max;
}

function textOrNull(v: unknown, max: number): v is string | null {
  return v === null || text(v, max);
}
export function isPeek(v: unknown): v is Peek {
  return rec(v) && textOrNull(v.last_line, 220) && textOrNull(v.model, 100);
}
export function isSource(v: unknown): v is SourceInfo {
  return rec(v) && typeof v.id === 'string' && idPattern.test(v.id) && agents.includes(String(v.agent)) && text(v.label, 300) &&
    num(v.modified_ms) && num(v.bytes) && typeof v.active === 'boolean' &&
    (v.running === null || v.running === 'background' || v.running === 'interactive') &&
    textOrNull(v.model, 100) && textOrNull(v.last_line, 220) && textOrNull(v.folder, 100) &&
    kindList.includes(String(v.kind)) && (v.parent === null || (typeof v.parent === 'string' && idPattern.test(v.parent)));
}
const MAP_SESSIONS_MAX = 128;
const MAP_TOOLS_MAX = 100;
const MAP_REPORTS_MAX = 256;
const MAP_OPEN_TOOLS_MAX = 8;
export function isMapPayload(v: unknown): v is MapPayload {
  return rec(v) && Object.keys(v).length === 3 && num(v.generated_ms) &&
    Array.isArray(v.sessions) && v.sessions.length <= MAP_SESSIONS_MAX &&
    v.sessions.every(s => rec(s) && Object.keys(s).length === 4 && typeof s.id === 'string' && idPattern.test(s.id) && num(s.open_calls) &&
      Array.isArray(s.open_tools) && s.open_tools.length <= MAP_OPEN_TOOLS_MAX && s.open_tools.every(t => text(t, 200)) &&
      Array.isArray(s.tools) && s.tools.length <= MAP_TOOLS_MAX &&
      s.tools.every(t => rec(t) && Object.keys(t).length === 3 && text(t.name, 200) && num(t.calls) && num(t.errors))) &&
    Array.isArray(v.reports) && v.reports.length <= MAP_REPORTS_MAX &&
    v.reports.every(r => rec(r) && Object.keys(r).length === 2 && typeof r.manager === 'string' && idPattern.test(r.manager) && typeof r.report === 'string' && idPattern.test(r.report));
}
/** The payload's sessions keyed by source id, in the shape the map model reads. */
export function toMapFacts(payload: MapPayload): Record<string, { readonly tools: readonly { readonly name: string; readonly calls: number; readonly errors: number }[]; readonly open_calls: number; readonly open_tools: readonly string[] }> {
  return Object.fromEntries(payload.sessions.map(s => [s.id, { tools: s.tools, open_calls: s.open_calls, open_tools: s.open_tools }]));
}
const tagColors = ['red', 'orange', 'yellow', 'green', 'blue', 'purple'];
export function isTags(v: unknown): v is Tags {
  return rec(v) && v.version === 1 && Object.keys(v).length === 2 && rec(v.tags) && Object.keys(v.tags).length <= 256 &&
    Object.entries(v.tags).every(([id, color]) => idPattern.test(id) && typeof color === 'string' && tagColors.includes(color));
}
function isTool(v: unknown): v is ToolRow {
  return rec(v) && text(v.name, 200) && num(v.calls) && num(v.completed) && num(v.errors) && num(v.unknown_outcome) &&
    num(v.total_ms) && num(v.max_ms) && numOrNull(v.median_ms) && numOrNull(v.last_used_ms);
}
function isCall(v: unknown): v is CallView {
  return rec(v) && text(v.tool, 200) && numOrNull(v.started_ms) && numOrNull(v.duration_ms) && (v.ok === null || typeof v.ok === 'boolean');
}
function isUsage(v: unknown): v is Usage {
  return rec(v) && numOrNull(v.input) && numOrNull(v.output) && numOrNull(v.cache_read) && numOrNull(v.cache_write) &&
    numOrNull(v.reasoning) && num(v.records);
}
function isTimeline(v: unknown): v is Timeline {
  return rec(v) && num(v.start_ms) && num(v.bucket_ms) && v.bucket_ms > 0 && Array.isArray(v.buckets) && v.buckets.length >= 1 && v.buckets.length <= 48 &&
    v.buckets.every(b => rec(b) && num(b.calls) && num(b.errors) && num(b.tool_ms));
}
export function isSnapshot(v: unknown): v is Snapshot {
  return rec(v) && states.includes(String(v.state)) && numOrNull(v.first_event_ms) && numOrNull(v.last_event_ms) && num(v.turns) &&
    num(v.open_calls) && isUsage(v.usage) && rec(v.breakdown) && num(v.breakdown.tool_ms) && num(v.breakdown.model_ms) &&
    num(v.breakdown.human_ms) && Array.isArray(v.tools) && v.tools.length <= 100 && v.tools.every(isTool) && num(v.tools_omitted) &&
    Array.isArray(v.recent) && v.recent.length <= 50 && v.recent.every(isCall) && (v.timeline === null || isTimeline(v.timeline)) && num(v.orphan_results);
}

export function sanitizeError(e: unknown): InspectorError {
  if (rec(e) && typeof e.code === 'string' && typeof e.user_message === 'string') {
    return { code: e.code.slice(0, 64), message: e.user_message.slice(0, 300) };
  }
  return { code: 'inspector_failed', message: 'The inspector did not respond.' };
}

export function createInspectorClient(call: Invoke) {
  const wrap = async <T>(command: string, args: Record<string, unknown> | undefined, check: (v: unknown) => T | null): Promise<T> => {
    let reply: unknown;
    try {
      reply = await call(command, args);
    } catch (e) {
      throw sanitizeError(e);
    }
    const ok = check(reply);
    if (ok === null) throw { code: 'invalid_response', message: 'The inspector reply could not be validated.' } satisfies InspectorError;
    return ok;
  };
  return {
    sources: () => wrap('inspector_sources', undefined, v => Array.isArray(v) && v.length <= MAX_SOURCES_LISTED && v.every(isSource) ? (v as SourceInfo[]) : null),
    tags: () => wrap('inspector_tags_load', undefined, v => (isTags(v) ? v : null)),
    saveTags: (tags: Tags) => (isTags(tags)
      ? wrap('inspector_tags_save', { tags }, v => (isTags(v) ? v : null))
      : Promise.reject({ code: 'inspector_failed', message: 'Those tags are not valid.' } satisfies InspectorError)),
    map: () => wrap('inspector_map', undefined, v => (isMapPayload(v) ? v : null)),
    snapshot: (id: string) => {
      if (!idPattern.test(id)) return Promise.reject({ code: 'inspector_failed', message: 'Invalid source id.' } satisfies InspectorError);
      return wrap('inspector_snapshot', { id }, v => rec(v) && rec(v.source) && typeof v.source.id === 'string' && idPattern.test(v.source.id) &&
        v.source.id === id && agents.includes(String(v.source.agent)) && text(v.source.label, 300) && num(v.source.modified_ms) &&
        num(v.source.bytes) && num(v.now_ms) && isSnapshot(v.snapshot) && isPeek(v.peek) ? (v as unknown as InspectorReply) : null);
    }
  };
}

// ---- display helpers (pure) ----------------------------------------------

export function fmtDuration(ms: number | null): string {
  if (ms === null) return 'unknown';
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${Math.round(s - m * 60)} s`;
  const h = Math.floor(m / 60);
  return `${h} h ${m - h * 60} min`;
}
export function fmtAgo(then: number | null, now: number): string {
  if (then === null) return 'never';
  const d = Math.max(0, now - then);
  if (d < 5000) return 'just now';
  return `${fmtDuration(d - (d % 1000))} ago`;
}
export function share(part: number, whole: number): string {
  return whole > 0 ? `${Math.round((part / whole) * 100)}%` : '0%';
}
export function sortTools(rows: readonly ToolRow[], key: 'name' | 'calls' | 'errors' | 'last_used_ms' | 'total_ms' | 'median_ms', desc: boolean): ToolRow[] {
  const out = [...rows];
  const val = (r: ToolRow): number | string => key === 'name' ? r.name.toLowerCase() : (r[key] ?? -1);
  out.sort((a, b) => {
    const x = val(a), y = val(b);
    const c = x < y ? -1 : x > y ? 1 : 0;
    return (desc ? -c : c) || a.name.localeCompare(b.name);
  });
  return out;
}
