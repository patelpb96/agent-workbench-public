'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const root = path.resolve(__dirname, '../../..');
const compiled = path.join(root, 'artifacts/frontend-contract-src/execution');
const { createWorkspaceController } = require(path.join(compiled, 'controller.js'));
const { createLayoutManager, updateWorkspaceChrome } = require(path.join(compiled, 'layout.js'));
const client = require(path.join(compiled, 'client.js'));
const reconcile = require(path.join(compiled, 'reconcile.js'));
const { limits } = require(path.join(compiled, 'generated.js'));
const { upgrade } = require('./v2-fixtures.cjs');
const fixture = name => upgrade(JSON.parse(fs.readFileSync(path.join(root, 'contracts/fixtures', name + '.json'), 'utf8')));
const clone = value => JSON.parse(JSON.stringify(value));
const success = fixture('fake-success');
const cancelled = fixture('fake-cancelled');
const layoutFixture = fixture('layout');
const W = success.snapshot.workspace_id;
const S = success.snapshot.session_id;
const R = success.snapshot.run_id;
const id = n => 'aaaaaaaa-0000-4000-8000-' + n.toString(16).padStart(12, '0');
const hint = { contract_version: 2, workspace_id: W };
const readCommands = new Set(['bootstrap_workspace', 'get_workspace_snapshot', 'list_run_events']);

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// A fixed microtask budget, never a sleep or an event-loop-until-idle loop.
async function drain() {
  for (let index = 0; index < 80; index++) await Promise.resolve();
}

async function finish(promise) {
  let done = false, result, failure;
  promise.then(value => { done = true; result = value; }, error => { done = true; failure = error; });
  await drain();
  assert.equal(done, true, 'operation must settle within the bounded microtask drain');
  if (failure !== undefined) throw failure;
  return result;
}

class FakeTimer {
  constructor() { this.now = 0; this.serial = 0; this.jobs = new Map(); }
  add(callback, ms, interval) {
    assert.equal(typeof callback, 'function');
    assert.ok(Number.isFinite(ms) && ms >= 0);
    if (interval) assert.ok(ms > 0);
    const handle = ++this.serial;
    this.jobs.set(handle, { callback, ms, interval, at: this.now + ms });
    return handle;
  }
  setTimeout(callback, ms) { return this.add(callback, ms, false); }
  setInterval(callback, ms) { return this.add(callback, ms, true); }
  clearTimeout(handle) { this.jobs.delete(handle); }
  clearInterval(handle) { this.jobs.delete(handle); }
  count(interval) { return [...this.jobs.values()].filter(job => job.interval === interval).length; }
  runReady() {
    // Only jobs present at entry run. A newly deferred pump requires another explicit turn.
    const ready = [...this.jobs.entries()].filter(([, job]) => job.at <= this.now);
    for (const [handle, job] of ready) {
      if (this.jobs.get(handle) !== job) continue;
      if (job.interval) job.at = this.now + job.ms;
      else this.jobs.delete(handle);
      job.callback();
    }
  }
  advance(ms) { assert.ok(Number.isFinite(ms) && ms >= 0); this.now += ms; this.runReady(); }
}

function snapshot(run = success.snapshot) {
  return {
    contract_version: 2,
    workspace: { workspace_id: W, label: 'SIMULATED workspace', created_at: success.snapshot.created_at },
    sessions: [{ contract_version: 2, workspace_id: W, session_id: S, provider_id: 'fake', label: 'SIMULATED session', created_at: success.snapshot.created_at }],
    runs: run === null ? [] : [clone(run)],
    layout: clone(layoutFixture), layout_warning: null, execution_available: true, codex_availability: 'not_configured'
  };
}

function summary(last, state = 'running') {
  return { ...clone(success.snapshot), last_seq: last, state, terminal_error: null };
}

function stream(count, text = 'SIMULATED output') {
  const events = clone(success.events.slice(0, Math.min(count, 3)));
  for (let seq = 4; seq <= count; seq++) events.push({
    ...clone(success.events[3]), seq, event_id: id(10000 + seq), payload: { stream: 'assistant', text }
  });
  return events;
}

function page(request, events, last = events.length) {
  const through = request.through_seq ?? last;
  const selected = events.filter(event => event.run_id === request.run_id && event.seq > request.after_seq && event.seq <= through).slice(0, request.limit);
  const next = selected.length ? selected[selected.length - 1].seq : request.after_seq;
  return {
    contract_version: 2, workspace_id: request.workspace_id, run_id: request.run_id,
    events: clone(selected), last_seq: last, through_seq: through,
    next_after_seq: next, has_more: next < through
  };
}

function safe(code) {
  return error => {
    assert.ok(error instanceof client.ExecutionClientError);
    assert.equal(error.code, code);
    assert.equal(error.stack, undefined);
    assert.deepEqual(Object.keys(error.error).sort(), ['code', 'recoverable', 'user_message']);
    return true;
  };
}

function harness(t, options = {}) {
  const timer = new FakeTimer();
  const calls = [], subscriptions = [];
  let serial = 100, maxReads = 0, maxMutations = 0;
  const nextId = options.nextId ?? (() => id(++serial));
  const layout = createLayoutManager(nextId);
  const api = client.createExecutionClient((command, args) => {
    assert.deepEqual(Object.keys(args), ['request']);
    const d = deferred();
    // Native receives v2 command names; the harness tracks logical names.
    assert.match(command, /_v2$/);
    const call = { command: command.replace(/_v2$/, ''), request: clone(args.request), reference: args.request, d, claimed: false, settled: false };
    calls.push(call);
    const pending = calls.filter(item => !item.settled);
    maxReads = Math.max(maxReads, pending.filter(item => readCommands.has(item.command)).length);
    maxMutations = Math.max(maxMutations, pending.filter(item => !readCommands.has(item.command)).length);
    assert.ok(maxReads <= 1, 'one native read at a time');
    assert.ok(maxMutations <= 1, 'one native mutation at a time');
    return d.promise;
  });
  const c = createWorkspaceController({
    client: api, timer, layout, nextId,
    subscribe: callback => {
      const d = deferred();
      subscriptions.push({ callback, d, unlistens: 0 });
      return d.promise;
    },
    onChange: options.onChange
  });
  const h = {
    c, layout, timer, calls, subscriptions,
    take(command) {
      const call = calls.find(item => item.command === command && !item.claimed && !item.settled);
      assert.ok(call, 'expected pending ' + command);
      call.claimed = true;
      return call;
    },
    reply(call, value) {
      assert.equal(call.settled, false); call.settled = true;
      // v2 bootstrap answers with its envelope; tests supply the snapshot.
      const wire = call.command === 'bootstrap_workspace' && value && value.workspace
        ? { contract_version: 2, storage_state: 'ready', upgrade_id: null, snapshot: value } : value;
      call.d.resolve(clone(wire));
    },
    fail(call, error) { assert.equal(call.settled, false); call.settled = true; call.d.reject(error); },
    listen(index = subscriptions.length - 1) {
      const sub = subscriptions[index];
      assert.ok(sub);
      sub.d.resolve(() => { sub.unlistens++; });
    },
    hint(value = hint, index = subscriptions.length - 1) { subscriptions[index].callback(value); },
    count(command) { return calls.filter(item => item.command === command).length; },
    mutations() { return calls.filter(item => !readCommands.has(item.command)); },
    async turn() {
      timer.runReady();
      await drain();
      assert.ok(timer.count(false) <= 1, 'one deferred pump');
      assert.ok(timer.count(true) <= 1, 'one visible poll');
    },
    async init(value = snapshot(), events = success.events) {
      const started = c.start();
      assert.equal(subscriptions.length, 1);
      assert.equal(calls.length, 0);
      h.listen();
      await drain();
      h.reply(h.take('bootstrap_workspace'), value);
      await drain();
      if (value.runs.length && c.getState().visible) {
        const load = h.take('list_run_events');
        h.reply(load, page(load.request, events));
      }
      await finish(started);
      assert.equal(c.getState().in_flight, false);
      return h;
    }
  };
  t.after(() => c.stop());
  return h;
}

test('listener precedes bootstrap; early and in-flight hints coalesce without losing a later wake', async t => {
  const h = harness(t);
  const started = h.c.start();
  for (let index = 0; index < 128; index++) h.hint();
  h.timer.runReady();
  await drain();
  assert.equal(h.calls.length, 0);
  assert.equal(h.timer.count(false), 0);
  h.listen();
  await drain();
  const bootstrap = h.take('bootstrap_workspace');
  assert.deepEqual(bootstrap.request, { contract_version: 2 });
  for (let index = 0; index < 128; index++) h.hint();
  h.reply(bootstrap, snapshot());
  await drain();
  const load = h.take('list_run_events');
  assert.deepEqual(load.request, { contract_version: 2, workspace_id: W, run_id: R, after_seq: 0, through_seq: 6, limit: limits.page_events });
  for (let index = 0; index < 128; index++) h.hint();
  await h.turn();
  assert.equal(h.count('get_workspace_snapshot'), 0);
  assert.equal(h.c.getState().dirty, true);
  h.reply(load, page(load.request, success.events));
  await finish(started);
  assert.equal(h.timer.count(false), 1);
  await h.turn();
  const refresh = h.take('get_workspace_snapshot');
  assert.deepEqual(refresh.request, { contract_version: 2, workspace_id: W });
  h.hint();
  h.reply(refresh, snapshot());
  await drain();
  assert.equal(h.c.getState().dirty, true);
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  assert.equal(h.c.getState().dirty, false);
  assert.equal(h.timer.count(false), 0);
  const count = h.calls.length;
  for (const value of [null, {}, { ...hint, contract_version: 1 }, { ...hint, workspace_id: id(999) }]) h.hint(value);
  await h.turn();
  assert.equal(h.calls.length, count);
  assert.deepEqual(h.c.getState().transcript.events, success.events);
  assert.equal(h.mutations().length, 0);
});

test('lost terminal hint is caught by the 2000ms poll; hidden windows pause reads and restore immediately', async t => {
  const h = harness(t);
  await h.init(snapshot(summary(3)), success.events.slice(0, 3));
  assert.deepEqual([...h.timer.jobs.values()].filter(job => job.interval).map(job => job.ms), [2000]);
  h.timer.advance(1999);
  await drain();
  assert.equal(h.count('get_workspace_snapshot'), 0);
  h.timer.advance(1);
  await drain();
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  const load = h.take('list_run_events');
  assert.equal(load.request.after_seq, 3);
  assert.equal(load.request.through_seq, null);
  h.reply(load, page(load.request, success.events));
  await drain();
  assert.equal(h.c.getState().snapshot.runs[0].state, 'succeeded');
  assert.equal(h.c.getState().transcript.applied_seq, 6);
  assert.deepEqual(h.c.getState().transcript.summary.usage, { input_tokens: null, output_tokens: null, estimated: false, cached_input_tokens: null, cache_write_input_tokens: null, reasoning_output_tokens: null });
  h.c.setVisibility(false);
  const count = h.calls.length;
  h.hint();
  h.c.onFocus();
  h.timer.advance(10000);
  await h.turn();
  assert.equal(h.calls.length, count);
  assert.equal(h.timer.jobs.size, 0);
  h.c.setVisibility(true);
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  h.c.onFocus();
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  assert.equal(h.c.getState().status, 'ready');
  assert.equal(h.mutations().length, 0);
});

test('hidden startup may read its initial snapshot, but does not fetch a page until visible again', async t => {
  const h = harness(t);
  h.c.setVisibility(false);
  await h.init();
  assert.equal(h.count('bootstrap_workspace'), 1);
  assert.equal(h.count('list_run_events'), 0);
  assert.equal(h.timer.jobs.size, 0);
  h.c.setVisibility(true);
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  const load = h.take('list_run_events');
  h.c.setVisibility(false);
  h.reply(load, page(load.request, success.events));
  await drain();
  const count = h.calls.length;
  h.hint();
  await h.turn();
  assert.equal(h.calls.length, count);
});

test('failed subscription is sanitized and retried once on poll/focus; late unlisten tears down after stop', async t => {
  const h = harness(t);
  const started = h.c.start();
  h.subscriptions[0].d.reject(new Error('private transport detail'));
  await drain();
  h.reply(h.take('bootstrap_workspace'), snapshot(null));
  await finish(started);
  assert.equal(h.c.getState().listener_ready, false);
  assert.equal(h.c.getState().last_error.code, 'execution_transport_error');
  assert.equal(JSON.stringify(h.c.getState()).includes('private transport detail'), false);
  h.timer.advance(2000);
  await drain();
  assert.equal(h.subscriptions.length, 2);
  for (let index = 0; index < 32; index++) h.c.onFocus();
  assert.equal(h.subscriptions.length, 2);
  await h.turn();
  const read = h.take('get_workspace_snapshot');
  h.c.stop();
  const stopped = h.c.getState();
  h.listen(1);
  h.reply(read, snapshot(null));
  h.hint(hint, 0);
  h.hint(hint, 1);
  await drain();
  assert.equal(h.subscriptions[1].unlistens, 1);
  assert.deepEqual(h.c.getState(), stopped);
  assert.equal(h.timer.jobs.size, 0);
  h.c.stop();
  assert.equal(h.subscriptions[1].unlistens, 1);
});

for (const phase of ['bootstrap', 'snapshot', 'page']) {
  test('stop/restart ignores stale ' + phase + ' replies and callbacks without overlapping reads', async t => {
    const h = harness(t);
    let firstStart, old;
    if (phase === 'bootstrap') {
      firstStart = h.c.start();
      h.listen();
      await drain();
      old = h.take('bootstrap_workspace');
    } else {
      await h.init();
      if (phase === 'snapshot') h.c.reconcile();
      else h.c.returnToLive();
      await h.turn();
      old = h.take(phase === 'snapshot' ? 'get_workspace_snapshot' : 'list_run_events');
    }
    h.c.stop();
    assert.equal(h.subscriptions[0].unlistens, 1);
    const restarted = h.c.start();
    h.listen(1);
    await drain();
    const before = h.calls.length;
    h.hint(hint, 0);
    if (phase === 'page') h.reply(old, page(old.request, success.events));
    else {
      const stale = snapshot(summary(2, 'starting'));
      stale.workspace.label = 'STALE response';
      stale.execution_available = false;
      h.reply(old, stale);
    }
    await drain();
    assert.equal(h.calls.length, before);
    assert.equal(h.c.getState().transcript, null);
    assert.notEqual(h.c.getState().snapshot?.workspace.label, 'STALE response');
    await h.turn();
    const fresh = h.take(phase === 'bootstrap' ? 'bootstrap_workspace' : 'get_workspace_snapshot');
    h.reply(fresh, snapshot());
    await drain();
    const load = h.take('list_run_events');
    h.reply(load, page(load.request, success.events));
    await finish(restarted);
    if (firstStart) await finish(firstStart);
    await drain();
    assert.equal(h.c.getState().status, 'ready');
    assert.deepEqual(h.c.getState().transcript.events, success.events);
    assert.equal(h.c.getState().snapshot.runs[0].last_seq, 6);
  });
}

test('restart waits for a pending old subscription and immediately disposes its late cleanup', async t => {
  const h = harness(t);
  const first = h.c.start();
  h.c.stop();
  const second = h.c.start();
  await drain();
  assert.equal(h.subscriptions.length, 1);
  assert.equal(h.calls.length, 0);
  h.listen(0);
  await drain();
  assert.equal(h.subscriptions[0].unlistens, 1);
  assert.equal(h.subscriptions.length, 2);
  assert.equal(h.calls.length, 0);
  h.listen(1);
  await drain();
  h.reply(h.take('bootstrap_workspace'), snapshot(null));
  await finish(first);
  await finish(second);
  await drain();
  assert.equal(h.c.getState().status, 'ready');
});

test('selected history is bounded, survives refresh, and superseded pages cannot replace the selected viewport', async t => {
  const h = harness(t);
  const all = stream(550);
  await h.init(snapshot(summary(500)), all.slice(0, 500));
  let state = h.c.getState();
  assert.equal(state.transcript.events.length, limits.visible_events);
  assert.equal(state.transcript.events[0].seq, 301);
  assert.equal(reconcile.omission(state.transcript).before.through_seq, 300);
  h.c.loadEarlier();
  await h.turn();
  let load = h.take('list_run_events');
  assert.equal(load.request.after_seq, 100);
  assert.equal(load.request.through_seq, 300);
  h.reply(load, page(load.request, all.slice(0, 500)));
  await drain();
  state = h.c.getState();
  assert.equal(state.transcript.view, 'history');
  assert.equal(state.transcript.events[0].seq, 101);
  assert.equal(state.transcript.applied_seq, 300);
  const history = clone(state.transcript.events);
  h.c.reconcile();
  await h.turn();
  const refreshed = snapshot(summary(550));
  refreshed.runs.push(clone(cancelled.snapshot));
  h.reply(h.take('get_workspace_snapshot'), refreshed);
  await drain();
  state = h.c.getState();
  assert.equal(state.transcript.summary_seq, 550);
  assert.equal(state.transcript.view, 'history');
  assert.deepEqual(state.transcript.events, history);
  assert.equal(state.selected_run_by_session[S], R);
  assert.equal(state.snapshot.runs.length, 2);
  h.c.loadEarlier();
  await h.turn();
  const superseded = h.take('list_run_events');
  h.c.returnToLive();
  const generation = h.c.getState().transcript.generation;
  h.reply(superseded, page(superseded.request, all));
  await drain();
  assert.equal(h.c.getState().transcript.generation, generation);
  assert.deepEqual(h.c.getState().transcript.events, history);
  await h.turn();
  load = h.take('list_run_events');
  assert.equal(load.request.after_seq, 350);
  assert.equal(load.request.through_seq, 550);
  h.reply(load, page(load.request, all));
  await drain();
  assert.equal(h.c.getState().transcript.events[0].seq, 351);
  h.c.returnToLive();
  await h.turn();
  const oldRunPage = h.take('list_run_events');
  h.c.selectRun(S, cancelled.snapshot.run_id);
  assert.deepEqual(h.c.getState().transcript.events, []);
  h.reply(oldRunPage, page(oldRunPage.request, all));
  await drain();
  assert.equal(h.c.getState().transcript.summary.run_id, cancelled.snapshot.run_id);
  assert.deepEqual(h.c.getState().transcript.events, []);
  await h.turn();
  load = h.take('list_run_events');
  h.reply(load, page(load.request, cancelled.events));
  await drain();
  assert.deepEqual(h.c.getState().transcript.events, cancelled.events);
  h.c.selectRun(S, R);
  assert.deepEqual(h.c.getState().transcript.events, [], 'switching runs discards rather than caches the previous buffer');
  h.c.selectSession(S);
  assert.equal(h.c.getState().selected_run_by_session[S], R);
  assert.equal(h.mutations().length, 0, 'selection/history controls never dispatch work');
});

test('large UTF8 transcript remains one bounded buffer and unknown usage is never replaced with zero', async t => {
  const h = harness(t);
  const events = stream(300, '😀'.repeat(limits.text_delta_bytes / 4));
  await h.init(snapshot(summary(300)), events);
  const state = h.c.getState();
  assert.ok(state.transcript.events.length < limits.visible_events);
  assert.ok(state.transcript.events.reduce((total, event) => total + client.utf8Bytes(reconcile.eventText(event)), 0) <= reconcile.renderedEventByteBudget);
  assert.equal(state.transcript.applied_seq, 300);
  assert.ok(reconcile.omission(state.transcript).before !== null);
  assert.equal(state.snapshot.runs[0].usage.input_tokens, null);
  assert.equal(state.snapshot.runs[0].usage.output_tokens, null);
  assert.equal(state.transcript.summary.provider_id, 'fake');
});

test('stale reads and immutable mutation receipts preserve newer summaries, sessions and layout intent', async t => {
  const h = harness(t);
  await h.init();
  const draft = updateWorkspaceChrome(h.c.getState().layout.draft, { theme: 'light' });
  h.c.editLayout(draft);
  h.c.reconcile();
  await h.turn();
  const oldRead = h.take('get_workspace_snapshot');
  const created = h.c.createSession('New SIMULATED session');
  const create = h.take('create_session');
  const newSession = { ...snapshot().sessions[0], session_id: id(201), label: create.request.label };
  h.reply(create, { contract_version: 2, session: newSession, replayed: true });
  await finish(created);
  const stale = snapshot(summary(2, 'starting'));
  h.reply(oldRead, stale);
  await drain();
  let state = h.c.getState();
  assert.equal(state.snapshot.sessions.length, 2);
  assert.equal(state.snapshot.runs[0].state, 'succeeded');
  assert.equal(state.snapshot.runs[0].last_seq, 6);
  assert.equal(state.transcript.applied_seq, 6);
  assert.equal(state.layout.draft.theme, 'light');
  assert.equal(state.layout.dirty, true);
  const cancel = h.c.cancelRun(R);
  h.reply(h.take('cancel_run'), { contract_version: 2, run: summary(1, 'queued'), replayed: true });
  await finish(cancel);
  state = h.c.getState();
  assert.equal(state.snapshot.runs[0].last_seq, 6);
  assert.equal(state.transcript.summary.state, 'succeeded');
  assert.equal(state.layout.draft.theme, 'light');
});

const mutationCases = [
  { command: 'create_session', key: 'create_session', invoke: (c, different = false) => c.createSession(different ? 'different label' : 'New SIMULATED session'), result: request => ({ contract_version: 2, session: { ...snapshot().sessions[0], session_id: id(202), label: request.label }, replayed: true }) },
  { command: 'start_fake_run', key: '😀'.repeat(64), invoke: (c, different = false) => c.startFakeRun(S, different ? 'different prompt' : ' private SIMULATED prompt\n', 'wait_for_cancel', '😀'.repeat(64)), result: () => ({ contract_version: 2, run: { ...summary(1, 'queued'), run_id: id(302), enqueue_order: 4 }, replayed: true }) },
  { command: 'cancel_run', key: 'cancel_run', invoke: (c, different = false) => c.cancelRun(different ? cancelled.snapshot.run_id : R), result: () => ({ contract_version: 2, run: summary(2, 'starting'), replayed: true }) }
];

for (const entry of mutationCases) {
  test(entry.command + ' uncertain retry keeps the entire DTO and ID; polls never repeat mutations', async t => {
    const h = harness(t);
    const value = snapshot();
    value.runs.push(clone(cancelled.snapshot));
    // Keep startup on the success run for a predictable fixture page.
    value.runs[0].enqueue_order = 3;
    await h.init(value);
    const rejected = assert.rejects(entry.invoke(h.c), safe('execution_transport_error'));
    const first = h.take(entry.command);
    const original = clone(first.request);
    assert.equal(client.isCanonicalId(original.request_id), true);
    assert.equal(original.contract_version, 2);
    assert.equal(original.workspace_id, W);
    if (entry.command === 'start_fake_run') assert.deepEqual(original, {
      contract_version: 2, workspace_id: W, request_id: original.request_id,
      session_id: S, provider_id: 'fake', prompt: ' private SIMULATED prompt\n', scenario: 'wait_for_cancel'
    });
    if (entry.command === 'create_session') assert.deepEqual(original, {
      contract_version: 2, workspace_id: W, request_id: original.request_id, label: 'New SIMULATED session', provider_id: 'fake'
    });
    if (entry.command === 'cancel_run') assert.deepEqual(original, {
      contract_version: 2, workspace_id: W, request_id: original.request_id, run_id: R
    });
    await finish(assert.rejects(h.c.createSession('Concurrent', 'other'), safe('state_conflict')));
    assert.equal(h.mutations().length, 1);
    // The injected transport must not be able to alter the retained retry DTO.
    first.reference.request_id = id(9999);
    if ('prompt' in first.reference) first.reference.prompt = 'transport corruption';
    h.fail(first, new Error('private transport exception'));
    await finish(rejected);
    const metadata = h.c.getState().attempts;
    assert.deepEqual(metadata, [{ key: entry.key, command: entry.command, request_id: original.request_id, pending: false }]);
    assert.equal(JSON.stringify(metadata).includes('prompt'), false);
    await finish(assert.rejects(entry.invoke(h.c, true), safe('request_id_conflict')));
    await finish(assert.rejects(h.c.createSession('other command', entry.key), safe('request_id_conflict')));
    await h.turn();
    h.reply(h.take('get_workspace_snapshot'), value);
    await drain();
    h.timer.advance(2000);
    await h.turn();
    h.reply(h.take('get_workspace_snapshot'), value);
    await drain();
    assert.equal(h.mutations().length, 1);
    const retry = h.c.retryMutation(entry.key);
    const second = h.take(entry.command);
    assert.deepEqual(second.request, original);
    const result = entry.result(original);
    if (entry.command === 'cancel_run') result.run.enqueue_order = 3;
    if (entry.command === 'start_fake_run') {
      assert.ok(result.run.enqueue_order > Math.max(...value.runs.map(run => run.enqueue_order)),
        'a new mock run must have a unique increasing enqueue order');
    }
    h.reply(second, result);
    await finish(retry);
    assert.deepEqual(h.c.getState().attempts, []);
    assert.equal(h.c.getState().mutation_pending, false);
  });
}

test('retry entries and keys are bounded; known pre-write failures release a slot', async t => {
  const h = harness(t);
  await h.init(snapshot(null));
  for (const key of ['', 'x'.repeat(65), '😀'.repeat(65), null]) {
    await finish(assert.rejects(h.c.createSession('SIMULATED', key), safe('validation_failed')));
  }
  assert.equal(h.mutations().length, 0);
  for (let index = 0; index < 8; index++) {
    const rejected = assert.rejects(h.c.createSession('SIMULATED ' + index, 'key-' + index), safe('storage_busy'));
    h.fail(h.take('create_session'), { code: 'storage_busy', user_message: 'Storage is busy.', recoverable: true });
    await finish(rejected);
    assert.equal(h.c.getState().attempts.length, index + 1);
  }
  await finish(assert.rejects(h.c.createSession('overflow', 'ninth'), safe('capacity_exceeded')));
  assert.equal(h.mutations().length, 8);
  const rejected = assert.rejects(h.c.retryMutation('key-0'), safe('validation_failed'));
  const retry = h.take('create_session');
  assert.equal(retry.request.request_id, h.calls.find(call => call.command === 'create_session').request.request_id);
  h.fail(retry, { code: 'validation_failed', user_message: 'Rejected before writing.', recoverable: true });
  await finish(rejected);
  assert.equal(h.c.getState().attempts.length, 7);
  const replacement = assert.rejects(h.c.createSession('replacement', 'ninth'), safe('storage_unavailable'));
  h.fail(h.take('create_session'), { code: 'storage_unavailable', user_message: 'Storage unavailable.', recoverable: true });
  await finish(replacement);
  assert.equal(h.c.getState().attempts.length, 8);
  await finish(assert.rejects(h.c.retryMutation('missing'), safe('state_conflict')));
});

test('invalid injected request IDs are rejected before mutation transport', async t => {
  const h = harness(t, { nextId: () => 'NOT-A-CANONICAL-ID' });
  await h.init(snapshot(null));
  await finish(assert.rejects(h.c.createSession('SIMULATED'), safe('validation_failed')));
  assert.equal(h.mutations().length, 0);
  assert.deepEqual(h.c.getState().attempts, []);
});

test('late mutation acknowledgement after restart cannot merge state; explicit retry keeps its original DTO', async t => {
  const h = harness(t);
  await h.init();
  const rejected = assert.rejects(h.c.createSession('Late SIMULATED session', 'late'), safe('state_conflict'));
  const old = h.take('create_session');
  h.c.stop();
  const restarted = h.c.start();
  h.listen(1);
  await drain();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  const load = h.take('list_run_events');
  h.reply(load, page(load.request, success.events));
  await finish(restarted);
  await finish(assert.rejects(h.c.retryMutation('late'), safe('state_conflict')));
  const response = { contract_version: 2, session: { ...snapshot().sessions[0], session_id: id(203), label: old.request.label }, replayed: true };
  h.reply(old, response);
  await finish(rejected);
  assert.equal(h.c.getState().snapshot.sessions.length, 1);
  assert.equal(h.c.getState().attempts[0].pending, false);
  const retried = h.c.retryMutation('late');
  const retry = h.take('create_session');
  assert.deepEqual(retry.request, old.request);
  h.reply(retry, response);
  await finish(retried);
  assert.equal(h.c.getState().snapshot.sessions.length, 2);
});

test('layout save delegates exact retry, edit epochs and refreshed explicit reapply to the actual manager', async t => {
  const h = harness(t);
  await h.init();
  h.c.editLayout(updateWorkspaceChrome(h.c.getState().layout.draft, { theme: 'light' }));
  const failed = assert.rejects(h.c.saveLayout(), safe('execution_transport_error'));
  const first = h.take('save_layout');
  h.c.editLayout(updateWorkspaceChrome(h.c.getState().layout.draft, { theme: 'contrast' }));
  h.fail(first, new Error('private save failure'));
  await finish(failed);
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  assert.equal(h.count('save_layout'), 1);
  const saved = h.c.saveLayout();
  const retry = h.take('save_layout');
  assert.deepEqual(retry.request, first.request);
  h.reply(retry, { contract_version: 2, layout: { ...first.request.layout, revision: 2 }, replayed: true });
  await finish(saved);
  assert.equal(h.c.getState().layout.base.theme, 'light');
  assert.equal(h.c.getState().layout.draft.theme, 'contrast');
  assert.equal(h.c.getState().layout.draft.revision, 2);
  assert.equal(h.c.getState().layout.dirty, true);
  const conflict = assert.rejects(h.c.saveLayout(), safe('layout_revision_conflict'));
  h.fail(h.take('save_layout'), { code: 'layout_revision_conflict', user_message: 'Refresh the layout.', recoverable: true });
  await finish(conflict);
  assert.throws(() => h.c.reapplyLayout(), safe('layout_revision_conflict'));
  await h.turn();
  const remote = snapshot();
  remote.layout.revision = 4;
  remote.layout.theme = 'system';
  h.reply(h.take('get_workspace_snapshot'), remote);
  await drain();
  assert.equal(h.c.getState().layout.draft.revision, 2);
  h.c.reapplyLayout();
  assert.equal(h.c.getState().layout.draft.theme, 'contrast');
  const reapplied = h.c.saveLayout();
  const request = h.take('save_layout');
  assert.equal(request.request.expected_revision, 4);
  assert.notEqual(request.request.request_id, first.request.request_id);
  h.reply(request, { contract_version: 2, layout: { ...request.request.layout, revision: 5 }, replayed: false });
  await finish(reapplied);
  assert.equal(h.c.getState().layout.dirty, false);
});

test('failed reads do not spin; explicit focus retries and clears a sanitized error', async t => {
  const h = harness(t);
  await h.init();
  h.c.reconcile();
  await h.turn();
  h.fail(h.take('get_workspace_snapshot'), new Error('private read failure'));
  await drain();
  assert.equal(h.c.getState().last_error.code, 'execution_transport_error');
  assert.equal(h.timer.count(false), 0);
  const count = h.calls.length;
  for (let index = 0; index < 4; index++) await h.turn();
  assert.equal(h.calls.length, count);
  h.c.onFocus();
  await h.turn();
  h.reply(h.take('get_workspace_snapshot'), snapshot());
  await drain();
  assert.equal(h.c.getState().last_error, null);
  assert.equal(h.c.getState().status, 'ready');
});

test('getState and subscriber notifications cannot corrupt nested controller state', async t => {
  const h = harness(t, { onChange: state => {
    if (state.snapshot?.runs[0]) state.snapshot.runs[0].usage.input_tokens = 999;
    state.selected_run_by_session[S] = 'corrupt';
    if (state.transcript) state.transcript.events.length = 0;
    if (state.layout.draft) state.layout.draft.theme = 'light';
    state.attempts.push({ key: 'injected' });
    throw new Error('observer failure');
  } });
  await h.init();
  const state = h.c.getState();
  state.snapshot.sessions.length = 0;
  state.snapshot.runs[0].usage.output_tokens = 77;
  state.transcript.events[3].payload.text = 'corrupt';
  state.transcript.events.length = 0;
  state.selected_run_by_session[S] = 'corrupt';
  state.layout.draft.panels[0].geometry.width = 1;
  state.attempts.push({ key: 'injected' });
  const fresh = h.c.getState();
  assert.equal(fresh.snapshot.sessions.length, 1);
  assert.deepEqual(fresh.snapshot.runs[0].usage, success.snapshot.usage);
  assert.deepEqual(fresh.transcript.events, success.events);
  assert.equal(fresh.selected_run_by_session[S], R);
  assert.equal(fresh.layout.draft.theme, 'dark');
  assert.equal(fresh.layout.draft.panels[0].geometry.width, 460);
  assert.deepEqual(fresh.attempts, []);
});

test('codex runs require explicit consent, a ready Codex, a Codex session and the 1024-byte profile limit', async t => {
  const codexSessionId = id(777);
  const withCodex = availability => {
    const value = snapshot();
    value.codex_availability = availability;
    value.sessions.push({ ...value.sessions[0], session_id: codexSessionId, provider_id: 'codex', label: 'Codex session' });
    return value;
  };
  const h = harness(t);
  await h.init(withCodex('ready'));
  const before = h.mutations().length;
  await finish(assert.rejects(h.c.startCodexRun(codexSessionId, 'Hello', false), safe('validation_failed')));
  await finish(assert.rejects(h.c.startCodexRun(S, 'Hello', true, 'fake-target'), safe('session_not_found')));
  await finish(assert.rejects(h.c.startCodexRun(codexSessionId, '😀'.repeat(256) + 'x', true, 'too-long'), safe('validation_failed')));
  assert.equal(h.mutations().length, before);
  const pending = h.c.startCodexRun(codexSessionId, 'Hello 😀', true);
  const call = h.take('start_codex_run');
  assert.deepEqual(call.request, {
    contract_version: 2, workspace_id: W, request_id: call.request.request_id, session_id: codexSessionId,
    prompt: 'Hello 😀', profile_id: 'codex_text_v1', confirmed: true
  });
  h.reply(call, { contract_version: 2, replayed: false, run: {
    ...summary(1, 'queued'), session_id: codexSessionId, run_id: id(778), enqueue_order: 9,
    provider_id: 'codex', profile_id: 'codex_text_v1', model_id: 'gpt-6.1-sol' } });
  await finish(pending);
  assert.ok(h.c.getState().snapshot.runs.some(run => run.run_id === id(778) && run.provider_id === 'codex'));

  const off = harness(t);
  await off.init(withCodex('not_configured'));
  await finish(assert.rejects(off.c.startCodexRun(codexSessionId, 'Hello', true), safe('codex_unavailable')));
  assert.equal(off.mutations().length, 0);
});

test('a Codex follow-up carries earlier succeeded runs of the same session under codex_text_v2', async t => {
  const codexSessionId = id(777);
  const done = (n, order, session = codexSessionId, state = 'succeeded') => ({
    ...summary(7, state), session_id: session, run_id: id(n), enqueue_order: order,
    provider_id: 'codex', profile_id: 'codex_text_v1', model_id: 'gpt-6.1-sol',
    usage: { input_tokens: 5, output_tokens: 2, estimated: false, cached_input_tokens: 0, cache_write_input_tokens: null, reasoning_output_tokens: 0 }
  });
  const value = snapshot();
  value.codex_availability = 'ready';
  value.sessions.push({ ...value.sessions[0], session_id: codexSessionId, provider_id: 'codex', label: 'Codex session' });
  const otherCodex = id(779);
  value.sessions.push({ ...value.sessions[0], session_id: otherCodex, provider_id: 'codex', label: 'Another Codex session' });
  const failed = { ...done(804, 6, codexSessionId, 'failed'), terminal_error: { code: 'codex_timeout', user_message: 'Codex took too long.', recoverable: true } };
  value.runs = [...value.runs, done(801, 3), done(802, 4), done(803, 5, otherCodex), failed];
  const h = harness(t);
  await h.init(value);
  const before = h.mutations().length;
  // Not carry-eligible: another session's run, a failed run, an unknown run, descending order, duplicates.
  for (const [i, ids] of [[id(803)], [id(804)], [id(899)], [id(802), id(801)], [id(801), id(801)]].entries()) {
    await finish(assert.rejects(h.c.startCodexRun(codexSessionId, 'x', true, `bad-${i}`, ids), safe('validation_failed')));
  }
  assert.equal(h.mutations().length, before, 'nothing reached native');
  const pending = h.c.startCodexRun(codexSessionId, 'and now?', true, undefined, [id(801), id(802)]);
  const call = h.take('start_codex_run');
  assert.deepEqual(call.request, {
    contract_version: 2, workspace_id: W, request_id: call.request.request_id, session_id: codexSessionId,
    prompt: 'and now?', profile_id: 'codex_text_v2', confirmed: true, context_run_ids: [id(801), id(802)]
  });
  h.reply(call, { contract_version: 2, replayed: false, run: { ...done(805, 7, codexSessionId, 'queued'), last_seq: 1, profile_id: 'codex_text_v2',
    usage: { input_tokens: null, output_tokens: null, estimated: false, cached_input_tokens: null, cache_write_input_tokens: null, reasoning_output_tokens: null } } });
  await finish(pending);
  assert.ok(h.c.getState().snapshot.runs.some(run => run.run_id === id(805) && run.profile_id === 'codex_text_v2'));
});

test('the client accepts only well-formed Codex start requests for each profile', async () => {
  const base = { contract_version: 2, workspace_id: W, request_id: id(910), session_id: id(777), prompt: 'hi', confirmed: true };
  const send = request => client.createExecutionClient(async () => { throw new Error('transport must not be reached'); }).start_codex_run(request);
  const reject = request => assert.rejects(send(request), error => error.code === 'validation_failed' || /invalid|validation/i.test(String(error.message ?? error.code)));
  await reject({ ...base, profile_id: 'codex_text_v1', context_run_ids: [id(801)] });
  await reject({ ...base, profile_id: 'codex_text_v2' });
  await reject({ ...base, profile_id: 'codex_text_v2', context_run_ids: [] });
  await reject({ ...base, profile_id: 'codex_text_v2', context_run_ids: [id(801), id(801)] });
  await reject({ ...base, profile_id: 'codex_text_v2', context_run_ids: Array.from({ length: 7 }, (_, i) => id(820 + i)) });
  await reject({ ...base, profile_id: 'codex_text_v2', context_run_ids: ['not-an-id'] });
  await reject({ ...base, profile_id: 'codex_text_v3', context_run_ids: [id(801)] });
});

test('context previews are exact, bounded and tied to the requested workspace', async () => {
  const request = { contract_version: 2, workspace_id: W, session_id: id(777), prompt: 'and now?', context_run_ids: [id(801), id(802)] };
  const text = ['Earlier turns', '', '[User] hi 😀', '[Assistant] yo', '', 'New message from the user:', 'and now?'].join('\n');
  const ok = { contract_version: 2, workspace_id: W, text, bytes: new TextEncoder().encode(text).length, carried: 2, trimmed: false };
  const ask = (reply, req = request) => client.createExecutionClient(async (command, args) => {
    assert.equal(command, 'preview_codex_context_v2');
    assert.deepEqual(args.request, req);
    return reply;
  }).preview_codex_context(req);
  assert.deepEqual(await ask(ok), ok);
  for (const bad of [
    { ...ok, workspace_id: id(902) },
    { ...ok, bytes: ok.bytes + 1 },
    { ...ok, carried: 7 },
    { ...ok, text: '' },
    { ...ok, trimmed: 'no' },
    { ...ok, extra: 1 },
    { ...ok, text: 'x'.repeat(8193), bytes: 8193 }
  ]) await assert.rejects(ask(bad), () => true);
  const refuse = req => assert.rejects(client.createExecutionClient(async () => { throw new Error('must not reach native'); }).preview_codex_context(req), () => true);
  await refuse({ ...request, context_run_ids: [] });
  await refuse({ ...request, context_run_ids: [id(801), id(801)] });
  await refuse({ ...request, prompt: 'x'.repeat(1025) });
  await refuse({ ...request, confirmed: true });
});

test('run progress reads accept only text-free bounded steps for the requested workspace', async () => {
  const request = { contract_version: 2, workspace_id: W };
  const ok = { contract_version: 2, workspace_id: W, run_id: id(900), steps: [{ step: 'thread_started', count: 1 }, { step: 'reasoning_observed', count: 3 }] };
  const ask = reply => client.createExecutionClient(async (command, args) => {
    assert.equal(command, 'get_run_progress_v2');
    assert.deepEqual(args.request, request);
    return reply;
  }).get_run_progress(request);
  assert.deepEqual(await ask(ok), ok);
  assert.deepEqual((await ask({ ...ok, run_id: null, steps: [] })).steps, []);
  for (const bad of [
    { ...ok, workspace_id: id(901) },
    { ...ok, run_id: null },
    { ...ok, steps: [{ step: 'thread_started', count: 0 }] },
    { ...ok, steps: [{ step: 'text_delta', count: 1 }] },
    { ...ok, steps: [{ step: 'turn_started', count: 1, text: 'leak' }] },
    { ...ok, steps: Array.from({ length: 65 }, () => ({ step: 'reasoning_observed', count: 1 })) }
  ]) await assert.rejects(ask(bad));
});
