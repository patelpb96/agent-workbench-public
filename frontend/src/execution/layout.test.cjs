'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const root = path.resolve(__dirname, '../../..');
const compiled = path.join(root, 'artifacts/frontend-contract-src/execution');
const L = require(path.join(compiled, 'layout.js'));
const client = require(path.join(compiled, 'client.js'));
const { limits } = require(path.join(compiled, 'generated.js'));
const { upgrade } = require('./v2-fixtures.cjs');
const fixture = upgrade(JSON.parse(fs.readFileSync(path.join(root, 'contracts/fixtures/layout.json'), 'utf8')));
const clone = value => JSON.parse(JSON.stringify(value));
const id = n => 'bbbbbbbb-0000-4000-8000-' + n.toString(16).padStart(12, '0');
const P = fixture.panels[0].panel_id;
const S = fixture.panels[0].session_id;
const C = id(80);
const warning = { code: 'unsupported_layout_version', user_message: 'Stored layout is preserved.', recoverable: false };
const error = code => ({ code, user_message: 'SIMULATED ' + code, recoverable: true });

function safe(code) {
  return value => {
    assert.ok(value instanceof client.ExecutionClientError);
    assert.equal(value.code, code);
    assert.equal(value.stack, undefined);
    assert.deepEqual(Object.keys(value.error).sort(), ['code', 'recoverable', 'user_message']);
    return true;
  };
}

function manager() {
  let serial = 100;
  const m = L.createLayoutManager(() => id(++serial));
  m.observe(clone(fixture), null);
  return m;
}

function edit(m, patch) {
  m.edit(L.updateWorkspaceChrome(m.getState().draft, patch));
}

function acknowledgement(request, replayed = false) {
  return { contract_version: 2, layout: { ...clone(request.layout), revision: request.expected_revision + 1 }, replayed };
}

function frozenTree(value) {
  if (value === null || typeof value !== 'object') return;
  assert.equal(Object.isFrozen(value), true);
  for (const child of Object.values(value)) frozenTree(child);
}

test('null authority is not an invented default; preserved warnings lock edits and saves', () => {
  const m = L.createLayoutManager(() => id(1));
  assert.deepEqual(m.getState(), {
    base: null, draft: null, warning: null, edit_epoch: 0,
    attempt: null, dirty: false, conflicted: false, problem: null
  });
  assert.equal(L.canSaveLayout(m.getState()), false);
  assert.throws(() => m.edit(clone(fixture)), safe('layout_invalid'));
  assert.throws(() => m.beginSave(), safe('layout_invalid'));
  for (const code of ['layout_invalid', 'unsupported_layout_version', 'storage_unavailable']) {
    const detail = { ...warning, code };
    m.observe(null, detail);
    detail.user_message = 'external mutation';
    assert.equal(m.getState().base, null);
    assert.equal(m.getState().warning.user_message, warning.user_message);
    assert.equal(L.canSaveLayout(m.getState()), false);
    assert.throws(() => m.edit(clone(fixture)), safe('layout_invalid'));
    assert.throws(() => m.beginSave(), safe('layout_invalid'));
  }
  for (const args of [[null, null], [fixture, warning], [null, {}]]) {
    const before = m.getState();
    assert.throws(() => m.observe(...args), safe('layout_invalid'));
    assert.deepEqual(m.getState(), before);
  }
  m.observe(clone(fixture), null);
  assert.deepEqual(m.getState().base, fixture);
  assert.deepEqual(m.getState().draft, fixture);
  assert.equal(m.getState().warning, null);
  edit(m, { theme: 'light' });
  const request = m.beginSave();
  const before = m.getState();
  m.observe(null, warning);
  assert.deepEqual(m.getState().base, before.base);
  assert.deepEqual(m.getState().draft, before.draft);
  assert.deepEqual(m.getState().attempt, before.attempt);
  assert.throws(() => m.beginSave(), safe('layout_invalid'));
  m.acknowledge(acknowledgement(request));
  assert.equal(m.getState().warning.code, warning.code);
  assert.equal(m.getState().dirty, true);
  assert.equal(m.getState().attempt, null);
  assert.equal(L.canSaveLayout(m.getState()), false);
});

test('a later valid observation resolves a warning without discarding unsaved intent', () => {
  const m = manager();
  edit(m, { theme: 'light' });
  m.observe(null, warning);
  m.observe(clone(fixture), null);
  const state = m.getState();
  assert.equal(state.warning, null);
  assert.equal(state.draft.theme, 'light');
  assert.equal(state.dirty, true);
  assert.equal(L.canSaveLayout(state), true);
});

test('edit during save survives acknowledgement and is saved only by a subsequent acknowledged attempt', () => {
  const m = manager();
  edit(m, { theme: 'light' });
  const capturedEpoch = m.getState().edit_epoch;
  const first = m.beginSave();
  assert.deepEqual(first, {
    contract_version: 2, workspace_id: fixture.workspace_id, request_id: id(101),
    expected_revision: 1, layout: { ...clone(fixture), theme: 'light' }
  });
  edit(m, { theme: 'contrast', sidebar_width: 440 });
  assert.ok(m.getState().edit_epoch > capturedEpoch);
  assert.deepEqual(m.getState().attempt.request, first);
  assert.deepEqual(m.beginSave(), first);
  m.acknowledge(acknowledgement(first, true));
  let state = m.getState();
  assert.equal(state.base.revision, 2);
  assert.equal(state.base.theme, 'light');
  assert.equal(state.draft.revision, 2);
  assert.equal(state.draft.theme, 'contrast');
  assert.equal(state.draft.sidebar_width, 440);
  assert.equal(state.dirty, true);
  assert.equal(state.attempt, null);
  assert.equal(L.canSaveLayout(state), true);
  const second = m.beginSave();
  assert.equal(second.expected_revision, 2);
  assert.equal(second.layout.revision, 2);
  assert.notEqual(second.request_id, first.request_id);
  m.acknowledge(acknowledgement(second));
  state = m.getState();
  assert.equal(state.dirty, false);
  assert.deepEqual(state.base, state.draft);
  assert.equal(state.base.revision, 3);
  assert.equal(L.canSaveLayout(state), false);
  const epoch = state.edit_epoch;
  m.edit(clone(state.draft));
  assert.equal(m.getState().edit_epoch, epoch, 'an identical edit is not newer intent');
});

for (const code of ['execution_transport_error', 'storage_busy', 'storage_unavailable', 'invalid_execution_response']) {
  test(code + ' retains exact ambiguous DTO despite edits and remote revision advance', () => {
    const m = manager();
    edit(m, { theme: 'light' });
    const request = m.beginSave();
    m.reject(error(code));
    edit(m, { theme: 'contrast' });
    const remote = { ...clone(fixture), revision: 9, theme: 'system' };
    m.observe(remote, null);
    assert.equal(m.getState().conflicted, true);
    assert.equal(m.getState().base.revision, 9);
    assert.equal(m.getState().draft.revision, 1);
    assert.deepEqual(m.beginSave(), request);
    assert.equal(L.canSaveLayout(m.getState()), true, 'only the unresolved exact attempt remains retryable');
    assert.throws(() => m.reapply(), safe('layout_revision_conflict'));
    m.acknowledge(acknowledgement(request, true));
    const state = m.getState();
    assert.equal(state.base.revision, 9, 'an old immutable receipt cannot roll back authority');
    assert.equal(state.base.theme, 'system');
    assert.equal(state.draft.theme, 'contrast');
    assert.equal(state.dirty, true);
    assert.equal(state.conflicted, true);
    assert.equal(state.attempt, null);
    m.reapply();
    const reapplied = m.beginSave();
    assert.equal(reapplied.expected_revision, 9);
    assert.equal(reapplied.layout.theme, 'contrast');
    assert.notEqual(reapplied.request_id, request.request_id);
  });
}

for (const pending of [false, true]) {
  test('remote revision conflict requires refreshed explicit reapply; pending=' + pending, () => {
    const m = manager();
    edit(m, { theme: 'light', active_module: 'diagnostics' });
    if (pending) {
      m.beginSave();
      m.reject(error('layout_revision_conflict'));
      assert.equal(m.getState().attempt, null);
      assert.throws(() => m.reapply(), safe('layout_revision_conflict'));
    }
    const remote = { ...clone(fixture), revision: 2, theme: 'contrast', sidebar_width: 400 };
    m.observe(remote, null);
    assert.equal(m.getState().base.theme, 'contrast');
    assert.equal(m.getState().draft.theme, 'light');
    assert.equal(m.getState().draft.revision, 1);
    assert.equal(L.canSaveLayout(m.getState()), false);
    assert.throws(() => m.beginSave(), safe('layout_revision_conflict'));
    m.reapply();
    const state = m.getState();
    assert.equal(state.draft.revision, 2);
    assert.equal(state.draft.theme, 'light');
    assert.equal(state.draft.active_module, 'diagnostics');
    assert.equal(state.draft.sidebar_width, 300, 'full-layout intent is not silently merged with a remote draft');
    assert.equal(state.conflicted, false);
    assert.equal(state.dirty, true);
    assert.equal(m.beginSave().expected_revision, 2);
  });
}

for (const newerEdit of [false, true]) {
  test('own commit observed before acknowledgement does not become a remote conflict; newer edit=' + newerEdit, () => {
    const m = manager();
    edit(m, { theme: 'light' });
    const request = m.beginSave();
    if (newerEdit) edit(m, { theme: 'contrast' });
    const epoch = m.getState().edit_epoch;
    const response = acknowledgement(request);
    m.observe(response.layout, null);
    assert.equal(m.getState().base.revision, 2);
    assert.equal(m.getState().conflicted, false);
    assert.equal(m.getState().dirty, true, 'observation is not an acknowledgement');
    assert.equal(m.getState().edit_epoch, epoch);
    assert.deepEqual(m.beginSave(), request);
    m.acknowledge(response);
    assert.equal(m.getState().dirty, newerEdit);
    assert.equal(m.getState().draft.theme, newerEdit ? 'contrast' : 'light');
    assert.equal(m.getState().draft.revision, 2);
    assert.equal(m.getState().attempt, null);
  });
}

test('stale and wrong acknowledgements cannot consume a newer attempt or roll authority backward', () => {
  const m = manager();
  edit(m, { theme: 'light' });
  const first = m.beginSave();
  const oldAck = acknowledgement(first);
  m.acknowledge(oldAck);
  edit(m, { theme: 'contrast' });
  const second = m.beginSave();
  const before = m.getState();
  m.acknowledge({ ...oldAck, replayed: true });
  assert.deepEqual(m.getState(), before);
  for (const mutate of [
    response => { response.layout.workspace_id = id(900); },
    response => { response.layout.revision = 4; },
    response => { response.layout.theme = 'system'; },
    response => { response.contract_version = 1; },
    response => { response.extra = true; }
  ]) {
    const wrong = acknowledgement(second);
    mutate(wrong);
    assert.throws(() => m.acknowledge(wrong), safe('invalid_execution_response'));
    assert.deepEqual(m.getState(), before);
  }
  m.observe({ ...clone(fixture), revision: 1 }, null);
  assert.deepEqual(m.getState(), before);
  m.acknowledge(acknowledgement(second));
  assert.equal(m.getState().base.revision, 3);
  assert.equal(m.getState().base.theme, 'contrast');
  assert.equal(m.getState().dirty, false);
  m.acknowledge(oldAck);
  assert.equal(m.getState().base.revision, 3);
  assert.throws(() => m.acknowledge({ ...acknowledgement(second), layout: { ...clone(fixture), revision: 20 } }), safe('invalid_execution_response'));
});

test('same-revision contradictions remain visible and authority never moves backward', () => {
  const m = manager();
  edit(m, { theme: 'light' });
  m.observe({ ...clone(fixture), theme: 'contrast' }, null);
  assert.equal(m.getState().conflicted, true);
  assert.equal(m.getState().problem.code, 'layout_revision_conflict');
  assert.deepEqual(m.getState().base, fixture);
  assert.equal(m.getState().draft.theme, 'light');
  assert.throws(() => m.beginSave(), safe('layout_revision_conflict'));
  assert.throws(() => m.reapply(), safe('layout_revision_conflict'));
  m.observe({ ...clone(fixture), revision: 5 }, null);
  m.observe({ ...clone(fixture), revision: 2 }, null);
  assert.equal(m.getState().base.revision, 5);
  assert.equal(m.getState().draft.revision, 1);
  m.reapply();
  assert.equal(m.getState().draft.revision, 5);
});

test('state, requests and nested DTOs are immutable defensive copies; invalid ID sources do not create attempts', () => {
  const input = clone(fixture);
  const m = L.createLayoutManager(() => id(1));
  m.observe(input, null);
  input.panels[0].geometry.width = 1;
  input.theme = 'light';
  assert.deepEqual(m.getState().base, fixture);
  const draft = { ...clone(fixture), theme: 'light' };
  m.edit(draft);
  draft.panels[0].expanded_geometry.width = 1;
  draft.theme = 'contrast';
  const request = m.beginSave();
  frozenTree(request);
  frozenTree(m.getState());
  assert.throws(() => { request.layout.panels[0].geometry.width = 1; }, TypeError);
  assert.throws(() => { m.getState().attempt.request.layout.theme = 'contrast'; }, TypeError);
  assert.equal(m.beginSave().layout.theme, 'light');
  assert.equal(m.beginSave().layout.panels[0].expanded_geometry.width, 460);
  const response = acknowledgement(request);
  m.acknowledge(response);
  response.layout.theme = 'system';
  assert.equal(m.getState().base.theme, 'light');
  for (const badId of ['bad', id(1).toUpperCase(), null]) {
    const invalid = L.createLayoutManager(() => badId);
    invalid.observe(fixture, null);
    edit(invalid, { theme: 'light' });
    assert.throws(() => invalid.beginSave(), safe('validation_failed'));
    assert.equal(invalid.getState().attempt, null);
    assert.equal(invalid.getState().dirty, true);
  }
});

test('full/compact/block cycling restores expanded geometry without progressive shrinking', () => {
  let value = clone(fixture);
  const original = clone(value);
  for (let cycle = 0; cycle < 3; cycle++) {
    value = L.cyclePanelStage(value, P);
    assert.equal(value.panels[0].stage, 'compact');
    assert.deepEqual(value.panels[0].geometry, { x: 48, y: 64, width: 320, height: 48 });
    assert.deepEqual(value.panels[0].expanded_geometry, original.panels[0].geometry);
    value = L.cyclePanelStage(value, P);
    assert.equal(value.panels[0].stage, 'block');
    assert.deepEqual(value.panels[0].geometry, { x: 48, y: 64, width: 80, height: 48 });
    value = L.cyclePanelStage(value, P);
    assert.deepEqual(value, original);
  }
  assert.deepEqual(fixture, original);
  let compact = L.cyclePanelStage(fixture, P);
  compact = L.moveOrResizePanel(compact, P, { x: 200, y: 220, width: 300, height: 48 }, null);
  assert.deepEqual(compact.panels[0].expanded_geometry, original.panels[0].geometry);
  value = L.cyclePanelStage(L.cyclePanelStage(compact, P), P);
  assert.deepEqual(value.panels[0].geometry, original.panels[0].geometry);
  const full = L.moveOrResizePanel(value, P, { x: 101, y: 111, width: 600, height: 400 }, null);
  assert.deepEqual(full.panels[0].geometry, full.panels[0].expanded_geometry);
  const restoredCompact = clone(compact);
  assert.equal(L.cyclePanelStage(restoredCompact, P).panels[0].stage, 'block');
  assert.deepEqual(L.cyclePanelStage(L.cyclePanelStage(restoredCompact, P), P).panels[0].geometry, original.panels[0].geometry);
});

test('viewport clamp is display-only and handles zero, tiny, collapsed and intersected container bounds', () => {
  const cases = [
    [{ width: 1000, height: 800 }, null, { full: [48, 64, 460, 350], compact: [48, 64, 320, 48], block: [48, 64, 80, 48] }],
    [{ width: 100, height: 70 }, null, { full: [0, 0, 100, 70], compact: [0, 22, 100, 48], block: [20, 22, 80, 48] }],
    [{ width: 0, height: 0 }, null, { full: [0, 0, 0, 0], compact: [0, 0, 0, 0], block: [0, 0, 0, 0] }],
    [{ width: 0.25, height: 0.5 }, null, { full: [0, 0, 0.25, 0.5], compact: [0, 0, 0.25, 0.5], block: [0, 0, 0.25, 0.5] }],
    [{ width: 1000, height: 800 }, { x: 100, y: 120, width: 200, height: 100 }, { full: [100, 120, 200, 100], compact: [100, 120, 200, 48], block: [100, 120, 80, 48] }],
    [{ width: 100, height: 70 }, { x: 90, y: 60, width: 200, height: 100 }, { full: [90, 60, 10, 10], compact: [90, 60, 10, 10], block: [90, 60, 10, 10] }],
    [{ width: 100, height: 70 }, { x: 900, y: 700, width: 200, height: 100 }, { full: [100, 70, 0, 0], compact: [100, 70, 0, 0], block: [100, 70, 0, 0] }]
  ];
  for (const [viewport, containerGeometry, expected] of cases) {
    for (const stage of ['full', 'compact', 'block']) {
      const panel = { ...clone(fixture.panels[0]), stage, container_id: containerGeometry ? C : null };
      const container = containerGeometry ? { container_id: C, geometry: clone(containerGeometry) } : null;
      const before = clone({ panel, viewport, container });
      const actual = L.clampPanelForViewport(panel, viewport, container);
      const [x, y, width, height] = expected[stage];
      assert.deepEqual(actual, { x, y, width, height }, stage + ':' + JSON.stringify(viewport));
      assert.deepEqual({ panel, viewport, container }, before);
      frozenTree(actual);
      assert.equal(client.isLayout({ ...clone(fixture), panels: [panel], containers: container ? [container] : [] }), true);
    }
  }
  for (const viewport of [{ width: -1, height: 10 }, { width: NaN, height: 10 }, { width: 10, height: Infinity }, { width: '10', height: 10 }]) {
    assert.throws(() => L.clampPanelForViewport(fixture.panels[0], viewport, null), safe('layout_invalid'));
  }
  assert.throws(() => L.clampPanelForViewport({ ...fixture.panels[0], container_id: C }, { width: 100, height: 100 }, null), safe('layout_invalid'));
});

test('one panel per session is idempotent, collision-safe, bounded and independent of run dispatch', () => {
  const existing = L.ensureSessionPanel(fixture, S, id(1000));
  assert.equal(existing.created, false);
  assert.equal(existing.panel.panel_id, P);
  assert.deepEqual(existing.layout, fixture);
  let value = clone(fixture);
  for (let index = 1; index < limits.workspace_sessions; index++) {
    const result = L.ensureSessionPanel(value, id(2000 + index), id(3000 + index));
    assert.equal(result.created, true);
    assert.equal(result.layout.panels.length, index + 1);
    assert.equal(client.isLayout(result.layout), true);
    frozenTree(result.layout);
    value = result.layout;
  }
  assert.equal(value.panels.length, 16);
  assert.throws(() => L.ensureSessionPanel(value, id(4000), id(5000)), safe('capacity_exceeded'));
  assert.equal(L.ensureSessionPanel(value, S, id(5000)).created, false);
  assert.throws(() => L.ensureSessionPanel(fixture, id(4000), P), safe('layout_invalid'));
  const containerLayout = { ...clone(fixture), containers: [{ container_id: C, geometry: clone(fixture.panels[0].geometry) }] };
  assert.throws(() => L.ensureSessionPanel(containerLayout, id(4000), C), safe('layout_invalid'));
  for (const bad of ['bad', id(1).toUpperCase(), null]) {
    assert.throws(() => L.ensureSessionPanel(fixture, bad, id(5000)), safe('layout_invalid'));
    assert.throws(() => L.ensureSessionPanel(fixture, S, bad), safe('layout_invalid'));
  }
});

test('layout helpers reject cross-kind/self/unknown references, duplicate panels and container overflow', () => {
  const geometry = clone(fixture.panels[0].geometry);
  const invalidLayouts = [
    value => { value.containers.push({ container_id: P, geometry }); },
    value => { value.containers.push({ container_id: P, geometry }); value.panels[0].container_id = P; },
    value => { value.panels[0].container_id = C; },
    value => { value.panels.push({ ...clone(value.panels[0]), panel_id: id(2) }); },
    value => { value.panels.push({ ...clone(value.panels[0]), session_id: id(2) }); },
    value => { value.containers = [{ container_id: C, geometry }, { container_id: C, geometry }]; },
    value => { value.containers = Array.from({ length: 9 }, (_, index) => ({ container_id: id(100 + index), geometry })); },
    value => { value.containers = [{ container_id: C, geometry, parent_id: C }]; },
    value => { value.panels = Array.from({ length: 17 }, (_, index) => ({ ...clone(value.panels[0]), panel_id: id(100 + index), session_id: id(200 + index) })); },
    value => { value.panels[0].panel_id = id(1).toUpperCase(); },
    value => { value.panels[0].stage = 'unknown'; }
  ];
  for (const change of invalidLayouts) {
    const value = clone(fixture);
    change(value);
    assert.equal(client.isLayout(value), false);
    for (const operation of [
      () => L.cyclePanelStage(value, P),
      () => L.moveOrResizePanel(value, P, geometry, null),
      () => L.ensureSessionPanel(value, S, id(5000)),
      () => L.updateWorkspaceChrome(value, {})
    ]) assert.throws(operation, safe('layout_invalid'));
    const m = manager();
    assert.throws(() => m.edit(value), safe('layout_invalid'));
    assert.equal(m.getState().dirty, false);
  }
  for (const containerId of [P, C, id(999)]) {
    assert.throws(() => L.moveOrResizePanel(fixture, P, geometry, containerId), safe('layout_invalid'));
  }
  const nested = { ...clone(fixture), containers: [{ container_id: C, geometry }] };
  const absolute = { x: 800, y: 600, width: 460, height: 350 };
  const moved = L.moveOrResizePanel(nested, P, absolute, C);
  assert.deepEqual(moved.panels[0].geometry, absolute, 'durable coordinates stay canvas-absolute, not container-relative');
  assert.equal(moved.panels[0].container_id, C);
  assert.equal(L.moveOrResizePanel(moved, P, absolute, null).panels[0].container_id, null);
});

test('nonfinite, out-of-range and malformed explicit geometry edits are rejected before copying', () => {
  const original = clone(fixture);
  for (const [field, values] of [
    ['x', [-1, 100001, NaN, Infinity, -Infinity, '0', null]],
    ['y', [-1, 100001, NaN, Infinity]],
    ['width', [0, -1, 100001, NaN, Infinity]],
    ['height', [0, -1, 100001, NaN, Infinity]]
  ]) {
    for (const value of values) {
      const geometry = { ...clone(fixture.panels[0].geometry), [field]: value };
      assert.throws(() => L.moveOrResizePanel(fixture, P, geometry, null), safe('layout_invalid'), field + ':' + String(value));
      const draft = clone(fixture);
      draft.panels[0].expanded_geometry[field] = value;
      assert.throws(() => L.cyclePanelStage(draft, P), safe('layout_invalid'));
    }
  }
  for (const geometry of [{ x: 0, y: 0, width: 1 }, { x: 0, y: 0, width: 1, height: 1, extra: true }]) {
    assert.throws(() => L.moveOrResizePanel(fixture, P, geometry, null), safe('layout_invalid'));
  }
  const boundary = { x: 100000, y: 0, width: 100000, height: 1 };
  assert.deepEqual(L.moveOrResizePanel(fixture, P, boundary, null).panels[0].geometry, boundary);
  assert.deepEqual(fixture, original);
});

test('Shift grid snapping has deterministic defaults, validates inputs and does not snap without Shift', () => {
  for (const [value, shift, grid, expected] of [
    [31, true, undefined, 40], [29, true, undefined, 20], [31, false, undefined, 31],
    [12, true, 5, 10], [-31, true, undefined, -40], [-1, true, undefined, 0],
    [2.25, false, 0.5, 2.25], [2.25, true, 0.5, 2.5]
  ]) assert.equal(L.snapValue(value, shift, grid), expected);
  assert.equal(Object.is(L.snapValue(-1, true), -0), false);
  for (const args of [[NaN, true], [Infinity, false], [1, 'yes'], [1, true, 0], [1, false, -1], [1, true, NaN], [1, true, Infinity], [Number.MAX_VALUE, true, Number.MIN_VALUE]]) {
    assert.throws(() => L.snapValue(...args), safe('layout_invalid'));
  }
});

test('chrome patches accept only wire fields/enums and sidebar bounds, without mutating layout input', () => {
  for (const theme of ['dark', 'light', 'system', 'contrast']) {
    for (const active_module of ['agents', 'tasks', 'usage', 'approvals', 'diagnostics']) {
      const result = L.updateWorkspaceChrome(fixture, { theme, active_module, sidebar_width: 240 });
      assert.equal(result.theme, theme);
      assert.equal(result.active_module, active_module);
      assert.equal(result.sidebar_width, 240);
      assert.equal(result.revision, fixture.revision);
      assert.deepEqual(result.panels, fixture.panels);
      frozenTree(result);
    }
  }
  assert.equal(L.updateWorkspaceChrome(fixture, { sidebar_width: 440 }).sidebar_width, 440);
  for (const patch of [
    { theme: 'unknown' }, { active_module: 'models' }, { sidebar_width: 239.9 },
    { sidebar_width: 440.1 }, { sidebar_width: NaN }, { sidebar_width: Infinity },
    { sidebar_width: '300' }, { revision: 2 }, { panels: [] }, { media_path: 'not-a-wire-field' },
    { theme: undefined }, [], null, { theme: { toString: () => 'dark' } }
  ]) assert.throws(() => L.updateWorkspaceChrome(fixture, patch), safe('layout_invalid'));
  assert.equal(fixture.theme, 'dark');
  assert.equal(fixture.sidebar_width, 300);
});
