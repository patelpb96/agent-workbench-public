'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const root = path.resolve(__dirname, '../../..');
const compiled = path.join(root, 'artifacts/frontend-contract-src/execution');
const client = require(path.join(compiled, 'client.js'));
const reconcile = require(path.join(compiled, 'reconcile.js'));
const { limits } = require(path.join(compiled, 'generated.js'));
const { upgrade } = require('./v2-fixtures.cjs');
const fixture = name => upgrade(JSON.parse(fs.readFileSync(path.join(root, 'contracts/fixtures', name + '.json'), 'utf8')));
const clone = value => JSON.parse(JSON.stringify(value));
const success = fixture('fake-success');
const layout = fixture('layout');
const workspaceId = success.snapshot.workspace_id;
const sessionId = success.snapshot.session_id;
const requestId = 'aaaaaaaa-0000-4000-8000-000000000001';
const hint = { contract_version: 2, workspace_id: workspaceId };

function summary(last, state = 'running') {
  return { ...clone(success.snapshot), state, last_seq: last, terminal_error: null };
}

function stream(count, text = 'SIMULATED output') {
  const events = clone(success.events.slice(0, Math.min(3, count)));
  for (let seq = 4; seq <= count; seq += 1) {
    events.push({
      ...clone(success.events[3]), seq,
      event_id: '11111111-0000-4000-8000-' + String(seq).padStart(12, '0'),
      payload: { stream: 'assistant', text }
    });
  }
  return events;
}

function page(load, all, options = {}) {
  const request = load.request;
  const last = options.last_seq ?? all.length;
  const through = options.through_seq ?? request.through_seq ?? last;
  const limit = options.limit ?? request.limit;
  const events = all.filter(event => event.seq > request.after_seq && event.seq <= through).slice(0, limit);
  const next = events.length > 0 ? events[events.length - 1].seq : request.after_seq;
  return {
    contract_version: 2, workspace_id: request.workspace_id, run_id: request.run_id,
    events: clone(events), last_seq: last, through_seq: through, next_after_seq: next, has_more: next < through
  };
}

function loadAll(state, events, range = { after_seq: 0, through_seq: null, mode: 'replace' }) {
  let started = reconcile.beginLoad(state, range);
  state = started.state;
  let load = started.load;
  while (load !== null) {
    state = reconcile.applyPage(state, load, page(load, events));
    assert.equal(state.problem, null);
    load = reconcile.nextLoad(state);
  }
  return state;
}

function workspaceSnapshot() {
  return {
    contract_version: 2,
    workspace: { workspace_id: workspaceId, label: 'SIMULATED workspace', created_at: success.snapshot.created_at },
    sessions: [{ contract_version: 2, workspace_id: workspaceId, session_id: sessionId, provider_id: 'fake', label: 'SIMULATED session', created_at: success.snapshot.created_at }],
    runs: [clone(success.snapshot)], layout: clone(layout), layout_warning: null, execution_available: true,
    codex_availability: 'not_configured'
  };
}

// v2 bootstrap wire envelope around a workspace snapshot.
function boot(snapshot) {
  return { contract_version: 2, storage_state: 'ready', upgrade_id: null, snapshot };
}

function safeError(error, code) {
  assert.ok(error instanceof client.ExecutionClientError);
  assert.equal(error.code, code);
  assert.equal(error.stack, undefined);
  assert.deepEqual(Object.keys(error.error).sort(), ['code', 'recoverable', 'user_message']);
  return true;
}

test('fixture histories load explicitly; unknown usage stays null', () => {
  for (const name of ['fake-success', 'fake-failed', 'fake-cancelled', 'fake-recovered']) {
    const data = fixture(name);
    let state = reconcile.createTranscript(data.snapshot);
    assert.equal(state.summary_seq, data.snapshot.last_seq);
    assert.equal(state.applied_seq, 0);
    assert.equal(state.events.length, 0);
    assert.match(reconcile.omission(state).message, /not been loaded/);
    const started = reconcile.beginTail(state);
    state = reconcile.applyPage(started.state, started.load, page(started.load, data.events));
    assert.equal(state.problem, null);
    assert.deepEqual(state.events, data.events);
    assert.equal(state.applied_seq, data.snapshot.last_seq);
    assert.equal(state.summary.usage.input_tokens, null);
    assert.equal(state.summary.usage.output_tokens, null);
    assert.equal(reconcile.omission(state).message, null);
  }
});

test('duplicates compare identity and content; gaps preserve the last good viewport', () => {
  const started = reconcile.beginTail(reconcile.createTranscript(success.snapshot));
  const response = page(started.load, success.events);
  const good = reconcile.applyPage(started.state, started.load, response);
  assert.strictEqual(reconcile.applyPage(good, started.load, clone(response)), good);
  for (const change of [
    value => { value.events[3].payload.text = 'different content'; },
    value => { value.events[3].event_id = requestId; },
    value => { value.events[3].payload.stream = 'stderr'; }
  ]) {
    const conflict = clone(response);
    change(conflict);
    const failed = reconcile.applyPage(good, started.load, conflict);
    assert.equal(failed.problem.code, 'event_history_unavailable');
    assert.deepEqual(failed.events, good.events);
    assert.equal(failed.dirty, true);
  }
  const gap = clone(response);
  gap.events.splice(1, 1);
  const failed = reconcile.applyPage(started.state, started.load, gap);
  assert.equal(failed.applied_seq, 0);
  assert.equal(failed.events.length, 0);
  assert.equal(failed.problem.code, 'event_history_unavailable');
  const reusedId = clone(response);
  reusedId.events[4].event_id = reusedId.events[3].event_id;
  assert.equal(reconcile.applyPage(started.state, started.load, reusedId).problem.code, 'event_history_unavailable');
});

test('hint/snapshot races do not skip the first unseen event or clear a later hint', () => {
  let state = loadAll(reconcile.createTranscript(summary(3)), success.events.slice(0, 3));
  state = reconcile.beginReconcile(state);
  const started = reconcile.beginCatchUp(state);
  state = reconcile.markDirty(started.state, hint);
  for (let index = 0; index < 1000; index += 1) state = reconcile.markDirty(state, hint);
  state = reconcile.acceptSummary(state, success.snapshot);
  assert.equal(state.applied_seq, 3);
  assert.equal(state.summary_seq, 6);
  assert.equal(state.dirty, true);
  const firstLoad = { ...started.load, request: { ...started.load.request, limit: 2 } };
  state = reconcile.applyPage(state, firstLoad, page(firstLoad, success.events));
  assert.equal(state.events[3].seq, 4);
  assert.equal(state.applied_seq, 5);
  const next = reconcile.nextLoad(state);
  assert.equal(next.request.after_seq, 5);
  assert.equal(next.request.through_seq, 6);
  state = reconcile.applyPage(state, next, page(next, success.events));
  assert.deepEqual(state.events.map(event => event.seq), [1, 2, 3, 4, 5, 6]);
  assert.equal(state.dirty, true);
  assert.equal(Object.prototype.hasOwnProperty.call(state, 'hints'), false);
  assert.strictEqual(reconcile.markDirty(state, { ...hint, workspace_id: requestId }), state);
});

test('pagination remains pinned while newer summaries arrive', () => {
  const all = stream(450);
  const started = reconcile.beginLoad(reconcile.beginReconcile(reconcile.createTranscript(summary(450))), {
    after_seq: 0, through_seq: null, mode: 'replace'
  });
  let state = reconcile.applyPage(started.state, started.load, page(started.load, all));
  assert.equal(state.applied_seq, 200);
  state = reconcile.acceptSummary(state, summary(460));
  const next = reconcile.nextLoad(state);
  assert.equal(next.request.after_seq, 200);
  assert.equal(next.request.through_seq, 450);
  state = reconcile.applyPage(state, next, page(next, stream(460), { last_seq: 460 }));
  assert.equal(state.applied_seq, 400);
  assert.equal(state.summary_seq, 460);
  assert.equal(state.known_last_seq, 460);
  assert.equal(state.dirty, true);
  assert.equal(reconcile.nextLoad(state).request.through_seq, 450);
  assert.equal(reconcile.nextLoad(state).request.after_seq, 400);
});

test('event-count and UTF8 rendering bounds expose earlier bounded ranges', () => {
  let state = loadAll(reconcile.createTranscript(summary(500)), stream(500));
  assert.equal(state.events.length, limits.visible_events);
  assert.equal(state.events[0].seq, 301);
  let omitted = reconcile.omission(state);
  assert.deepEqual(omitted.before, { after_seq: 0, through_seq: 300 });
  assert.match(omitted.message, /Earlier history omitted/);
  const earlier = reconcile.beginEarlier(state);
  assert.equal(earlier.load.request.after_seq, 100);
  assert.equal(earlier.load.request.through_seq, 300);
  state = reconcile.applyPage(earlier.state, earlier.load, page(earlier.load, stream(500)));
  assert.equal(state.view, 'history');
  assert.equal(state.applied_seq, 300);
  assert.equal(state.summary_seq, 500);
  assert.equal(state.events[0].seq, 101);
  assert.deepEqual(reconcile.omission(state).after, { after_seq: 300, through_seq: 500 });
  assert.equal(reconcile.beginCatchUp(state).load.request.through_seq, 500);

  const unicode = '😀'.repeat(limits.text_delta_bytes / 4);
  assert.equal(client.utf8Bytes(unicode), limits.text_delta_bytes);
  state = loadAll(reconcile.createTranscript(summary(300)), stream(300, unicode));
  const bytes = state.events.reduce((sum, event) => sum + client.utf8Bytes(reconcile.eventText(event)), 0);
  assert.ok(bytes <= reconcile.renderedEventByteBudget);
  assert.ok(bytes < limits.frontend_text_bytes);
  assert.ok(state.events.length < 200);
  assert.equal(state.applied_seq, 300);
  omitted = reconcile.omission(state);
  assert.equal(omitted.before.through_seq, state.events[0].seq - 1);
  assert.match(omitted.message, /omitted/);
  assert.ok(!state.events.some(event => event.kind === 'text_delta' && event.payload.text.includes('\ufffd')));
});

test('superseded pages and stale immutable receipts cannot roll state/layout backward', () => {
  let state = loadAll(reconcile.createTranscript(success.snapshot), success.events);
  const old = reconcile.beginTail(state);
  const newer = reconcile.beginLoad(old.state, { after_seq: 0, through_seq: 3, mode: 'replace', view: 'history' });
  assert.strictEqual(reconcile.applyPage(newer.state, old.load, page(old.load, success.events)), newer.state);
  state = reconcile.acceptRunResult(state, { contract_version: 2, run: summary(2, 'starting'), replayed: true });
  assert.equal(state.summary.state, 'succeeded');
  assert.equal(state.summary_seq, 6);
  assert.equal(state.applied_seq, 6);
  assert.equal(state.dirty, true);
  const current = { layout: { ...clone(layout), revision: 9 }, warning: null, dirty: false };
  const merged = reconcile.acceptLayoutResult(current, { contract_version: 2, layout: clone(layout), replayed: true });
  assert.equal(merged.layout.revision, 9);
  const warning = { code: 'unsupported_layout_version', user_message: 'Unsupported stored layout.', recoverable: false };
  const blocked = reconcile.acceptLayoutResult({ layout: null, warning, dirty: false }, {
    contract_version: 2, layout: clone(layout), replayed: true
  });
  assert.equal(blocked.layout, null);
  assert.deepEqual(blocked.warning, warning);
});

test('unverifiable evicted duplicates request reconciliation without an unbounded cache', () => {
  const all = stream(500);
  const started = reconcile.beginLoad(reconcile.createTranscript(summary(500)), { after_seq: 0, through_seq: null, mode: 'replace' });
  const firstPage = page(started.load, all);
  let state = reconcile.applyPage(started.state, started.load, firstPage);
  while (reconcile.nextLoad(state)) {
    const next = reconcile.nextLoad(state);
    state = reconcile.applyPage(state, next, page(next, all));
  }
  const failed = reconcile.applyPage(state, started.load, firstPage);
  assert.equal(failed.problem.code, 'event_history_unavailable');
  assert.equal(failed.events.length, 200);
});

test('plain text rendering preserves markup as text and makes terminal controls visible', () => {
  const event = clone(success.events[3]);
  event.payload.text = '<script>secret()</script>\u001b[31m';
  const output = reconcile.eventText(event);
  assert.ok(output.startsWith('<script>secret()</script>'));
  assert.ok(output.includes('\\u001b[31m'));
  assert.ok(!output.includes('\u001b'));
});

test('all eight client methods invoke exactly command_v2 plus {request}', async () => {
  const session = workspaceSnapshot().sessions[0];
  const layoutRequest = { ...clone(layout), revision: 0 };
  const codexRun = { ...clone(success.snapshot), provider_id: 'codex', profile_id: 'codex_text_v1', model_id: 'gpt-6.1-sol' };
  const envelope = { contract_version: 2, storage_state: 'ready', upgrade_id: null, snapshot: workspaceSnapshot() };
  const cases = [
    ['bootstrap_workspace', { contract_version: 2 }, envelope, workspaceSnapshot()],
    ['create_session', { contract_version: 2, workspace_id: workspaceId, request_id: requestId, label: 'SIMULATED session', provider_id: 'fake' }, { contract_version: 2, session, replayed: false }],
    ['start_fake_run', { contract_version: 2, workspace_id: workspaceId, session_id: sessionId, request_id: requestId, provider_id: 'fake', prompt: 'SIMULATED prompt', scenario: 'success' }, { contract_version: 2, run: success.snapshot, replayed: false }],
    ['start_codex_run', { contract_version: 2, workspace_id: workspaceId, session_id: sessionId, request_id: requestId, prompt: 'Hello 😀', profile_id: 'codex_text_v1', confirmed: true }, { contract_version: 2, run: codexRun, replayed: false }],
    ['cancel_run', { contract_version: 2, workspace_id: workspaceId, run_id: success.snapshot.run_id, request_id: requestId }, { contract_version: 2, run: success.snapshot, replayed: true }],
    ['get_workspace_snapshot', { contract_version: 2, workspace_id: workspaceId }, workspaceSnapshot()],
    ['list_run_events', { contract_version: 2, workspace_id: workspaceId, run_id: success.snapshot.run_id, after_seq: 0, through_seq: null, limit: 200 }, null],
    ['save_layout', { contract_version: 2, workspace_id: workspaceId, request_id: requestId, expected_revision: 0, layout: layoutRequest }, { contract_version: 2, layout, replayed: true }]
  ];
  for (const [command, request, configured, expected] of cases) {
    const result = configured ?? page({ request }, success.events);
    let calls = 0;
    const api = client.createExecutionClient(async (actual, args) => {
      calls += 1;
      assert.equal(actual, command + '_v2');
      assert.deepEqual(Object.keys(args), ['request']);
      assert.strictEqual(args.request, request);
      return result;
    });
    assert.equal(calls, 0);
    assert.deepEqual(await api[command](request), expected ?? result);
    assert.equal(calls, 1);
  }
  // Codex consent and the 1024-byte profile limit are enforced before transport.
  let sent = 0;
  const guarded = client.createExecutionClient(async () => { sent += 1; return null; });
  const codex = cases[3][1];
  for (const bad of [{ ...codex, confirmed: false }, { ...codex, prompt: 'x'.repeat(1025) },
    { ...codex, prompt: '😀'.repeat(256) + 'x' }, { ...codex, profile_id: 'other' }, { ...codex, executable: 'C:/evil.exe' }]) {
    await assert.rejects(guarded.start_codex_run(bad), error => safeError(error, 'validation_failed'));
  }
  assert.equal(sent, 0);
  assert.deepEqual(client.availableProviders, ['fake', 'codex']);
  assert.equal(client.executionNotification, 'execution_changed_v2');
});

test('structured native errors survive; unknown errors expose no raw details or stack', async () => {
  const structured = { code: 'storage_busy', user_message: 'Storage is busy.', recoverable: true };
  const preserved = client.createExecutionClient(async () => { throw structured; });
  await assert.rejects(preserved.bootstrap_workspace({ contract_version: 2 }), error => {
    safeError(error, 'storage_busy');
    assert.deepEqual(error.error, structured);
    return true;
  });
  const raw = 'C:\\private\\token-secret.sqlite bearer=secret';
  for (const value of [raw, new Error(raw), null, { message: raw }, { ...structured, stack: raw },
    { code: '', user_message: raw, recoverable: true }, { code: 'bad', user_message: raw, recoverable: 'yes' }]) {
    const api = client.createExecutionClient(async () => { throw value; });
    await assert.rejects(api.bootstrap_workspace({ contract_version: 2 }), error => {
      safeError(error, 'execution_transport_error');
      assert.ok(!JSON.stringify(error).includes('secret'));
      assert.ok(!error.message.includes('private'));
      return true;
    });
  }
});

test('layout guard matches native cross-kind identity and parent-reference rules', () => {
  for (const parentReference of [false, true]) {
    const candidate = clone(layout);
    const panel = candidate.panels[0];
    candidate.containers.push({ container_id: panel.panel_id, geometry: clone(panel.geometry) });
    if (parentReference) panel.container_id = panel.panel_id;
    assert.equal(client.isLayout(candidate), false);
  }
  const candidate = clone(layout);
  const containerId = '00000000-0000-4000-8000-000000000008';
  candidate.containers.push({ container_id: containerId, geometry: clone(candidate.panels[0].geometry) });
  candidate.panels[0].container_id = containerId;
  assert.equal(client.isLayout(candidate), true);
});

test('native-accepted system usage pages reconcile without inventing known usage', async () => {
  const all = clone(success.events);
  all.find(event => event.kind === 'usage_updated').source = 'system';
  const request = { contract_version: 2, workspace_id: workspaceId, run_id: success.snapshot.run_id, after_seq: 0, through_seq: null, limit: 200 };
  const response = page({ request }, all);
  const reader = client.createExecutionClient(async () => response);
  const received = await reader.list_run_events(request);
  const started = reconcile.beginLoad(reconcile.createTranscript(success.snapshot), { after_seq: 0, through_seq: null, mode: 'replace' });
  const restored = reconcile.applyPage(started.state, started.load, received);
  assert.equal(restored.problem, null);
  assert.equal(restored.events.find(event => event.kind === 'usage_updated').source, 'system');
  assert.equal(restored.summary.usage.input_tokens, null);
  assert.equal(restored.summary.usage.output_tokens, null);
});

test('schema-valid Unicode structured errors preserve their codes and messages', () => {
  const detail = { code: 'fixture_unicode_error', user_message: '🧪'.repeat(2048), recoverable: true };
  const sanitized = client.sanitizeExecutionError(detail);
  assert.equal(sanitized.code, detail.code);
  assert.deepEqual(sanitized, detail);
  assert.equal(client.isExecutionError({ ...detail, user_message: '🧪'.repeat(2049) }), false);
});

test('a pinned page cannot silently return fewer committed events than requested', async () => {
  const request = { contract_version: 2, workspace_id: workspaceId, run_id: success.snapshot.run_id, after_seq: 0, through_seq: 6, limit: 200 };
  const shortened = page({ request }, success.events, { limit: 2 });
  assert.equal(client.isEventPage(shortened), true);
  assert.equal(client.pageMatchesRequest(request, shortened), false);
  const reader = client.createExecutionClient(async () => shortened);
  await assert.rejects(reader.list_run_events(request), error => safeError(error, 'invalid_execution_response'));
});

test('session/workspace labels enforce native character bounds and controls before transport', async () => {
  const invalid = ['', 'x'.repeat(129), '😀'.repeat(129), '\u0000', '\t', '\n', '\u001b', '\u007f', '\u0085', '\u009f'];
  for (const label of invalid) {
    let calls = 0;
    const api = client.createExecutionClient(async () => { calls++; return { contract_version: 2, session: workspaceSnapshot().sessions[0], replayed: false }; });
    await assert.rejects(api.create_session({ contract_version: 2, workspace_id: workspaceId, request_id: requestId, label, provider_id: 'fake' }),
      error => safeError(error, 'validation_failed'), JSON.stringify(label));
    assert.equal(calls, 0);
    for (const target of ['session', 'workspace']) {
      const value = workspaceSnapshot();
      (target === 'session' ? value.sessions[0] : value.workspace).label = label;
      const reader = client.createExecutionClient(async () => boot(value));
      await assert.rejects(reader.bootstrap_workspace({ contract_version: 2 }),
        error => safeError(error, 'invalid_execution_response'), `${target}: ${JSON.stringify(label)}`);
    }
  }
  // Rust chars() / schema code points, not JS UTF-16 units; do not trim labels.
  for (const label of ['x', ' ', 'x'.repeat(128), '😀'.repeat(128)]) {
    const value = workspaceSnapshot();
    value.workspace.label = label;
    value.sessions[0].label = label;
    const reader = client.createExecutionClient(async () => boot(value));
    assert.deepEqual(await reader.bootstrap_workspace({ contract_version: 2 }), value);
    const api = client.createExecutionClient(async () => ({ contract_version: 2, session: value.sessions[0], replayed: false }));
    assert.equal((await api.create_session({ contract_version: 2, workspace_id: workspaceId, request_id: requestId, label, provider_id: 'fake' })).session.label, label);
  }
});

test('workspace slot limits and preserved-layout warning are checked as native invariants', async () => {
  const run = (index, state) => ({
    ...summary(state === 'queued' ? 1 : 3, state),
    run_id: `22222222-0000-4000-8000-${String(index + 1).padStart(12, '0')}`,
    enqueue_order: index + 1
  });
  const warning = { code: 'layout_invalid', user_message: 'Stored layout is preserved.', recoverable: true };
  for (const change of [
    value => { value.runs = [run(0, 'running'), run(1, 'starting')]; },
    value => { value.runs = Array.from({ length: limits.queued_runs + 1 }, (_, index) => run(index, 'queued')); },
    value => { value.layout = null; value.layout_warning = null; },
    value => { value.layout_warning = warning; }
  ]) {
    const value = workspaceSnapshot(); change(value);
    const reader = client.createExecutionClient(async () => boot(value));
    await assert.rejects(reader.bootstrap_workspace({ contract_version: 2 }), error => safeError(error, 'invalid_execution_response'));
  }
  for (const change of [
    value => { value.layout = null; value.layout_warning = warning; },
    value => { value.runs = [...Array.from({ length: limits.queued_runs }, (_, index) => run(index, 'queued')), run(limits.queued_runs, 'running')]; }
  ]) {
    const value = workspaceSnapshot(); change(value);
    const reader = client.createExecutionClient(async () => boot(value));
    assert.deepEqual(await reader.bootstrap_workspace({ contract_version: 2 }), value);
  }
});

test('event text and tool strings enforce native empty/Unicode boundary matrices', () => {
  const text = clone(success.events.find(event => event.kind === 'text_delta'));
  assert.equal(client.isRunEvent({ ...text, payload: { ...text.payload, text: '' } }), false);
  const tool = { ...text, kind: 'tool_activity', payload: { tool_call_id: 'call-1', title: 'SIMULATED tool', status: 'started' } };
  for (const field of ['tool_call_id', 'title']) {
    const bound = field === 'tool_call_id' ? 128 : 256;
    for (const value of ['', 'x'.repeat(bound + 1), '😀'.repeat(bound + 1)]) {
      assert.equal(client.isRunEvent({ ...tool, payload: { ...tool.payload, [field]: value } }), false, `${field}:${value.length}`);
    }
    for (const value of ['x', 'x'.repeat(bound), '😀'.repeat(bound)]) {
      assert.equal(client.isRunEvent({ ...tool, payload: { ...tool.payload, [field]: value } }), true, `${field}:${value.length}`);
    }
  }
});

test('HTML entry is strict UTF-8 consistent with its declared charset', () => {
  const html = new TextDecoder('utf-8', { fatal: true }).decode(fs.readFileSync(path.join(root, 'frontend/index.html')));
  assert.match(html, /<meta charset="UTF-8">/);
});

test('versions, canonical identities, response ownership, required nulls and disabled providers are guarded', async () => {
  let calls = 0;
  const api = client.createExecutionClient(async () => { calls += 1; return workspaceSnapshot(); });
  await assert.rejects(api.bootstrap_workspace({ contract_version: 1 }), error => safeError(error, 'unsupported_contract_version'));
  await assert.rejects(api.get_workspace_snapshot({ contract_version: 2, workspace_id: requestId.toUpperCase() }), error => safeError(error, 'validation_failed'));
  const start = { contract_version: 2, workspace_id: workspaceId, session_id: sessionId, request_id: requestId, prompt: 'SIMULATED', scenario: 'success' };
  for (const provider_id of ['codex', 'claude']) {
    await assert.rejects(api.start_fake_run({ ...start, provider_id }), error => safeError(error, 'provider_execution_disabled'));
  }
  await assert.rejects(api.start_fake_run({ ...start, provider_id: 'unknown' }), error => safeError(error, 'validation_failed'));
  assert.equal(calls, 0);
  for (const mutate of [
    value => { value.contract_version = 1; },
    value => { value.runs[0].run_id = 'not-an-id'; },
    value => { delete value.runs[0].terminal_error; },
    value => { value.workspace.workspace_id = requestId; },
    value => { value.sessions[0].provider_id = 'claude'; }
  ]) {
    const result = workspaceSnapshot();
    mutate(result);
    const reader = client.createExecutionClient(async () => result);
    await assert.rejects(reader.get_workspace_snapshot({ contract_version: 2, workspace_id: workspaceId }), error => {
      return safeError(error, result.contract_version === 1 ? 'unsupported_contract_version' : 'invalid_execution_response');
    });
  }
  const request = { contract_version: 2, workspace_id: workspaceId, run_id: success.snapshot.run_id, after_seq: 0, through_seq: null, limit: 200 };
  const invalidPage = page({ request }, success.events);
  invalidPage.events.splice(1, 1);
  const reader = client.createExecutionClient(async () => invalidPage);
  await assert.rejects(reader.list_run_events(request), error => safeError(error, 'invalid_execution_response'));
});
