import type * as Wire from '../execution-v2/generated.js';
import { executionNotification, limits, runTransitions } from '../execution-v2/generated.js';

export { executionNotification };
export const availableProviders = ['fake', 'codex'] as const;
export type Transport = (command: string, args: { request: unknown }) => Promise<unknown>;

export const progressSteps = ['thread_started', 'turn_started', 'reasoning_observed', 'message_complete'] as const;
/** Volatile, text-free progress of the active Codex run. Display only; never journaled. */
export interface RunProgress {
  readonly contract_version: 2;
  readonly workspace_id: Wire.Id;
  readonly run_id: Wire.Id | null;
  readonly steps: readonly { readonly step: (typeof progressSteps)[number]; readonly count: number }[];
}

export interface ExecutionClient {
  bootstrap_workspace(request: Wire.BootstrapRequest): Promise<Wire.WorkspaceSnapshot>;
  create_session(request: Wire.CreateSessionRequest): Promise<Wire.CreateSessionResponse>;
  start_fake_run(request: Wire.StartFakeRunRequest): Promise<Wire.StartFakeRunResponse>;
  start_codex_run(request: Wire.StartCodexRunRequest): Promise<Wire.StartCodexRunResponse>;
  cancel_run(request: Wire.CancelRunRequest): Promise<Wire.CancelRunResponse>;
  get_workspace_snapshot(request: Wire.WorkspaceRequest): Promise<Wire.WorkspaceSnapshot>;
  list_run_events(request: Wire.EventPageRequest): Promise<Wire.EventPage>;
  /** Optional display-only read; absent clients simply show no progress. */
  get_run_progress?(request: Wire.WorkspaceRequest): Promise<RunProgress>;
  /** Optional read: the exact text a carried-context Codex turn would send. Nothing is admitted. */
  preview_codex_context?(request: Wire.PreviewCodexContextRequest): Promise<Wire.CodexContextPreview>;
  save_layout(request: Wire.SaveLayoutRequest): Promise<Wire.SaveLayoutResponse>;
}

export function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).byteLength;
}

function record(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function shape(value: unknown, fields: readonly string[]): value is Record<string, unknown> {
  return record(value) && Object.keys(value).length === fields.length &&
    fields.every(field => Object.prototype.hasOwnProperty.call(value, field));
}

export function isCanonicalId(value: unknown): value is Wire.Id {
  return typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
}

export function isCounter(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function positive(value: unknown): value is number {
  return isCounter(value) && value > 0;
}

function timestamp(value: unknown): value is Wire.Timestamp {
  return typeof value === 'string' && value.length <= 40 &&
    /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/.test(value) &&
    Number.isFinite(Date.parse(value));
}

function text(value: unknown): value is string {
  return typeof value === 'string';
}

// JSON Schema length and native Rust chars() count code points, not UTF-16 units.
function boundedText(value: unknown, minimum: number, maximum: number): value is string {
  if (typeof value !== 'string' || value.length > maximum * 2) return false;
  let count = 0;
  for (const _character of value) if (++count > maximum) return false;
  return count >= minimum;
}

function label(value: unknown): value is string {
  return boundedText(value, 1, 128) && !/[\u0000-\u001f\u007f-\u009f]/.test(value);
}

export function isExecutionError(value: unknown): value is Wire.Error {
  try {
    return shape(value, ['code', 'user_message', 'recoverable']) &&
      boundedText(value.code, 1, 64) && boundedText(value.user_message, 1, 2048) &&
      typeof value.recoverable === 'boolean';
  } catch {
    return false;
  }
}

function genericError(): Wire.Error {
  return {
    code: 'execution_transport_error',
    user_message: 'The execution request could not be completed.',
    recoverable: true
  };
}

export class ExecutionClientError extends Error {
  readonly error: Wire.Error;
  readonly code: string;
  readonly user_message: string;
  readonly recoverable: boolean;

  constructor(detail: Wire.Error) {
    const safe = isExecutionError(detail) ? { ...detail } : genericError();
    super(safe.user_message);
    this.name = 'ExecutionClientError';
    this.stack = undefined;
    this.error = Object.freeze(safe);
    this.code = safe.code;
    this.user_message = safe.user_message;
    this.recoverable = safe.recoverable;
  }
}

export function sanitizeExecutionError(value: unknown): Wire.Error {
  try {
    if (value instanceof ExecutionClientError && isExecutionError(value.error)) return { ...value.error };
    if (isExecutionError(value)) return { ...value };
  } catch {
    // Never use an unknown exception's message, stack, cause, or string conversion.
  }
  return genericError();
}

function failure(code: string, user_message: string): ExecutionClientError {
  return new ExecutionClientError({ code, user_message, recoverable: true });
}

function state(value: unknown): value is Wire.RunState {
  return text(value) && Object.prototype.hasOwnProperty.call(runTransitions, value);
}

function fakeState(value: unknown): value is Wire.RunState {
  return state(value) && value !== 'waiting_approval';
}

function nullableCounter(value: unknown): boolean {
  return value === null || isCounter(value);
}

function usage(value: unknown): value is Wire.Usage {
  return shape(value, ['input_tokens', 'output_tokens', 'estimated', 'cached_input_tokens',
    'cache_write_input_tokens', 'reasoning_output_tokens']) &&
    nullableCounter(value.input_tokens) && nullableCounter(value.output_tokens) &&
    nullableCounter(value.cached_input_tokens) && nullableCounter(value.cache_write_input_tokens) &&
    nullableCounter(value.reasoning_output_tokens) && typeof value.estimated === 'boolean';
}

function provider(value: unknown): value is Wire.ProviderId {
  return value === 'fake' || value === 'codex';
}

// Closed, versioned profiles: fake has none; Codex is codex_text_v1 or codex_text_v2 on gpt-6.1-sol.
function profiled(providerId: unknown, profile: unknown, model: unknown): boolean {
  return providerId === 'fake' ? profile === null && model === null
    : providerId === 'codex' && (profile === 'codex_text_v1' || profile === 'codex_text_v2') && model === 'gpt-6.1-sol';
}

// v1 never carries earlier runs; v2 always carries 1..=6 distinct canonical run ids.
function contextShape(profile: unknown, request: Record<string, unknown>): boolean {
  const present = Object.prototype.hasOwnProperty.call(request, 'context_run_ids');
  if (profile === 'codex_text_v1') return !present;
  const ids = request.context_run_ids;
  return profile === 'codex_text_v2' && present && Array.isArray(ids) && ids.length >= 1 &&
    ids.length <= limits.codex_context_runs && ids.every(isCanonicalId) && new Set(ids).size === ids.length &&
    !ids.some(id => id === request.request_id);
}

export function isRunSnapshot(value: unknown): value is Wire.RunSnapshot {
  return shape(value, ['contract_version', 'workspace_id', 'session_id', 'run_id', 'provider_id', 'profile_id', 'model_id', 'state', 'last_seq', 'created_at', 'updated_at', 'usage', 'terminal_error', 'enqueue_order']) &&
    value.contract_version === 2 && isCanonicalId(value.workspace_id) &&
    isCanonicalId(value.session_id) && isCanonicalId(value.run_id) && provider(value.provider_id) &&
    profiled(value.provider_id, value.profile_id, value.model_id) &&
    fakeState(value.state) && positive(value.last_seq) && positive(value.enqueue_order) &&
    timestamp(value.created_at) && timestamp(value.updated_at) && usage(value.usage) &&
    (value.state === 'failed' ? isExecutionError(value.terminal_error) : value.terminal_error === null);
}

function session(value: unknown): value is Wire.SessionSnapshot {
  return shape(value, ['contract_version', 'workspace_id', 'session_id', 'provider_id', 'label', 'created_at']) &&
    value.contract_version === 2 && isCanonicalId(value.workspace_id) && isCanonicalId(value.session_id) &&
    provider(value.provider_id) && label(value.label) && timestamp(value.created_at);
}

function geometry(value: unknown): value is Wire.Geometry {
  if (!shape(value, ['x', 'y', 'width', 'height'])) return false;
  return ['x', 'y', 'width', 'height'].every(key => {
    const number = value[key];
    return typeof number === 'number' && Number.isFinite(number) &&
      number >= (key === 'x' || key === 'y' ? 0 : 1) && number <= 100000;
  });
}

export function isLayout(value: unknown): value is Wire.Layout {
  if (!shape(value, ['layout_version', 'workspace_id', 'revision', 'theme', 'active_module', 'sidebar_width', 'panels', 'containers']) ||
      value.layout_version !== 1 || !isCanonicalId(value.workspace_id) || !isCounter(value.revision) ||
      !['dark', 'light', 'system', 'contrast'].includes(String(value.theme)) ||
      !['agents', 'tasks', 'usage', 'approvals', 'diagnostics'].includes(String(value.active_module)) ||
      typeof value.sidebar_width !== 'number' || !Number.isFinite(value.sidebar_width) ||
      value.sidebar_width < 240 || value.sidebar_width > 440 ||
      !Array.isArray(value.panels) || value.panels.length > 16 ||
      !Array.isArray(value.containers) || value.containers.length > 8) return false;
  const containers = new Set<string>();
  for (const container of value.containers) {
    if (!shape(container, ['container_id', 'geometry']) || !isCanonicalId(container.container_id) ||
        !geometry(container.geometry) || containers.has(container.container_id)) return false;
    containers.add(container.container_id);
  }
  const panels = new Set<string>();
  const sessions = new Set<string>();
  for (const panel of value.panels) {
    if (!shape(panel, ['panel_id', 'session_id', 'container_id', 'stage', 'geometry', 'expanded_geometry']) ||
        !isCanonicalId(panel.panel_id) || !isCanonicalId(panel.session_id) ||
        !(panel.container_id === null || (isCanonicalId(panel.container_id) && containers.has(panel.container_id))) ||
        !['full', 'compact', 'block'].includes(String(panel.stage)) ||
        !geometry(panel.geometry) || !geometry(panel.expanded_geometry) ||
        panels.has(panel.panel_id) || containers.has(panel.panel_id) || sessions.has(panel.session_id)) return false;
    panels.add(panel.panel_id);
    sessions.add(panel.session_id);
  }
  return true;
}

export function isRunEvent(value: unknown): value is Wire.RunEvent {
  if (!shape(value, ['contract_version', 'event_id', 'workspace_id', 'session_id', 'run_id', 'provider_id', 'source', 'seq', 'occurred_at', 'kind', 'payload']) ||
      value.contract_version !== 2 || !isCanonicalId(value.event_id) || !isCanonicalId(value.workspace_id) ||
      !isCanonicalId(value.session_id) || !isCanonicalId(value.run_id) || !provider(value.provider_id) ||
      !['fixture', 'system', 'provider'].includes(String(value.source)) || !positive(value.seq) ||
      !timestamp(value.occurred_at) || !record(value.payload)) return false;
  const payload = value.payload;
  const codex = value.provider_id === 'codex';
  // Fake output is fixture-sourced; Codex output is provider-sourced; state is system.
  const outputSource = codex ? 'provider' : 'fixture';
  if (!codex && value.source === 'provider') return false;
  let valid = false;
  switch (value.kind) {
    case 'run_created':
      valid = shape(payload, ['state']) && payload.state === 'queued' && value.seq === 1 &&
        value.source === (codex ? 'system' : 'fixture');
      break;
    case 'run_state_changed':
      valid = shape(payload, ['from', 'to', 'error']) && fakeState(payload.from) && fakeState(payload.to) &&
        runTransitions[payload.from].includes(payload.to) &&
        (payload.to === 'failed' ? isExecutionError(payload.error) : payload.error === null);
      break;
    case 'text_delta':
      valid = value.source === outputSource && shape(payload, ['stream', 'text']) &&
        (payload.stream === 'assistant' || payload.stream === 'stderr') && text(payload.text) && payload.text.length > 0 &&
        utf8Bytes(payload.text) <= limits.text_delta_bytes;
      break;
    case 'tool_activity':
      valid = !codex && value.source === 'fixture' && shape(payload, ['tool_call_id', 'title', 'status']) &&
        boundedText(payload.tool_call_id, 1, 128) && boundedText(payload.title, 1, 256) &&
        ['started', 'completed', 'failed'].includes(String(payload.status));
      break;
    case 'usage_updated':
      valid = (codex ? value.source === 'provider' : value.source === 'fixture' || value.source === 'system') &&
        usage(payload);
      break;
    case 'output_truncated':
      valid = value.source === 'system' && shape(payload, ['scope', 'discarded_bytes', 'discarded_events']) &&
        positive(payload.discarded_events) &&
        (payload.scope === 'event_count' ? payload.discarded_bytes === null :
          (payload.scope === 'pending_bytes' || payload.scope === 'persisted_text_bytes') && positive(payload.discarded_bytes));
      break;
    case 'run_recovered':
      valid = value.source === 'system' && shape(payload, ['previous_state', 'state']) &&
        fakeState(payload.previous_state) && runTransitions[payload.previous_state].length > 0 && payload.state === 'interrupted';
      break;
  }
  return valid && utf8Bytes(JSON.stringify(value)) <=
    (value.kind === 'text_delta' ? limits.event_bytes : limits.nontext_event_bytes);
}

export function isEventPage(value: unknown): value is Wire.EventPage {
  if (!shape(value, ['contract_version', 'workspace_id', 'run_id', 'events', 'last_seq', 'through_seq', 'next_after_seq', 'has_more']) ||
      value.contract_version !== 2 || !isCanonicalId(value.workspace_id) || !isCanonicalId(value.run_id) ||
      !isCounter(value.last_seq) || !isCounter(value.through_seq) || !isCounter(value.next_after_seq) ||
      value.next_after_seq > value.through_seq || value.through_seq > value.last_seq ||
      typeof value.has_more !== 'boolean' || value.has_more !== (value.next_after_seq < value.through_seq) ||
      !Array.isArray(value.events) || value.events.length > limits.page_events || !value.events.every(isRunEvent)) return false;
  const events = value.events;
  const ids = new Set<string>();
  for (let index = 0; index < events.length; index += 1) {
    const event = events[index];
    if (event.workspace_id !== value.workspace_id || event.run_id !== value.run_id ||
        event.seq > value.through_seq || ids.has(event.event_id) ||
        (index > 0 && event.seq !== events[index - 1].seq + 1)) return false;
    ids.add(event.event_id);
  }
  return events.length === 0 || events[events.length - 1].seq === value.next_after_seq;
}

export function pageMatchesRequest(request: Wire.EventPageRequest, page: Wire.EventPage): boolean {
  if (page.workspace_id !== request.workspace_id || page.run_id !== request.run_id ||
      page.through_seq !== (request.through_seq ?? page.last_seq) || request.after_seq > page.through_seq ||
      page.events.length !== Math.min(request.limit, page.through_seq - request.after_seq)) return false;
  if (page.events.length === 0) {
    return request.after_seq === page.through_seq && page.next_after_seq === request.after_seq && !page.has_more;
  }
  return page.events[0].seq === request.after_seq + 1 &&
    page.next_after_seq === request.after_seq + page.events.length;
}

export function isExecutionChanged(value: unknown): value is Wire.ExecutionChanged {
  return shape(value, ['contract_version', 'workspace_id']) && value.contract_version === 2 && isCanonicalId(value.workspace_id);
}

function workspace(value: unknown): value is Wire.WorkspaceSnapshot {
  if (!shape(value, ['contract_version', 'workspace', 'sessions', 'runs', 'layout', 'layout_warning', 'execution_available', 'codex_availability']) ||
      !['not_configured', 'ready', 'quarantined', 'unsupported_platform'].includes(String(value.codex_availability)) ||
      value.contract_version !== 2 || !shape(value.workspace, ['workspace_id', 'label', 'created_at']) ||
      !isCanonicalId(value.workspace.workspace_id) || !label(value.workspace.label) || !timestamp(value.workspace.created_at) ||
      !Array.isArray(value.sessions) || value.sessions.length > limits.workspace_sessions || !value.sessions.every(session) ||
      !Array.isArray(value.runs) || value.runs.length > limits.workspace_runs || !value.runs.every(isRunSnapshot) ||
      !(value.layout === null || isLayout(value.layout)) ||
      !(value.layout_warning === null || isExecutionError(value.layout_warning)) ||
      typeof value.execution_available !== 'boolean') return false;
  const id = value.workspace.workspace_id;
  const sessions = new Set(value.sessions.map(item => item.session_id));
  const owners = new Map(value.sessions.map(item => [item.session_id, item.provider_id] as const));
  const runs = new Set(value.runs.map(item => item.run_id));
  const orders = new Set(value.runs.map(item => item.enqueue_order));
  const queued = value.runs.filter(item => item.state === 'queued').length;
  const active = value.runs.filter(item => item.state !== 'queued' && runTransitions[item.state].length > 0).length;
  return queued <= limits.queued_runs && active <= 1 &&
    sessions.size === value.sessions.length && runs.size === value.runs.length && orders.size === value.runs.length &&
    value.sessions.every(item => item.workspace_id === id) &&
    value.runs.every(item => item.workspace_id === id && owners.get(item.session_id) === item.provider_id) &&
    (value.layout === null || (value.layout.workspace_id === id && value.layout.panels.every(panel => sessions.has(panel.session_id)))) &&
    (value.layout !== null) !== (value.layout_warning !== null);
}

function bootstrapResponse(value: unknown): value is Wire.BootstrapResponse {
  return shape(value, ['contract_version', 'storage_state', 'upgrade_id', 'snapshot']) &&
    value.contract_version === 2 && value.storage_state === 'ready' && value.upgrade_id === null &&
    workspace(value.snapshot);
}

function runResponse(value: unknown): value is Wire.StartFakeRunResponse {
  return shape(value, ['contract_version', 'run', 'replayed']) && value.contract_version === 2 &&
    isRunSnapshot(value.run) && typeof value.replayed === 'boolean';
}

export function isSaveLayoutResponse(value: unknown): value is Wire.SaveLayoutResponse {
  return shape(value, ['contract_version', 'layout', 'replayed']) && value.contract_version === 2 &&
    isLayout(value.layout) && typeof value.replayed === 'boolean';
}

function mutationRequest(value: unknown, extra: readonly string[]): value is Record<string, unknown> {
  return shape(value, ['contract_version', 'workspace_id', 'request_id', ...extra]) &&
    value.contract_version === 2 && isCanonicalId(value.workspace_id) && isCanonicalId(value.request_id);
}

export function createExecutionClient(transport: Transport): Required<ExecutionClient> {
  async function call<T>(
    command: string,
    request: unknown,
    validateRequest: (value: unknown) => boolean,
    validateResult: (value: unknown) => value is T,
    relate: (value: T) => boolean = () => true
  ): Promise<T> {
    try {
      if (record(request) && typeof request.contract_version === 'number' && request.contract_version !== 2) {
        throw failure('unsupported_contract_version', 'This execution contract version is not supported.');
      }
      if (!validateRequest(request)) throw failure('validation_failed', 'The execution request is invalid.');
      if (command === 'start_fake_run' && record(request) && request.provider_id !== 'fake') {
        throw failure('provider_execution_disabled', 'This command starts simulated runs only.');
      }
      const result = await transport(command + '_v2', { request });
      if (record(result) && typeof result.contract_version === 'number' && result.contract_version !== 2) {
        throw failure('unsupported_contract_version', 'This execution contract version is not supported.');
      }
      if (!validateResult(result) || !relate(result)) {
        throw failure('invalid_execution_response', 'The execution response could not be validated.');
      }
      return result;
    } catch (error: unknown) {
      throw new ExecutionClientError(sanitizeExecutionError(error));
    }
  }

  return {
    bootstrap_workspace: request => call('bootstrap_workspace', request,
      value => shape(value, ['contract_version']) && value.contract_version === 2, bootstrapResponse)
      .then(response => response.snapshot as Wire.WorkspaceSnapshot),
    get_workspace_snapshot: request => call('get_workspace_snapshot', request,
      value => shape(value, ['contract_version', 'workspace_id']) && value.contract_version === 2 && isCanonicalId(value.workspace_id),
      workspace, result => result.workspace.workspace_id === request.workspace_id),
    preview_codex_context: request => call('preview_codex_context', request,
      value => shape(value, ['contract_version', 'workspace_id', 'session_id', 'prompt', 'context_run_ids']) &&
        value.contract_version === 2 && isCanonicalId(value.workspace_id) && isCanonicalId(value.session_id) &&
        text(value.prompt) && value.prompt.length > 0 && utf8Bytes(value.prompt) <= limits.codex_prompt_bytes &&
        !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(value.prompt) &&
        Array.isArray(value.context_run_ids) && value.context_run_ids.length >= 1 &&
        value.context_run_ids.length <= limits.codex_context_runs && value.context_run_ids.every(isCanonicalId) &&
        new Set(value.context_run_ids).size === value.context_run_ids.length,
      (value): value is Wire.CodexContextPreview => shape(value, ['contract_version', 'workspace_id', 'text', 'bytes', 'carried', 'trimmed']) &&
        value.contract_version === 2 && isCanonicalId(value.workspace_id) && typeof value.text === 'string' &&
        value.text.length > 0 && isCounter(value.bytes) && value.bytes === utf8Bytes(value.text) &&
        value.bytes <= limits.codex_context_bytes && isCounter(value.carried) && value.carried <= limits.codex_context_runs &&
        typeof value.trimmed === 'boolean',
      result => result.workspace_id === request.workspace_id),
    get_run_progress: request => call('get_run_progress', request,
      value => shape(value, ['contract_version', 'workspace_id']) && value.contract_version === 2 && isCanonicalId(value.workspace_id),
      (value): value is RunProgress => shape(value, ['contract_version', 'workspace_id', 'run_id', 'steps']) &&
        value.contract_version === 2 && isCanonicalId(value.workspace_id) &&
        (value.run_id === null || isCanonicalId(value.run_id)) &&
        Array.isArray(value.steps) && value.steps.length <= 64 &&
        (value.run_id !== null || value.steps.length === 0) &&
        value.steps.every(step => shape(step, ['step', 'count']) &&
          (progressSteps as readonly unknown[]).includes(step.step) && positive(step.count)),
      result => result.workspace_id === request.workspace_id),
    create_session: request => call('create_session', request,
      value => mutationRequest(value, ['label', 'provider_id']) && label(value.label) && provider(value.provider_id),
      (value): value is Wire.CreateSessionResponse => shape(value, ['contract_version', 'session', 'replayed']) &&
        value.contract_version === 2 && session(value.session) && typeof value.replayed === 'boolean',
      result => result.session.workspace_id === request.workspace_id),
    start_fake_run: request => call('start_fake_run', request,
      value => mutationRequest(value, ['session_id', 'provider_id', 'prompt', 'scenario']) &&
        isCanonicalId(value.session_id) && ['fake', 'codex', 'claude'].includes(String(value.provider_id)) &&
        text(value.prompt) && value.prompt.length > 0 && utf8Bytes(value.prompt) <= limits.prompt_bytes &&
        !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(value.prompt) &&
        ['success', 'failure', 'wait_for_cancel'].includes(String(value.scenario)),
      runResponse, result => result.run.workspace_id === request.workspace_id && result.run.session_id === request.session_id),
    start_codex_run: request => call('start_codex_run', request,
      value => mutationRequest(value, ['session_id', 'prompt', 'profile_id', 'confirmed',
        ...(record(value) && Object.prototype.hasOwnProperty.call(value, 'context_run_ids') ? ['context_run_ids'] : [])]) &&
        isCanonicalId(value.session_id) && contextShape(value.profile_id, value) && value.confirmed === true &&
        text(value.prompt) && value.prompt.length > 0 && utf8Bytes(value.prompt) <= limits.codex_prompt_bytes &&
        !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(value.prompt),
      (value): value is Wire.StartCodexRunResponse => runResponse(value) && value.run.provider_id === 'codex',
      result => result.run.workspace_id === request.workspace_id && result.run.session_id === request.session_id),
    cancel_run: request => call('cancel_run', request,
      value => mutationRequest(value, ['run_id']) && isCanonicalId(value.run_id),
      runResponse, result => result.run.workspace_id === request.workspace_id && result.run.run_id === request.run_id),
    list_run_events: request => call('list_run_events', request,
      value => shape(value, ['contract_version', 'workspace_id', 'run_id', 'after_seq', 'through_seq', 'limit']) &&
        value.contract_version === 2 && isCanonicalId(value.workspace_id) && isCanonicalId(value.run_id) &&
        isCounter(value.after_seq) && (value.through_seq === null || (isCounter(value.through_seq) && value.after_seq <= value.through_seq)) &&
        positive(value.limit) && value.limit <= limits.page_events,
      isEventPage, result => pageMatchesRequest(request, result)),
    save_layout: request => call('save_layout', request,
      value => mutationRequest(value, ['expected_revision', 'layout']) && isCounter(value.expected_revision) &&
        isLayout(value.layout) && value.layout.workspace_id === value.workspace_id && value.layout.revision === value.expected_revision,
      isSaveLayoutResponse, result => result.layout.workspace_id === request.workspace_id &&
        result.layout.revision === request.expected_revision + 1)
  };
}
