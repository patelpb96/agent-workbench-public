import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { test, expect } from './fixtures.mjs';
const panel = page => page.locator('section.agent');
const composer = page => page.getByRole('region', { name: 'Prompt', exact: true });
async function ready(page) {
  await page.goto('/');
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeEnabled();
}
async function submit(page, scenario, prompt) {
  await composer(page).getByLabel('Prompt (local test runner)').fill(prompt);
  await composer(page).getByLabel('Test scenario').selectOption(scenario);
  await composer(page).getByRole('button', { name: 'Send test run', exact: true }).click();
}

async function createEmptySession(page, label) {
  await ready(page);
  await page.getByLabel('Session label', { exact: true }).fill(label);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
}
async function saveDraft(page, bridge, revision) {
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', `SAVED · REV ${revision}`);
  const snapshot = await bridge.snapshot();
  expect(snapshot.layout.revision).toBe(revision);
  expect(snapshot.runs).toEqual([]);
  return snapshot.layout;
}
async function reopenLayout(page, bridge, saved) {
  const saves = bridge.counts.save_layout;
  await bridge.restart();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeEnabled();
  await expect(panel(page)).toHaveCount(saved.panels.length);
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', `SAVED · REV ${saved.revision}`);
  const restored = await bridge.snapshot();
  expect(restored.layout).toEqual(saved);
  expect(restored.runs).toEqual([]);
  expect(bridge.counts.start_fake_run).toBe(0);
  expect(bridge.counts.save_layout).toBe(saves); // Reopen is read-only, never an implicit save.
  await expect(page.getByRole('button', { name: 'Save layout', exact: true })).toBeDisabled();
}
async function expectPanelGeometry(page, geometry) {
  await expect.poll(async () => {
    const box = await panel(page).boundingBox(), canvas = await page.locator('.canvas').boundingBox();
    return box && canvas && { x: box.x - canvas.x, y: box.y - canvas.y, width: box.width, height: box.height };
  }).toEqual(geometry);
}

async function virtualDrag(page, handle, dx, dy) {
  await handle.scrollIntoViewIfNeeded();
  const box = await handle.boundingBox();
  expect(box).not.toBeNull();
  const x = box.x + box.width / 2, y = box.y + box.height / 2;
  // CDP input in this fresh headless context only, never the physical OS pointer.
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + dx, y + dy, { steps: 3 });
  await page.mouse.up();
}
async function narrowPanelViewport(page, original) {
  await page.setViewportSize({ width: 850, height: 960 }); // Virtual viewport, not a desktop window.
  await expect.poll(async () => (await panel(page).boundingBox()).width).toBeLessThan(original.width);
  const canvas = await page.locator('.canvas').boundingBox();
  await expectPanelGeometry(page, { ...original, x: 0, width: canvas.width });
  return canvas.width;
}

test('real Svelte DOM + native SQLite: success/failure/cancel/history/layout/restart without redispatch', async ({ page, bridge }) => {
  await ready(page);
  await expect(panel(page)).toHaveCount(0);
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED headless integration');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  expect((await bridge.snapshot()).runs).toHaveLength(0); // Create/open never sends work.
  await expect(panel(page)).toContainText('No runs yet');

  await submit(page, 'success', 'SIMULATED headless success');
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  await expect(panel(page)).toContainText('SIMULATED: fake response complete.');
  await expect(panel(page)).toContainText('input unknown, output unknown');
  const success = (await bridge.snapshot()).runs[0];

  await submit(page, 'failure', 'SIMULATED headless failure');
  await expect(panel(page)).toHaveAttribute('data-state', 'failed');
  await expect(panel(page)).toContainText('fake_start_failed');
  const failed = (await bridge.snapshot()).runs.find(run => run.state === 'failed');

  await submit(page, 'wait_for_cancel', 'SIMULATED headless cancel');
  await expect(panel(page)).toHaveAttribute('data-state', 'running');
  await panel(page).getByRole('button', { name: 'Cancel this run', exact: true }).click();
  await expect(panel(page)).toHaveAttribute('data-state', 'cancelled');
  const cancelled = (await bridge.snapshot()).runs.find(run => run.state === 'cancelled');

  const history = composer(page).getByLabel('Selected / historical run');
  for (const run of [success, failed, cancelled]) {
    await history.selectOption(run.run_id);
    await expect(panel(page)).toHaveAttribute('data-state', run.state);
    await expect(panel(page).locator('.transcript')).toContainText(run.state === 'succeeded' ? 'fake response complete' : run.state === 'failed' ? 'fake_start_failed' : 'cancel_requested → cancelled');
  }
  expect((await bridge.snapshot()).runs).toHaveLength(3);

  const initial = await panel(page).boundingBox();
  const handle = panel(page).getByRole('button', { name: /^Focus SIMULATED headless integration; drag/ });
  await handle.press('ArrowRight'); // Virtual browser keyboard only, never OS input.
  await expect.poll(async () => (await panel(page).boundingBox()).x).toBeGreaterThan(initial.x);
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  const saved = (await bridge.snapshot()).layout;

  await submit(page, 'wait_for_cancel', 'SIMULATED headless restart');
  await expect(panel(page)).toHaveAttribute('data-state', 'running');
  const before = await bridge.snapshot();
  expect(before.runs).toHaveLength(4);
  const running = before.runs.find(run => run.state === 'running');
  expect(bridge.counts.start_fake_run).toBe(4);
  await bridge.restart(); // EOF -> real owner shutdown -> real SQLite startup audit/recovery.
  await page.reload();
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeEnabled();
  await expect(panel(page)).toHaveAttribute('data-state', 'interrupted');
  await expect(panel(page)).toContainText('Run interrupted after restart; previous state: running.');
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  const restored = await bridge.snapshot();
  expect(restored.layout).toEqual(saved);
  expect(restored.runs).toHaveLength(4);
  expect(restored.runs.find(run => run.run_id === running.run_id).state).toBe('interrupted');
  // Deliberate bounded observation across a visible 2-second polling cycle,
  // not an arbitrary delay used instead of UI readiness assertions.
  await page.waitForTimeout(2200);
  expect((await bridge.snapshot()).runs).toHaveLength(4);
  expect(bridge.counts.start_fake_run).toBe(4);
  await composer(page).getByLabel('Selected / historical run').selectOption(success.run_id);
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  await expect(panel(page)).toContainText('fake response complete.');
});

test('headless browser facade cannot broaden native execution or dispatch real providers', async ({ page, bridge }) => {
  await ready(page);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  const snapshot = await bridge.snapshot();
  for (const provider of ['codex', 'claude']) {
    const rejected = await page.evaluate(async ({ workspace, sessions, provider }) => {
      return globalThis.__headlessInvoke({ command: 'start_fake_run_v2', request: {
        contract_version: 2, workspace_id: workspace.workspace_id, session_id: sessions[0].session_id,
        request_id: crypto.randomUUID(), provider_id: provider, prompt: 'SIMULATED forbidden provider check', scenario: 'success'
      } });
    }, { ...snapshot, provider });
    // v2 providers are a closed set: codex is refused by the fake command;
    // unknown 'claude' fails strict decode.
    expect(rejected.error.code).toBe(provider === 'codex' ? 'provider_execution_disabled' : 'validation_failed');
  }
  const missing = await page.evaluate(async () => {
    try { await globalThis.__headlessInvoke({ command: 'bootstrap_workspace_v2' }); return false; }
    catch { return true; }
  });
  expect(missing).toBe(true);
  await assert.rejects(bridge.call('bootstrap_workspace', undefined), /explicit request/);
  expect((await bridge.snapshot()).workspace.workspace_id).toBe(snapshot.workspace.workspace_id);
  expect((await bridge.snapshot()).runs).toHaveLength(0);
  const pathInjection = await page.evaluate(() => globalThis.__headlessInvoke({
    command: 'bootstrap_workspace_v2', request: { contract_version: 2, directory: '../not-a-browser-selected-workspace' }
  }));
  expect(pathInjection.error.code).toBe('validation_failed');
  const blocked = await page.evaluate(async () => {
    const results = [];
    for (const command of ['open_background_file', 'shutdown', 'launch_workbench']) {
      try { await globalThis.__headlessInvoke({ command, request: {} }); results.push(false); }
      catch { results.push(true); }
    }
    return results;
  });
  expect(blocked).toEqual([true, true, true]);
  expect((await bridge.snapshot()).workspace.workspace_id).toBe(snapshot.workspace.workspace_id);
});

test('crashed owned test process still cleans its exact workspace and reports failure', async ({ bridge }) => {
  await bridge.snapshot();
  await bridge.crashOwnedForTest();
  await assert.rejects(bridge.cleanup(), /Headless bridge failed/);
  expect(bridge.evidence.owned_workspace_removed).toBe(true);
  expect(bridge.evidence.normal_exits).toBe(0);
});

test('panel stage cycling saves each stage and restores full geometry across native restarts', async ({ page, bridge }) => {
  await createEmptySession(page, 'SIMULATED stage round trip');
  const base = await saveDraft(page, bridge, 1);
  const original = base.panels[0];
  expect(original.stage).toBe('full');
  await expectPanelGeometry(page, original.geometry);
  let authority = base;
  const stages = [
    { stage: 'compact', button: 'Collapse panel to compact stage', width: 320, height: 48 },
    { stage: 'block', button: 'Collapse panel to status block stage', width: 80, height: 48 },
    { stage: 'full', button: 'Restore panel full expanded geometry', ...original.geometry }
  ];
  for (const [index, target] of stages.entries()) {
    const geometry = { ...original.geometry, width: target.width, height: target.height };
    await panel(page).getByRole('button', { name: target.button, exact: true }).click();
    await expect(panel(page)).toHaveAttribute('aria-label', new RegExp(`; ${target.stage};`));
    await expectPanelGeometry(page, geometry);
    await expect(panel(page).locator('.panel-body')).toHaveCount(target.stage === 'full' ? 1 : 0);
    expect((await bridge.snapshot()).layout).toEqual(authority); // Only explicit Save writes a stage.
    const saved = await saveDraft(page, bridge, index + 2);
    expect(saved.panels[0]).toEqual({ ...original, stage: target.stage, geometry });
    await reopenLayout(page, bridge, saved);
    await expect(panel(page)).toHaveAttribute('aria-label', new RegExp(`; ${target.stage};`));
    await expectPanelGeometry(page, geometry);
    await expect(panel(page).locator('.panel-body')).toHaveCount(target.stage === 'full' ? 1 : 0);
    authority = saved;
  }
  expect(authority.panels[0]).toEqual(original); // Expanded geometry and IDs were not lost.
  expect(bridge.counts.save_layout).toBe(4);
});

test('root nesting and unnesting persist through selector actions and native restarts', async ({ page, bridge }) => {
  await createEmptySession(page, 'SIMULATED nesting round trip');
  const base = await saveDraft(page, bridge, 1);
  const original = base.panels[0];
  const nesting = page.getByRole('combobox', { name: 'Selected panel nesting', exact: true });
  await expect(nesting).toHaveValue('');
  await page.getByRole('button', { name: 'Add root container · draft only', exact: true }).click();
  const container = page.getByRole('region', { name: 'Root container 1', exact: true });
  await expect(container).toBeVisible();
  // Inspect actual options: only canvas root or a root container, never a panel/self parent.
  const choices = await nesting.locator('option').evaluateAll(options => options.map(option => option.value));
  expect(choices).toHaveLength(2);
  expect(choices[0]).toBe('');
  const containerId = choices[1];
  expect(containerId).not.toBe(original.panel_id);
  await nesting.selectOption(containerId);
  await expect(panel(page)).toContainText('Depth 1/1');
  const insideContainer = async () => {
    const child = await panel(page).boundingBox(), parent = await container.boundingBox();
    return !!child && !!parent && child.x >= parent.x && child.y >= parent.y &&
      child.x + child.width <= parent.x + parent.width && child.y + child.height <= parent.y + parent.height;
  };
  await expect.poll(insideContainer).toBe(true);
  await expectPanelGeometry(page, original.geometry);
  expect((await bridge.snapshot()).layout).toEqual(base); // Add/nest only stages intent.
  const nested = await saveDraft(page, bridge, 2);
  expect(nested.containers).toHaveLength(1);
  expect(nested.containers[0].container_id).toBe(containerId);
  // This intentionally uses an unclamped fixture, not the deferred C4 shrink case.
  expect(nested.panels[0]).toEqual({ ...original, container_id: containerId });
  await reopenLayout(page, bridge, nested);
  await expect(nesting).toHaveValue(containerId);
  await expect(panel(page)).toContainText('Depth 1/1');
  await expect.poll(insideContainer).toBe(true);
  await expectPanelGeometry(page, original.geometry);
  await nesting.selectOption('');
  await expect(panel(page)).toContainText('Depth 0/1');
  expect((await bridge.snapshot()).layout).toEqual(nested);
  const unnested = await saveDraft(page, bridge, 3);
  expect(unnested.panels[0]).toEqual(original);
  expect(unnested.containers).toEqual(nested.containers); // Unnesting does not delete the root.
  await reopenLayout(page, bridge, unnested);
  await expect(nesting).toHaveValue('');
  await expect(panel(page)).toContainText('Depth 0/1');
  await expect(container).toBeVisible();
  await expectPanelGeometry(page, original.geometry);
  expect(bridge.counts.save_layout).toBe(3);
});

test('lost committed run reply retries the original DTO once without another run or events', async ({ page, bridge, transportFaults }) => {
  await createEmptySession(page, 'SIMULATED lost run reply');
  transportFaults.dropNextReply('start_fake_run');
  const forbidden = await page.evaluate(async () => {
    try { await globalThis.__headlessInvoke({ command: 'arm_transport_fault', request: {} }); return false; }
    catch { return true; }
  });
  expect(forbidden).toBe(true); // Fault arming is trusted Node-only, never a page command.
  const originalPrompt = 'SIMULATED original immutable retry input';
  await submit(page, 'success', originalPrompt);
  const retry = page.getByRole('button', { name: 'Retry exact mutation', exact: true });
  await expect(retry).toBeEnabled();
  await expect(page.locator('.notice.error').filter({ hasText: 'execution_transport_error' }).first()).toBeVisible();
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  const committed = await bridge.snapshot();
  expect(committed.runs).toHaveLength(1);
  const eventRequest = { contract_version: 2, workspace_id: committed.workspace.workspace_id,
    run_id: committed.runs[0].run_id, after_seq: 0, through_seq: null, limit: 200 };
  const events = await bridge.invoke('list_run_events', eventRequest);
  expect(events.has_more).toBe(false);
  expect(events.events).toHaveLength(committed.runs[0].last_seq);
  expect(transportFaults.records[0].request.prompt).toBe(originalPrompt);
  expect(transportFaults.records[0].reply.ok.replayed).toBe(false);
  await composer(page).getByLabel('Prompt (local test runner)').fill('SIMULATED later draft must not replace receipt');
  await composer(page).getByLabel('Test scenario').selectOption('failure');
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  // Observe one real visible polling cycle after a lost reply, not a readiness sleep.
  await page.waitForTimeout(2200);
  expect(bridge.counts.start_fake_run).toBe(1);
  expect((await bridge.snapshot()).runs).toEqual(committed.runs);
  await expect(page.locator('.attempt-list')).not.toContainText(originalPrompt);
  await retry.click();
  await expect(retry).toHaveCount(0);
  const records = transportFaults.records;
  expect(records).toHaveLength(2);
  expect(records[1].request).toEqual(records[0].request);
  expect(records[1].reply.ok.replayed).toBe(true); // Actual native receipt, not a fabricated acknowledgement.
  expect((await bridge.snapshot()).runs).toEqual(committed.runs);
  expect(bridge.counts.start_fake_run).toBe(2);
  expect(await bridge.invoke('list_run_events', eventRequest)).toEqual(events);
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  await expect(composer(page).getByLabel('Prompt (local test runner)')).toHaveValue('SIMULATED later draft must not replace receipt');
  expect(transportFaults.evidence.injected).toBe(1);
  expect(JSON.stringify(transportFaults.evidence)).not.toContain(originalPrompt);
});

test('lost layout reply replays the captured save then explicitly saves preserved later edits', async ({ page, bridge, transportFaults }) => {
  await createEmptySession(page, 'SIMULATED lost layout reply');
  const base = await saveDraft(page, bridge, 1);
  const handle = panel(page).getByRole('button', { name: /^Focus SIMULATED lost layout reply; drag/ });
  await handle.press('ArrowRight');
  transportFaults.dropNextReply('save_layout');
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  const retry = page.getByRole('button', { name: 'Retry exact layout save', exact: true });
  await expect(retry).toBeEnabled();
  const committed = (await bridge.snapshot()).layout;
  expect(committed.revision).toBe(2);
  const firstGeometry = { ...base.panels[0].geometry, x: base.panels[0].geometry.x + 5 };
  expect(committed.panels[0].geometry).toEqual(firstGeometry);
  await handle.press('ArrowDown'); // This later intent must not alter the unresolved DTO.
  const laterGeometry = { ...firstGeometry, y: firstGeometry.y + 5 };
  await expectPanelGeometry(page, laterGeometry);
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  await expect(retry).toBeEnabled();
  expect(bridge.counts.save_layout).toBe(2);
  expect((await bridge.snapshot()).layout).toEqual(committed);
  await retry.click();
  await expect(retry).toHaveCount(0);
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'STAGED · UNSAVED');
  const records = transportFaults.records;
  expect(records).toHaveLength(2);
  expect(records[1].request).toEqual(records[0].request);
  expect(records[0].reply.ok.replayed).toBe(false);
  expect(records[1].reply.ok.replayed).toBe(true);
  expect((await bridge.snapshot()).layout).toEqual(committed); // Replay did not advance revision or save later edits.
  await expectPanelGeometry(page, laterGeometry);
  const saved = await saveDraft(page, bridge, 3);
  expect(saved.panels[0].geometry).toEqual(laterGeometry);
  const fresh = transportFaults.records[2].request;
  expect(fresh.request_id).not.toBe(records[0].request.request_id);
  expect(fresh.expected_revision).toBe(2);
  expect(fresh.layout.panels[0].geometry).toEqual(laterGeometry);
  expect(bridge.counts.save_layout).toBe(4);
  expect(transportFaults.evidence.injected).toBe(1);
  await reopenLayout(page, bridge, saved);
  await expectPanelGeometry(page, laterGeometry);
});

test('genuine native layout conflict preserves draft until explicit reapply and save', async ({ page, bridge, transportFaults }) => {
  await createEmptySession(page, 'SIMULATED revision conflict');
  const base = await saveDraft(page, bridge, 1);
  await panel(page).getByRole('button', { name: /^Focus SIMULATED revision conflict; drag/ }).press('ArrowRight');
  const intent = { ...base.panels[0].geometry, x: base.panels[0].geometry.x + 5 };
  transportFaults.conflictNextSave();
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'CONFLICT');
  await expect(page.locator('.notice.error').filter({ hasText: 'layout_revision_conflict' }).first()).toBeVisible();
  const external = transportFaults.external;
  expect(external.reply.layout.revision).toBe(2);
  expect(external.reply.layout.theme).not.toBe(base.theme);
  expect(transportFaults.records[0].reply.error.code).toBe('layout_revision_conflict');
  expect((await bridge.snapshot()).layout).toEqual(external.reply.layout);
  await expectPanelGeometry(page, intent);
  await expect(page.locator('.workbench')).toHaveAttribute('data-theme', base.theme);
  await expect(page.getByRole('button', { name: 'Save layout', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  const reapply = page.getByRole('button', { name: 'Reapply intent to refreshed base', exact: true });
  await expect(reapply).toBeEnabled();
  const writes = bridge.counts.save_layout;
  await reapply.click();
  await expect(reapply).toHaveCount(0);
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'STAGED · UNSAVED');
  expect(bridge.counts.save_layout).toBe(writes);
  expect((await bridge.snapshot()).layout).toEqual(external.reply.layout); // Reapply is not a write/merge.
  await expectPanelGeometry(page, intent);
  const saved = await saveDraft(page, bridge, 3);
  expect(saved.theme).toBe(base.theme); // Explicitly reapplied whole intent, not an automatic merge.
  expect(saved.panels[0].geometry).toEqual(intent);
  const records = transportFaults.records;
  expect(records).toHaveLength(2);
  expect(records[0].request.expected_revision).toBe(1);
  expect(records[1].request.expected_revision).toBe(2);
  expect(records[1].request.request_id).not.toBe(records[0].request.request_id);
  expect(records[1].request.request_id).not.toBe(external.request.request_id);
  expect(records[1].reply.ok.replayed).toBe(false);
  expect(bridge.counts.save_layout).toBe(4); // Baseline, competing commit, rejected stale request, explicit fresh save.
  expect(transportFaults.evidence.injected).toBe(1);
  await reopenLayout(page, bridge, saved);
  await expectPanelGeometry(page, intent);
});

test.describe('owned native paging fixture', () => {
  test.use({ nativeHistory: true });
  test('earlier history and live tail are contiguous, escaped and bounded across restart', async ({ page, bridge }, testInfo) => {
    const initial = await bridge.snapshot();
    expect(initial.sessions).toHaveLength(1);
    expect(initial.runs).toHaveLength(1);
    const run = initial.runs[0];
    expect(run.state).toBe('succeeded');
    expect(run.last_seq).toBe(604);
    const native = [];
    while (native.length < run.last_seq) {
      const next = await bridge.invoke('list_run_events', { contract_version: 2,
        workspace_id: run.workspace_id, run_id: run.run_id, after_seq: native.length,
        through_seq: run.last_seq, limit: 200 });
      expect(next.events.length).toBeGreaterThan(0);
      expect(next.events.length).toBeLessThanOrEqual(200);
      native.push(...next.events);
      expect(native.length).toBeLessThanOrEqual(604); // Fixed, bounded Node oracle; not a frontend cache.
    }
    expect(native.map(event => event.seq)).toEqual(Array.from({ length: 604 }, (_, i) => i + 1));
    await ready(page);
    await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
    const limits = await page.evaluate(async () => (await import('/src/execution/generated.ts')).limits);
    const budget = limits.frontend_text_bytes - limits.nontext_event_bytes;
    const text = event => {
      if (event.kind === 'text_delta') return event.payload.text;
      if (event.kind === 'run_created') return 'Run queued.';
      if (event.kind === 'run_state_changed' && event.payload.error === null) return `${event.payload.from} → ${event.payload.to}`;
      throw new Error('Unexpected fixed native fixture event kind.');
    };
    const expected = (after, through) => {
      const rows = native.filter(event => event.seq > after && event.seq <= through)
        .map(event => ({ seq: event.seq, text: text(event) }));
      expect(rows.length).toBeLessThanOrEqual(limits.visible_events);
      let bytes = rows.reduce((sum, row) => sum + Buffer.byteLength(row.text), 0);
      while (bytes > budget) bytes -= Buffer.byteLength(rows.shift().text);
      return rows;
    };
    const transcript = panel(page).getByRole('region', { name: 'Simulated run transcript', exact: true });
    const list = transcript.getByRole('list', { name: 'Simulated events', exact: true }).locator('li');
    const windows = [];
    const check = async (after, through, view) => {
      const rows = expected(after, through);
      await expect.poll(() => list.evaluateAll(items => items.map(item => item.value))).toEqual(rows.map(row => row.seq));
      const actual = await list.evaluateAll(items => items.map(item => ({ seq: item.value, text: item.querySelector('pre').textContent })));
      expect(actual).toEqual(rows);
      const bytes = actual.reduce((sum, row) => sum + Buffer.byteLength(row.text), 0);
      expect(actual.length).toBeLessThanOrEqual(limits.visible_events);
      expect(bytes).toBeLessThanOrEqual(budget);
      expect(new Set(actual.map(row => row.seq)).size).toBe(actual.length);
      expect(actual.map(row => row.seq)).toEqual(Array.from({ length: actual.length }, (_, i) => actual[0].seq + i));
      await expect(panel(page).locator('.transcript-controls > .badge')).toHaveText(view);
      await expect(transcript.locator('img, script')).toHaveCount(0);
      expect(await page.evaluate(() => globalThis.__historyUnsafe ?? false)).toBe(false);
      windows.push({ first: actual[0].seq, last: actual.at(-1).seq, count: actual.length, bytes, view });
      return actual;
    };
    const tailAfter = Math.max(0, run.last_seq - limits.visible_events);
    let rows = await check(tailAfter, run.last_seq, 'LIVE TAIL');
    expect(rows.length).toBeLessThan(limits.visible_events); // Text cap is actually exercised, not only event count.
    const covered = new Set(rows.map(row => row.seq));
    for (let pass = 0; rows[0].seq > 1 && pass < 8; pass++) {
      const end = rows[0].seq - 1, after = Math.max(0, end - limits.page_events);
      await transcript.getByRole('button', { name: 'Load earlier history', exact: true }).click();
      rows = await check(after, end, 'HISTORY VIEW');
      expect(rows.at(-1).seq).toBe(end);
      for (const row of rows) { expect(covered.has(row.seq)).toBe(false); covered.add(row.seq); }
      if (pass === 1) {
        await page.getByRole('button', { name: 'Refresh', exact: true }).click();
        await expect(transcript).toHaveAttribute('aria-busy', 'false');
        await check(after, end, 'HISTORY VIEW'); // A refresh cannot jump a selected history cursor.
      }
    }
    expect(rows[0].seq).toBe(1);
    expect(covered.size).toBe(run.last_seq);
    await expect(transcript.getByRole('button', { name: 'Load earlier history', exact: true })).toHaveCount(0);
    await panel(page).getByRole('button', { name: 'Return to live tail', exact: true }).click();
    await check(tailAfter, run.last_seq, 'LIVE TAIL');
    await expect(panel(page).getByRole('button', { name: 'Return to live tail', exact: true })).toBeDisabled();
    await bridge.restart();
    await page.reload();
    await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeEnabled();
    await check(tailAfter, run.last_seq, 'LIVE TAIL');
    const restored = await bridge.snapshot();
    expect(restored.runs).toEqual(initial.runs);
    expect(restored.layout).toEqual(initial.layout);
    for (const command of ['create_session', 'start_fake_run', 'cancel_run', 'save_layout']) expect(bridge.counts[command]).toBe(0);
    await testInfo.attach('bounded-history-windows', { contentType: 'application/json', body: Buffer.from(JSON.stringify({
      native_fixture: true, native_events: native.length, text_budget: budget, windows }, null, 2)) });
  });
});

test('emulated document visibility pauses UI polling and reconciles native changes without redispatch', async ({ page, bridge }, testInfo) => {
  await createEmptySession(page, 'SIMULATED visibility check');
  await submit(page, 'wait_for_cancel', 'SIMULATED visibility run');
  await expect(panel(page)).toHaveAttribute('data-state', 'running');
  const transcript = panel(page).getByRole('region', { name: 'Simulated run transcript', exact: true });
  await expect(transcript).toHaveAttribute('aria-busy', 'false');
  const originalVisibility = await page.evaluate(() => ({ hidden: document.hidden, state: document.visibilityState,
    ownHidden: Object.hasOwn(document, 'hidden'), ownState: Object.hasOwn(document, 'visibilityState') }));
  expect(originalVisibility).toEqual({ hidden: false, state: 'visible', ownHidden: false, ownState: false });
  const visibility = hidden => page.evaluate(value => {
    // Document-level test emulation only. No headless tab/OS foreground APIs.
    Object.defineProperty(document, 'hidden', { configurable: true, value });
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: value ? 'hidden' : 'visible' });
    document.dispatchEvent(new Event('visibilitychange'));
  }, hidden);
  const reads = () => ({ snapshots: bridge.counts.get_workspace_snapshot, events: bridge.counts.list_run_events });
  const observeHidden = () => page.evaluate(() => new Promise(resolve => {
    let ticks = 0;
    const timer = setInterval(() => { if (++ticks === 5) { clearInterval(timer); resolve(ticks); } }, 460);
  })); // Real JS timers remain active; no browser clock or whole-page freeze.
  const observations = [], hiddenTicks = [];
  await visibility(true);
  expect(await page.evaluate(() => document.hidden)).toBe(true);
  await expect(transcript).toHaveAttribute('aria-busy', 'false');
  const paused = reads();
  hiddenTicks.push(await observeHidden()); // Cross 2000ms while observing five real browser ticks.
  expect(reads()).toEqual(paused);
  const active = (await bridge.snapshot()).runs[0];
  expect(active.state).toBe('running'); // Hiding pauses UI reads, not the native run.
  await bridge.invoke('cancel_run', { contract_version: 2, workspace_id: active.workspace_id,
    run_id: active.run_id, request_id: randomUUID() }); // Trusted Node test changes real native state while UI is hidden.
  await expect.poll(async () => (await bridge.snapshot()).runs[0].state).toBe('cancelled');
  const terminal = (await bridge.snapshot()).runs;
  expect(reads()).toEqual(paused);
  await expect(panel(page)).toHaveAttribute('data-state', 'running'); // Retained summary is not new authority.
  observations.push({ mode: 'hidden', reads: paused, native_state: 'cancelled', rendered_state: 'running' });
  await visibility(false);
  await expect(panel(page)).toHaveAttribute('data-state', 'cancelled');
  await expect(transcript).toContainText('cancel_requested → cancelled');
  await expect(transcript).toHaveAttribute('aria-busy', 'false');
  const resumed = reads();
  expect(resumed.snapshots).toBeGreaterThan(paused.snapshots);
  expect(resumed.events).toBeGreaterThan(paused.events);
  await expect.poll(() => reads().snapshots).toBeGreaterThan(resumed.snapshots); // Visible periodic polling resumes.
  await visibility(true);
  await expect(transcript).toHaveAttribute('aria-busy', 'false');
  const secondPause = reads();
  hiddenTicks.push(await observeHidden());
  expect(reads()).toEqual(secondPause);
  await visibility(false);
  await expect.poll(() => reads().snapshots).toBeGreaterThan(secondPause.snapshots);
  await expect(panel(page)).toHaveAttribute('data-state', 'cancelled');
  expect((await bridge.snapshot()).runs).toEqual(terminal);
  expect(bridge.counts.start_fake_run).toBe(1);
  expect(bridge.counts.cancel_run).toBe(1);
  await expect(composer(page).getByLabel('Selected / historical run')).toHaveValue(active.run_id);
  await page.evaluate(() => {
    delete document.hidden; delete document.visibilityState;
    document.dispatchEvent(new Event('visibilitychange'));
  });
  expect(await page.evaluate(() => ({ hidden: document.hidden, state: document.visibilityState })))
    .toEqual({ hidden: originalVisibility.hidden, state: originalVisibility.state });
  observations.push({ mode: 'restored', reads: reads(), native_state: 'cancelled', rendered_state: 'cancelled' });
  expect(hiddenTicks).toEqual([5, 5]);
  await testInfo.attach('document-visibility-evidence', { contentType: 'application/json', body: Buffer.from(JSON.stringify({
    emulated_document: true, native_window_visibility_stub: true, hidden_observation_ms: 2300,
    cycles: 2, hidden_browser_tick_counts: hiddenTicks, observations }, null, 2)) });
});

for (const input of ['keyboard', 'virtual-pointer']) {
  test(`clamped ${input} move preserves durable full and expanded sizes after save/restart`, async ({ page, bridge }) => {
    await createEmptySession(page, 'SIMULATED clamped move');
    const base = await saveDraft(page, bridge, 1), original = base.panels[0];
    const width = await narrowPanelViewport(page, original.geometry);
    expect((await bridge.snapshot()).layout).toEqual(base); // Passive viewport changes never save/rewrite.
    const handle = panel(page).getByRole('button', { name: /^Focus SIMULATED clamped move; drag/ });
    if (input === 'keyboard') await handle.press('ArrowDown');
    else await virtualDrag(page, handle, 0, 15);
    const geometry = { ...original.geometry, x: 0, y: original.geometry.y + (input === 'keyboard' ? 5 : 15) };
    await expectPanelGeometry(page, { ...geometry, width });
    const saved = await saveDraft(page, bridge, 2);
    expect(saved.panels[0]).toEqual({ ...original, geometry, expanded_geometry: geometry });
    await reopenLayout(page, bridge, saved);
    await expectPanelGeometry(page, { ...geometry, width });
    await page.setViewportSize({ width: 1440, height: 960 });
    await expectPanelGeometry(page, geometry); // Widening reveals the preserved full size.
  });

  test(`clamped ${input} height resize preserves unedited width and commits requested height`, async ({ page, bridge }) => {
    await createEmptySession(page, 'SIMULATED clamped resize');
    const base = await saveDraft(page, bridge, 1), original = base.panels[0];
    const width = await narrowPanelViewport(page, original.geometry);
    const handle = panel(page).getByRole('button', { name: /^Resize SIMULATED clamped resize; use arrow keys/ });
    if (input === 'keyboard') await handle.press('ArrowDown');
    else await virtualDrag(page, handle, 0, 15);
    const geometry = { ...original.geometry, x: 0, height: original.geometry.height + (input === 'keyboard' ? 5 : 15) };
    await expectPanelGeometry(page, { ...geometry, width });
    const saved = await saveDraft(page, bridge, 2);
    expect(saved.panels[0]).toEqual({ ...original, geometry, expanded_geometry: geometry });
    await reopenLayout(page, bridge, saved);
    await page.setViewportSize({ width: 1440, height: 960 });
    await expectPanelGeometry(page, geometry);
    if (input === 'keyboard') await handle.press('ArrowRight');
    else await virtualDrag(page, handle, 15, 0);
    const wider = { ...geometry, width: geometry.width + (input === 'keyboard' ? 5 : 15) };
    await expectPanelGeometry(page, wider);
    const resized = await saveDraft(page, bridge, 3);
    expect(resized.panels[0]).toEqual({ ...original, geometry: wider, expanded_geometry: wider });
    await reopenLayout(page, bridge, resized);
    await expectPanelGeometry(page, wider);
  });
}

test('nesting a full panel in a smaller container preserves its expanded size on unnest and restart', async ({ page, bridge }) => {
  await createEmptySession(page, 'SIMULATED clamped nesting');
  const base = await saveDraft(page, bridge, 1), original = base.panels[0];
  await page.getByRole('button', { name: 'Add root container · draft only', exact: true }).click();
  const resize = page.getByRole('button', { name: 'Resize root container 1; use arrow keys, Shift snaps', exact: true });
  for (let i = 0; i < 15; i++) await resize.press('Shift+ArrowLeft'); // 540 -> 240.
  for (let i = 0; i < 13; i++) await resize.press('Shift+ArrowUp'); // 440 -> 180.
  const nesting = page.getByRole('combobox', { name: 'Selected panel nesting', exact: true });
  const containerId = await nesting.locator('option').nth(1).getAttribute('value');
  await nesting.selectOption(containerId);
  const geometry = { ...original.geometry, x: 24, y: 48 };
  await expectPanelGeometry(page, { ...geometry, width: 240, height: 180 });
  const nested = await saveDraft(page, bridge, 2);
  expect(nested.panels[0]).toEqual({ ...original, container_id: containerId, geometry, expanded_geometry: geometry });
  expect(nested.containers[0].geometry).toEqual({ x: 24, y: 48, width: 240, height: 180 });
  await reopenLayout(page, bridge, nested);
  await expectPanelGeometry(page, { ...geometry, width: 240, height: 180 });
  await nesting.selectOption('');
  await expectPanelGeometry(page, geometry);
  const unnested = await saveDraft(page, bridge, 3);
  expect(unnested.panels[0]).toEqual({ ...original, geometry, expanded_geometry: geometry });
  await reopenLayout(page, bridge, unnested);
  await expectPanelGeometry(page, geometry);
});

for (const input of ['keyboard', 'virtual-pointer']) {
  for (const action of ['move', 'height-resize']) {
    test(`clamped root container ${input} ${action} preserves intended dimensions`, async ({ page, bridge }) => {
      await ready(page); // No panel overlays the root's pointer handles.
      await page.getByRole('button', { name: 'Add root container · draft only', exact: true }).click();
      const base = await saveDraft(page, bridge, 1), original = base.containers[0];
      const root = page.getByRole('region', { name: 'Root container 1', exact: true });
      const check = async geometry => {
        await expect.poll(async () => {
          const box = await root.boundingBox(), canvas = await page.locator('.canvas').boundingBox();
          return box && canvas && { x: box.x - canvas.x, y: box.y - canvas.y, width: box.width, height: box.height };
        }).toEqual(geometry);
      };
      await page.setViewportSize({ width: 850, height: 960 });
      await expect.poll(async () => (await root.boundingBox()).width).toBeLessThan(original.geometry.width);
      const width = (await page.locator('.canvas').boundingBox()).width;
      await check({ ...original.geometry, x: 0, width });
      expect((await bridge.snapshot()).layout).toEqual(base);
      const handle = page.getByRole('button', { name: action === 'move'
        ? 'Move root container 1; arrow keys move, Shift snaps'
        : 'Resize root container 1; use arrow keys, Shift snaps', exact: true });
      if (input === 'keyboard') await handle.press('ArrowDown');
      else await virtualDrag(page, handle, 0, 15);
      const delta = input === 'keyboard' ? 5 : 15;
      const geometry = { ...original.geometry, x: 0,
        ...(action === 'move' ? { y: original.geometry.y + delta } : { height: original.geometry.height + delta }) };
      await check({ ...geometry, width });
      const saved = await saveDraft(page, bridge, 2);
      expect(saved.containers).toEqual([{ ...original, geometry }]);
      await reopenLayout(page, bridge, saved);
      await page.setViewportSize({ width: 1440, height: 960 });
      await check(geometry);
      if (action === 'height-resize') {
        if (input === 'keyboard') await handle.press('ArrowRight');
        else await virtualDrag(page, handle, 15, 0);
        const wider = { ...geometry, width: geometry.width + delta };
        await check(wider);
        const resized = await saveDraft(page, bridge, 3);
        expect(resized.containers).toEqual([{ ...original, geometry: wider }]);
        await reopenLayout(page, bridge, resized);
        await check(wider);
      }
      expect(bridge.counts.create_session).toBe(0);
    });
  }
}

for (const scene of ['panel', 'root-container']) {
  test(`clamped ${scene} pointer resize crossing into view preserves the visible anchor`, async ({ page, bridge }) => {
    if (scene === 'panel') await createEmptySession(page, 'SIMULATED resize anchor');
    else {
      await ready(page);
      await page.getByRole('button', { name: 'Add root container · draft only', exact: true }).click();
    }
    const base = await saveDraft(page, bridge, 1);
    const original = scene === 'panel' ? base.panels[0] : base.containers[0];
    const object = scene === 'panel' ? panel(page) : page.getByRole('region', { name: 'Root container 1', exact: true });
    await page.setViewportSize({ width: 850, height: 960 });
    await expect.poll(async () => (await object.boundingBox()).width).toBeLessThan(original.geometry.width);
    const before = await object.boundingBox(), canvas = await page.locator('.canvas').boundingBox();
    const handle = scene === 'panel' ? panel(page).getByRole('button', { name: /^Resize SIMULATED resize anchor; use arrow keys/ })
      : page.getByRole('button', { name: 'Resize root container 1; use arrow keys, Shift snaps', exact: true });
    const targetWidth = canvas.width - 20;
    await virtualDrag(page, handle, targetWidth - original.geometry.width, 0);
    const geometry = { ...original.geometry, x: before.x - canvas.x, y: before.y - canvas.y, width: targetWidth };
    await expect.poll(async () => {
      const box = await object.boundingBox(), currentCanvas = await page.locator('.canvas').boundingBox();
      return { x: box.x - currentCanvas.x, y: box.y - currentCanvas.y, width: box.width, height: box.height };
    }).toEqual(geometry);
    const saved = await saveDraft(page, bridge, 2);
    if (scene === 'panel') expect(saved.panels[0]).toEqual({ ...original, geometry, expanded_geometry: geometry });
    else expect(saved.containers[0]).toEqual({ ...original, geometry });
    await reopenLayout(page, bridge, saved);
    const restored = await object.boundingBox(), currentCanvas = await page.locator('.canvas').boundingBox();
    expect({ x: restored.x - currentCanvas.x, y: restored.y - currentCanvas.y, width: restored.width, height: restored.height }).toEqual(geometry);
  });
}

test('lost committed session reply replays the original label without duplicate sessions or dispatch', async ({ page, bridge, transportFaults }) => {
  await ready(page);
  const originalLabel = 'SIMULATED original session receipt';
  const laterLabel = 'SIMULATED later unsent session label';
  transportFaults.dropNextReply('create_session');
  await page.getByLabel('Session label', { exact: true }).fill(originalLabel);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  const retry = page.getByRole('button', { name: 'Retry exact mutation', exact: true });
  await expect(retry).toBeEnabled();
  await expect(page.locator('.notice.error').filter({ hasText: 'execution_transport_error' }).first()).toBeVisible();
  const committed = await bridge.snapshot();
  expect(committed.sessions).toHaveLength(1);
  expect(committed.sessions[0].label).toBe(originalLabel);
  expect(committed.runs).toEqual([]);
  await expect(panel(page)).toHaveCount(0); // Lost create acknowledgement never opens/dispatches automatically.
  await page.getByLabel('Session label', { exact: true }).fill(laterLabel);
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  await page.waitForTimeout(2200); // One visible poll interval must not retry a mutation.
  expect(bridge.counts.create_session).toBe(1);
  expect((await bridge.snapshot()).sessions).toEqual(committed.sessions);
  await expect(page.locator('.attempt-list')).not.toContainText(originalLabel);
  await retry.click();
  await expect(retry).toHaveCount(0);
  const records = transportFaults.records;
  expect(records).toHaveLength(2);
  expect(records[0].request.label).toBe(originalLabel);
  expect(records[1].request).toEqual(records[0].request);
  expect(records[0].reply.ok.replayed).toBe(false);
  expect(records[1].reply.ok.replayed).toBe(true);
  expect((await bridge.snapshot()).sessions).toEqual(committed.sessions);
  expect((await bridge.snapshot()).runs).toEqual([]);
  await expect(page.getByLabel('Session label', { exact: true })).toHaveValue(laterLabel);
  await page.locator('.session-roster').getByRole('button', { name: new RegExp('^' + originalLabel) }).click();
  await expect(panel(page)).toHaveCount(1);
  await expect(panel(page)).toContainText('No runs yet');
  expect(bridge.counts.create_session).toBe(2);
  expect(bridge.counts.start_fake_run).toBe(0);
  expect(transportFaults.evidence.injected).toBe(1);
  expect(JSON.stringify(transportFaults.evidence)).not.toContain(originalLabel);
});

test('lost cancellation reply retries the original run after selecting different history', async ({ page, bridge, transportFaults }) => {
  await createEmptySession(page, 'SIMULATED cancellation receipt');
  await submit(page, 'success', 'SIMULATED prior successful history');
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  const success = (await bridge.snapshot()).runs[0];
  await submit(page, 'wait_for_cancel', 'SIMULATED cancellation target');
  await expect(panel(page)).toHaveAttribute('data-state', 'running');
  const target = (await bridge.snapshot()).runs.find(run => run.state === 'running');
  expect(target.run_id).not.toBe(success.run_id);
  transportFaults.dropNextReply('cancel_run');
  await panel(page).getByRole('button', { name: 'Cancel this run', exact: true }).click();
  const retry = page.getByRole('button', { name: 'Retry exact mutation', exact: true });
  await expect(retry).toBeEnabled();
  await expect(panel(page)).toHaveAttribute('data-state', 'cancelled');
  const committed = await bridge.snapshot();
  expect(committed.runs).toHaveLength(2);
  const terminal = committed.runs.find(run => run.run_id === target.run_id);
  expect(terminal.state).toBe('cancelled');
  const eventRequest = { contract_version: 2, workspace_id: target.workspace_id, run_id: target.run_id,
    after_seq: 0, through_seq: null, limit: 200 };
  const events = await bridge.invoke('list_run_events', eventRequest);
  expect(events.has_more).toBe(false);
  expect(events.events).toHaveLength(terminal.last_seq);
  const selection = composer(page).getByLabel('Selected / historical run');
  await selection.selectOption(success.run_id); // Retry must not retarget the currently selected run.
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  await page.waitForTimeout(2200);
  expect(bridge.counts.cancel_run).toBe(1);
  expect(bridge.counts.start_fake_run).toBe(2);
  expect((await bridge.snapshot()).runs).toEqual(committed.runs);
  await retry.click();
  await expect(retry).toHaveCount(0);
  const records = transportFaults.records;
  expect(records).toHaveLength(2);
  expect(records[0].request.run_id).toBe(target.run_id);
  expect(records[1].request).toEqual(records[0].request);
  expect(records[0].reply.ok.replayed).toBe(false);
  expect(records[1].reply.ok.replayed).toBe(true);
  expect((await bridge.snapshot()).runs).toEqual(committed.runs);
  expect(await bridge.invoke('list_run_events', eventRequest)).toEqual(events);
  await expect(selection).toHaveValue(success.run_id);
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  expect(bridge.counts.cancel_run).toBe(2);
  expect(bridge.counts.start_fake_run).toBe(2);
  expect(transportFaults.evidence.injected).toBe(1);
});

test('live run board groups FIFO work, filters sessions, inspects history and restores without dispatch', async ({ page, bridge }, testInfo) => {
  await ready(page);
  const tasks = page.getByRole('button', { name: 'T Tasks RUNS', exact: true });
  const board = page.getByRole('region', { name: 'Run queue and history', exact: true });
  await tasks.click();
  await expect(board).toContainText('No runs yet');
  expect(bridge.counts.start_fake_run).toBe(0);
  await page.getByRole('button', { name: 'C Canvas', exact: true }).click();
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED board A');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  await submit(page, 'success', 'SIMULATED board history');
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  const success = (await bridge.snapshot()).runs[0];
  await submit(page, 'wait_for_cancel', 'SIMULATED board active');
  await expect(panel(page)).toHaveAttribute('data-state', 'running');
  const cycle = page.getByRole('button', { name: /^Cycle selected panel/ });
  await cycle.click(); await cycle.click();
  await expect(panel(page)).toHaveClass(/block/);
  await tasks.click();
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED board B');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(2);
  for (let i = 0; i < 2; i++) {
    await submit(page, 'success', `SIMULATED board queued ${i}`);
    // Successful send clears the draft, so Send is correctly disabled until
    // the next prompt. Wait for that acknowledgement, not an enabled button.
    await expect(composer(page).getByLabel('Prompt (local test runner)')).toHaveValue('');
    await expect.poll(async () => (await bridge.snapshot()).runs.length).toBe(3 + i);
  }
  const snapshot = await bridge.snapshot(), queued = snapshot.runs.filter(run => run.state === 'queued').sort((a, b) => a.enqueue_order - b.enqueue_order);
  expect(queued).toHaveLength(2);
  await expect(board.locator('[data-group="active"] li')).toHaveCount(1);
  await expect(board.locator('[data-group="queued"] li')).toHaveCount(2);
  await expect(board.locator('[data-group="history"] li')).toHaveCount(1);
  expect(await board.locator('[data-group="queued"] li').evaluateAll(rows => rows.map(row => row.dataset.runId))).toEqual(queued.map(run => run.run_id));
  const b = snapshot.sessions.find(session => session.label === 'SIMULATED board B');
  await board.getByLabel('Filter runs by session').selectOption(b.session_id);
  await expect(board).toContainText('2 shown / 4 retained');
  await expect(board.locator('[data-group="active"] li')).toHaveCount(0);
  await expect(board.locator('[data-group="history"] li')).toHaveCount(0);
  await board.getByLabel('Filter runs by session').selectOption('');
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  const saved = (await bridge.snapshot()).layout;
  expect(saved.active_module).toBe('tasks');
  const originalPanel = saved.panels.find(panel => panel.session_id === success.session_id);
  expect(originalPanel.stage).toBe('block');
  await testInfo.attach('headless-run-board-preview', { body: await page.screenshot(), contentType: 'image/png' });
  const before = { ...bridge.counts }, eventRequest = { contract_version: 2, workspace_id: success.workspace_id, run_id: success.run_id, after_seq: 0, through_seq: null, limit: 200 };
  const oldEvents = await bridge.invoke('list_run_events', eventRequest);
  await board.locator(`[data-run-id="${success.run_id}"]`).getByRole('button', { name: /^Inspect run/ }).click();
  await expect(board).toHaveCount(0);
  const focused = panel(page).filter({ has: page.getByRole('button', { name: /^Focus SIMULATED board A;/ }) });
  await expect(focused).not.toHaveClass(/block|compact/);
  await expect(focused.locator('.transcript')).toContainText('fake response complete');
  await expect(composer(page).getByLabel('Selected / historical run')).toHaveValue(success.run_id);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
  expect((await bridge.snapshot()).layout).toEqual(saved);
  expect(await bridge.invoke('list_run_events', eventRequest)).toEqual(oldEvents);
  // Only an explicit Save persists the reveal, retaining canonical geometry.
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 2');
  const revealed = (await bridge.snapshot()).layout.panels.find(panel => panel.session_id === success.session_id);
  expect(revealed).toEqual({ ...originalPanel, stage: 'full', geometry: originalPanel.expanded_geometry });
  await tasks.click();
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 3');
  const boardLayout = (await bridge.snapshot()).layout, starts = bridge.counts.start_fake_run, saves = bridge.counts.save_layout;
  await bridge.restart(); await page.reload();
  await expect(board).toBeVisible();
  await expect(board.locator('[data-group="active"] li')).toHaveCount(0);
  await expect(board.locator('[data-group="queued"] li')).toHaveCount(0);
  await expect(board.locator('[data-group="history"] li')).toHaveCount(4);
  const restored = await bridge.snapshot();
  expect(restored.layout).toEqual(boardLayout);
  expect(restored.runs.filter(run => run.state === 'interrupted')).toHaveLength(3);
  expect(restored.runs.find(run => run.run_id === success.run_id)).toEqual(success);
  expect(bridge.counts.start_fake_run).toBe(starts); expect(bridge.counts.save_layout).toBe(saves);
  // Compact also reveals directly to full; neither branch manufactures a panel
  // identity, overwrites expanded geometry or persists without explicit Save.
  await board.locator(`[data-run-id="${queued[0].run_id}"]`).getByRole('button', { name: /^Inspect run/ }).click();
  await expect(composer(page).getByLabel('Selected / historical run')).toHaveValue(queued[0].run_id);
  await cycle.click();
  const bPanel = panel(page).filter({ has: page.getByRole('button', { name: /^Focus SIMULATED board B;/ }) });
  await expect(bPanel).toHaveClass(/compact/);
  await tasks.click();
  await board.locator(`[data-run-id="${queued[0].run_id}"]`).getByRole('button', { name: /^Inspect run/ }).click();
  await expect(bPanel).not.toHaveClass(/compact|block/);
  await expect(bPanel.locator('.transcript')).toContainText('interrupted');
  expect((await bridge.snapshot()).layout).toEqual(boardLayout);
  expect(bridge.counts.start_fake_run).toBe(starts); expect(bridge.counts.save_layout).toBe(saves);
});

test('board Inspect creates only a staged absent panel and escapes native labels', async ({ page, bridge }) => {
  const initial = await bridge.snapshot(), workspace_id = initial.workspace.workspace_id;
  const label = '<img src=x onerror=alert(1)> SIMULATED';
  const created = await bridge.invoke('create_session', { contract_version: 2, workspace_id, request_id: randomUUID(), label , provider_id: 'fake' });
  const started = await bridge.invoke('start_fake_run', { contract_version: 2, workspace_id, session_id: created.session.session_id, request_id: randomUUID(), provider_id: 'fake', prompt: 'SIMULATED private board input', scenario: 'success' });
  await expect.poll(async () => (await bridge.snapshot()).runs[0].state).toBe('succeeded');
  const saved = await bridge.invoke('save_layout', { contract_version: 2, workspace_id, request_id: randomUUID(), expected_revision: 0, layout: { ...initial.layout, active_module: 'tasks' } });
  await ready(page);
  const board = page.getByRole('region', { name: 'Run queue and history', exact: true });
  await expect(board).toBeVisible();
  await expect(board.locator('li strong')).toHaveText(label);
  await expect(board.locator('img,script')).toHaveCount(0);
  await expect(board).not.toContainText('SIMULATED private board input');
  await expect(board).toContainText('input tokens unknown');
  await expect(panel(page)).toHaveCount(0);
  const before = { ...bridge.counts }, committed = (await bridge.snapshot()).runs;
  await board.getByRole('button', { name: /^Inspect run/ }).click();
  await expect(panel(page)).toHaveCount(1);
  await expect(panel(page).locator('.transcript')).toContainText('fake response complete');
  await expect(composer(page).getByLabel('Selected / historical run')).toHaveValue(started.run.run_id);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
  const after = await bridge.snapshot();
  expect(after.layout).toEqual(saved.layout); expect(after.runs).toEqual(committed);
});

test('usage overview follows native unknown snapshots, filters and inspects without mutation across restart', async ({ page, bridge }, testInfo) => {
  await ready(page);
  const usageNav = page.getByRole('button', { name: 'M Models & Usage RUN USAGE', exact: true });
  const overview = page.getByRole('region', { name: 'Models and run usage', exact: true });
  await usageNav.click();
  await expect(overview).toContainText('No runs yet; no usage data to total');
  await expect(overview.locator('[data-total="input_tokens-reported"]')).toHaveText('unknown');
  expect(bridge.counts.start_fake_run).toBe(0);
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED usage A');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  await submit(page, 'success', 'SIMULATED private usage input A');
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  const success = (await bridge.snapshot()).runs[0];
  const htmlLabel = '<script>SIMULATED usage B</script>';
  await page.getByLabel('Session label', { exact: true }).fill(htmlLabel);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(2);
  await submit(page, 'wait_for_cancel', 'SIMULATED private usage input B');
  // The Canvas is intentionally hidden/inert while Usage is open.
  // Observe the visible usage row, rather than querying a hidden panel button.
  await expect(overview.locator('tbody tr').first()).toContainText('running');
  await expect(overview.locator('tbody tr')).toHaveCount(2);
  await expect(overview.locator('tbody tr').first()).toContainText(htmlLabel);
  await expect(overview.locator('script,img')).toHaveCount(0);
  await expect(overview).not.toContainText('SIMULATED private usage input');
  await expect(overview).toContainText('Codex: codex_text_v1 · gpt-6.1-sol; usage is reported by Codex, not a bill.');
  await expect(overview).toContainText('Claude: not supported in Workbench.');
  for (const direction of ['input_tokens','output_tokens']) {
    for (const kind of ['reported','estimated']) await expect(overview.locator(`[data-total="${direction}-${kind}"]`)).toHaveText('unknown');
    await expect(overview.locator(`[data-total="${direction}-unknown"]`)).toHaveText('2 / 2');
  }
  const snapshot = await bridge.snapshot(), a = snapshot.sessions.find(session => session.label === 'SIMULATED usage A');
  await overview.getByLabel('Filter usage by session').selectOption(a.session_id);
  await expect(overview.locator('tbody tr')).toHaveCount(1);
  await expect(overview.locator('tbody tr')).toHaveAttribute('data-run-id', success.run_id);
  await expect(overview.locator('[data-total="input_tokens-unknown"]')).toHaveText('1 / 1');
  await overview.getByLabel('Filter usage by session').selectOption('');
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  const saved = (await bridge.snapshot()).layout;
  expect(saved.active_module).toBe('usage');
  const before = { ...bridge.counts }, eventRequest = { contract_version: 2, workspace_id: success.workspace_id, run_id: success.run_id, after_seq: 0, through_seq: null, limit: 200 };
  const oldEvents = await bridge.invoke('list_run_events', eventRequest);
  await page.getByRole('button', { name: 'Refresh', exact: true }).click();
  await page.waitForTimeout(2200);
  await testInfo.attach('headless-usage-preview', { body: await page.screenshot(), contentType: 'image/png' });
  await overview.locator(`[data-run-id="${success.run_id}"]`).getByRole('button', { name: /^Inspect usage run/ }).click();
  await expect(overview).toHaveCount(0);
  await expect(composer(page).getByLabel('Selected / historical run')).toHaveValue(success.run_id);
  const focused = panel(page).filter({ has: page.getByRole('button', { name: /^Focus SIMULATED usage A;/ }) });
  await expect(focused.locator('.transcript')).toContainText('fake response complete');
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
  expect((await bridge.snapshot()).layout).toEqual(saved);
  expect(await bridge.invoke('list_run_events', eventRequest)).toEqual(oldEvents);
  await bridge.restart(); await page.reload();
  await expect(overview).toBeVisible();
  await expect(overview.locator('tbody tr')).toHaveCount(2);
  await expect(overview).toContainText('interrupted');
  for (const direction of ['input_tokens','output_tokens']) {
    for (const kind of ['reported','estimated']) await expect(overview.locator(`[data-total="${direction}-${kind}"]`)).toHaveText('unknown');
    await expect(overview.locator(`[data-total="${direction}-unknown"]`)).toHaveText('2 / 2');
  }
  const restored = await bridge.snapshot();
  expect(restored.layout).toEqual(saved);
  expect(restored.runs.find(run => run.run_id === success.run_id)).toEqual(success);
  expect(restored.runs.filter(run => run.state === 'interrupted')).toHaveLength(1);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
});

test('event metadata selects exact runs across sessions without payload display or mutation', async ({ page, bridge }, testInfo) => {
  await ready(page);
  const nav = page.getByRole('button', { name: 'E Events & Approvals EVENT METADATA', exact: true });
  const timeline = page.getByRole('region', { name: 'Selected-run event metadata', exact: true });
  await nav.click(); await expect(timeline).toContainText('No runs yet. This view never starts work.');
  await expect(timeline.getByRole('button', { name: 'Approve — unavailable', exact: true })).toBeDisabled();
  await expect(timeline.getByRole('button', { name: 'Deny — unavailable', exact: true })).toBeDisabled();
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED metadata A');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  await submit(page, 'success', 'SIMULATED private metadata input A');
  await expect(panel(page)).toHaveAttribute('data-state', 'succeeded');
  const success = (await bridge.snapshot()).runs[0];
  const label = '<b>SIMULATED metadata B</b>';
  await page.getByLabel('Session label', { exact: true }).fill(label);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(2);
  await submit(page, 'failure', 'SIMULATED private metadata input B');
  await expect(timeline.locator('.selection')).toContainText('failed');
  const snapshot = await bridge.snapshot(), failed = snapshot.runs.find(run => run.state === 'failed');
  const rows = timeline.getByRole('list', { name: 'Loaded event metadata', exact: true }).locator('li');
  const metadata = event => ({ event_id: event.event_id, seq: event.seq, kind: event.kind, source: event.source, occurred_at: event.occurred_at });
  const readRows = () => rows.evaluateAll(items => items.map(item => ({ event_id: item.dataset.eventId, seq: item.value, kind: item.dataset.kind, source: item.dataset.source, occurred_at: item.querySelector('time').dateTime })));
  const oracle = async run => (await bridge.invoke('list_run_events', { contract_version: 2, workspace_id: run.workspace_id, run_id: run.run_id, after_seq: 0, through_seq: null, limit: 200 })).events.map(metadata);
  await expect.poll(readRows).toEqual(await oracle(failed));
  await expect(timeline.locator('.selection')).toContainText(label);
  await expect(timeline.locator('b,script,img')).toHaveCount(0);
  await expect(timeline).not.toContainText('SIMULATED private metadata input');
  await page.getByRole('button', { name: 'Save layout', exact: true }).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  const saved = (await bridge.snapshot()).layout, before = { ...bridge.counts };
  expect(saved.active_module).toBe('approvals');
  const selection = timeline.getByLabel('Run for event metadata'), successHeaders = await oracle(success);
  await selection.selectOption(success.run_id);
  await expect.poll(readRows).toEqual(successHeaders);
  await expect(timeline).not.toContainText('fake response complete');
  await expect(timeline).not.toContainText('Prepare simulated');
  await expect(timeline.locator('.high-water')).toContainText(`Native high-water #${success.last_seq}`);
  await expect(page.getByRole('button', { name: 'Save layout', exact: true })).toBeDisabled();
  await testInfo.attach('headless-event-metadata-preview', { body: await page.screenshot(), contentType: 'image/png' });
  await selection.selectOption(failed.run_id); await expect.poll(readRows).toEqual(await oracle(failed));
  await selection.selectOption(success.run_id); await expect.poll(readRows).toEqual(successHeaders);
  expect((await bridge.snapshot()).runs).toEqual(snapshot.runs);
  expect((await bridge.snapshot()).layout).toEqual(saved);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
  await bridge.restart(); await page.reload();
  await expect(timeline).toBeVisible();
  await selection.selectOption(success.run_id); await expect.poll(readRows).toEqual(successHeaders);
  const restored = await bridge.snapshot();
  expect(restored.runs).toEqual(snapshot.runs); expect(restored.layout).toEqual(saved);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
});

test.describe('owned event-metadata paging fixture', () => {
  test.use({ nativeHistory: true });
  test('event metadata shares native bounded history paging, omission and live return across restart', async ({ page, bridge }) => {
    const initial = await bridge.snapshot(), run = initial.runs[0];
    expect(run.last_seq).toBe(604);
    const headers = [];
    while (headers.length < run.last_seq) {
      const result = await bridge.invoke('list_run_events', { contract_version: 2, workspace_id: run.workspace_id, run_id: run.run_id, after_seq: headers.length, through_seq: run.last_seq, limit: 200 });
      headers.push(...result.events.map(event => ({ event_id: event.event_id, seq: event.seq, kind: event.kind, source: event.source, occurred_at: event.occurred_at })));
    }
    expect(headers).toHaveLength(604); // Node oracle only, no new frontend cache.
    await ready(page);
    await page.getByRole('button', { name: 'E Events & Approvals EVENT METADATA', exact: true }).click();
    const timeline = page.getByRole('region', { name: 'Selected-run event metadata', exact: true });
    const list = timeline.getByRole('list', { name: 'Loaded event metadata', exact: true }).locator('li');
    const earlier = timeline.getByRole('button', { name: 'Load earlier event metadata', exact: true });
    const live = timeline.getByRole('button', { name: 'Return to live event metadata', exact: true });
    const read = () => list.evaluateAll(items => items.map(item => ({ event_id: item.dataset.eventId, seq: item.value, kind: item.dataset.kind, source: item.dataset.source, occurred_at: item.querySelector('time').dateTime })));
    const check = async (first, last, mode) => {
      await expect.poll(read).toEqual(headers.filter(event => event.seq >= first && event.seq <= last));
      await expect(timeline.locator('[data-view]')).toHaveAttribute('data-view', mode);
      await expect(timeline.locator('.coverage')).toContainText(`Loaded metadata #${first}–#${last}`);
      await expect(timeline.locator('.high-water')).toContainText('Native high-water #604');
      await expect(timeline).not.toContainText('SIMULATED HISTORY');
      await expect(timeline).not.toContainText('__historyUnsafe');
      await expect(timeline.locator('img,script')).toHaveCount(0);
      expect(await page.evaluate(() => globalThis.__historyUnsafe ?? false)).toBe(false);
      expect(await list.count()).toBeLessThanOrEqual(200);
    };
    // Metadata does not bypass the SAME existing raw transcript text budget.
    await check(479,604,'live');
    const covered = new Set(Array.from({ length: 126 }, (_,i) => i+479));
    for (const [first,last] of [[353,478],[227,352],[101,226],[1,100]]) {
      await earlier.click(); await check(first,last,'history');
      for (let seq=first;seq<=last;seq++) { expect(covered.has(seq)).toBe(false); covered.add(seq); }
      if (first===227) {
        await page.getByRole('button', { name: 'Refresh', exact: true }).click();
        await expect(timeline).toHaveAttribute('aria-busy','false');
        await check(first,last,'history');
        await expect(timeline).toContainText('Later history omitted: events 353–604.');
      }
    }
    expect(covered.size).toBe(604); await expect(earlier).toBeDisabled();
    await live.click(); await check(479,604,'live'); await expect(live).toBeDisabled();
    for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(0);
    await page.getByRole('button', { name: 'Save layout', exact: true }).click();
    await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 2');
    const saved = (await bridge.snapshot()).layout;
    await bridge.restart(); await page.reload(); await expect(timeline).toBeVisible(); await check(479,604,'live');
    const restored = await bridge.snapshot(); expect(restored.layout).toEqual(saved); expect(restored.runs).toEqual(initial.runs);
    expect(bridge.counts.create_session).toBe(0); expect(bridge.counts.start_fake_run).toBe(0); expect(bridge.counts.cancel_run).toBe(0); expect(bridge.counts.save_layout).toBe(1);
  });
});

test('workspace diagnostics follows loaded counts and layout intent through read-only refresh and restart', async ({ page, bridge }, testInfo) => {
  await ready(page);
  const nav = page.getByRole('button', { name: 'L Logs & Diagnostics WORKSPACE HEALTH', exact: true });
  const diagnostics = page.getByRole('region', { name: 'Workspace diagnostic metadata', exact: true });
  const metric = key => diagnostics.locator(`[data-metric="${key}"]`);
  await nav.click();
  await expect(metric('status')).toHaveText('ready'); await expect(metric('execution')).toHaveText('available');
  for (const key of ['sessions','runs','queued','active','terminal']) await expect(metric(key)).toHaveText('0');
  expect(bridge.counts.create_session).toBe(0); expect(bridge.counts.start_fake_run).toBe(0);
  await page.getByLabel('Session label', { exact: true }).fill('SIMULATED diagnostics session');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  await submit(page, 'success', 'SIMULATED private diagnostics success');
  await expect(panel(page)).toHaveAttribute('data-state','succeeded');
  await submit(page, 'wait_for_cancel', 'SIMULATED private diagnostics active');
  await expect(panel(page)).toHaveAttribute('data-state','running');
  for (let i=0;i<2;i++) {
    await submit(page,'success',`SIMULATED private diagnostics queued ${i}`);
    await expect(composer(page).getByLabel('Prompt (local test runner)')).toHaveValue('');
  }
  await expect.poll(async () => (await bridge.snapshot()).runs.find(run => run.state==='running')?.last_seq).toBe(5);
  const snapshot = await bridge.snapshot();
  await expect(metric('workspace_id')).toHaveText(snapshot.workspace.workspace_id);
  for (const [key,value] of Object.entries({sessions:1,runs:4,queued:2,active:1,terminal:1})) await expect(metric(key)).toHaveText(String(value));
  await expect(metric('layout_dirty')).toHaveText('yes'); await expect(metric('saved_revision')).toHaveText('0');
  await page.getByRole('button', { name:'Add root container · draft only',exact:true }).click();
  await page.getByRole('button', {name:'Open properties'}).click();
  await page.getByRole('tab', {name:'Appearance'}).click();
  await page.getByRole('combobox', {name:'Theme',exact:true}).selectOption('light');
  await page.getByRole('button', {name:'Close properties'}).click();
  await expect(metric('draft_panels')).toHaveText('1'); await expect(metric('draft_containers')).toHaveText('1');
  expect((await bridge.snapshot()).layout).toEqual(snapshot.layout);
  await page.getByRole('button',{name:'Save layout',exact:true}).click();
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  await expect(metric('saved_revision')).toHaveText('1'); await expect(metric('layout_dirty')).toHaveText('no');
  const saved = (await bridge.snapshot()).layout, before = {...bridge.counts};
  expect(saved.active_module).toBe('diagnostics');
  await expect(diagnostics).not.toContainText('SIMULATED private diagnostics');
  await expect(diagnostics).not.toContainText('SIMULATED diagnostics session');
  await expect(diagnostics).not.toContainText('Summary refresh:');
  await expect(diagnostics).toContainText('Worker mailbox depth, CPU/GPU/memory');
  await diagnostics.getByRole('button',{name:'Refresh workspace summaries',exact:true}).click();
  await page.waitForTimeout(2200);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
  expect((await bridge.snapshot()).layout).toEqual(saved);
  await testInfo.attach('headless-workspace-diagnostics-preview',{body:await page.screenshot(),contentType:'image/png'});
  await bridge.restart(); await page.reload(); await expect(diagnostics).toBeVisible();
  for (const [key,value] of Object.entries({sessions:1,runs:4,queued:0,active:0,terminal:4,saved_revision:1,draft_panels:1,draft_containers:1})) await expect(metric(key)).toHaveText(String(value));
  await expect(metric('layout_dirty')).toHaveText('no'); await expect(metric('status')).toHaveText('ready');
  await expect.poll(async () => Number(await metric('history_rows').textContent())).toBeGreaterThan(0);
  const restored = await bridge.snapshot(); expect(restored.layout).toEqual(saved); expect(restored.runs.filter(run=>run.state==='interrupted')).toHaveLength(3);
  for (const command of ['create_session','start_fake_run','cancel_run','save_layout']) expect(bridge.counts[command]).toBe(before[command]);
});

test('diagnostics unresolved metadata omits private bodies and Refresh never repeats a lost-reply mutation', async ({page,bridge,transportFaults}) => {
  await createEmptySession(page,'SIMULATED private diagnostics label');
  await page.getByRole('button',{name:'L Logs & Diagnostics WORKSPACE HEALTH',exact:true}).click();
  const diagnostics = page.getByRole('region',{name:'Workspace diagnostic metadata',exact:true});
  transportFaults.dropNextReply('start_fake_run');
  const privatePrompt='SIMULATED PRIVATE_DIAGNOSTICS_PROMPT_C19';
  await submit(page,'success',privatePrompt);
  const retry=page.getByRole('button',{name:'Retry exact mutation',exact:true});
  await expect(retry).toBeEnabled();
  await expect(diagnostics.locator('[data-metric="attempts"]')).toHaveText('1');
  await expect(diagnostics.getByRole('region',{name:'Unresolved request metadata',exact:true})).toContainText('start_fake_run');
  const record=transportFaults.records[0];
  await expect(diagnostics).toContainText(record.request.request_id);
  await expect(diagnostics).not.toContainText(privatePrompt);
  await expect(diagnostics).not.toContainText('SIMULATED private diagnostics label');
  await expect(diagnostics.getByRole('button')).toHaveCount(1); // Refresh only, never Retry.
  await diagnostics.getByRole('button',{name:'Refresh workspace summaries',exact:true}).click(); await page.waitForTimeout(2200);
  expect(bridge.counts.start_fake_run).toBe(1); expect(bridge.counts.create_session).toBe(1); expect(bridge.counts.save_layout).toBe(0); expect(bridge.counts.cancel_run).toBe(0);
  const committed=await bridge.snapshot(); expect(committed.runs).toHaveLength(1); expect(committed.runs[0].state).toBe('succeeded');
  await retry.click(); await expect(retry).toHaveCount(0); await expect(diagnostics.locator('[data-metric="attempts"]')).toHaveText('0');
  const records=transportFaults.records; expect(records).toHaveLength(2); expect(records[1].request).toEqual(records[0].request); expect(records[1].reply.ok.replayed).toBe(true);
  expect((await bridge.snapshot()).runs).toEqual(committed.runs); expect(bridge.counts.start_fake_run).toBe(2); expect(bridge.counts.save_layout).toBe(0);
});

test('snap grid appears only while Shift is held during a pointer drag, not on any Shift press', async ({ page, bridge }) => {
  await ready(page);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  const canvas = page.locator('.canvas');
  const handle = panel(page).first().locator('.drag-handle');
  // Shift alone, with no drag: no grid.
  await page.keyboard.down('Shift');
  await expect(canvas).not.toHaveClass(/\bgrid\b/);
  await page.keyboard.up('Shift');
  const box = await handle.boundingBox();
  const x = box.x + box.width / 2, y = box.y + box.height / 2;
  // Dragging without Shift: no grid.
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 30, y + 10, { steps: 2 });
  await expect(canvas).not.toHaveClass(/\bgrid\b/);
  // Shift pressed mid-drag: grid appears while the drag continues.
  await page.keyboard.down('Shift');
  await page.mouse.move(x + 60, y + 20, { steps: 2 });
  await expect(canvas).toHaveClass(/\bgrid\b/);
  // Releasing the drag ends it, even while Shift is still held.
  await page.mouse.up();
  await expect(canvas).not.toHaveClass(/\bgrid\b/);
  await page.keyboard.up('Shift');
  expect(bridge.counts.start_fake_run).toBe(0);
});

test('snap grid also appears when Shift was already held before the pointer went down', async ({ page, bridge }) => {
  void bridge; // the fixture isolates the native state for this test
  await ready(page);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  const canvas = page.locator('.canvas');
  const box = await panel(page).first().locator('.drag-handle').boundingBox();
  const x = box.x + box.width / 2, y = box.y + box.height / 2;
  await page.keyboard.down('Shift');
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 40, y + 15, { steps: 3 });
  await expect(canvas).toHaveClass(/\bgrid\b/);
  await page.mouse.up();
  await page.keyboard.up('Shift');
  await expect(canvas).not.toHaveClass(/\bgrid\b/);
});

test('session panels and root containers cannot be dragged or resized out of the visible canvas', async ({ page, bridge }) => {
  void bridge;
  await ready(page);
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  const canvas = page.locator('.canvas');
  const inside = async locator => {
    const c = await canvas.boundingBox();
    const b = await locator.boundingBox();
    return { left: b.x - c.x, top: b.y - c.y, right: c.x + c.width - (b.x + b.width), bottom: c.y + c.height - (b.y + b.height) };
  };
  const dragBy = async (handle, dx, dy) => {
    const b = await handle.boundingBox();
    await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
    await page.mouse.down();
    await page.mouse.move(b.x + b.width / 2 + dx, b.y + b.height / 2 + dy, { steps: 4 });
    await page.mouse.up();
  };
  const handle = panel(page).first().locator('.drag-handle');
  for (const [dx, dy] of [[3000, 3000], [-4000, -4000], [3000, -4000], [-4000, 3000]]) {
    await dragBy(handle, dx, dy);
    const m = await inside(panel(page).first());
    expect(Math.min(m.left, m.top, m.right, m.bottom), `panel after dragging ${dx},${dy}: ${JSON.stringify(m)}`).toBeGreaterThanOrEqual(-0.5);
  }
  // Resizing from the corner cannot push the panel past the edge either.
  await dragBy(handle, -2000, -2000);
  const grip = panel(page).first().locator('.resize');
  await dragBy(grip, 3000, 3000);
  const resized = await inside(panel(page).first());
  expect(Math.min(resized.left, resized.top, resized.right, resized.bottom), JSON.stringify(resized)).toBeGreaterThanOrEqual(-0.5);
  // A root container as well.
  await page.getByRole('button', { name: /root container/i }).first().click();
  const container = page.locator('section.nest').first();
  await expect(container).toBeVisible();
  const cHandle = container.locator('.drag-handle');
  for (const [dx, dy] of [[3000, 3000], [-4000, -4000]]) {
    await dragBy(cHandle, dx, dy);
    const m = await inside(container);
    expect(Math.min(m.left, m.top, m.right, m.bottom), `container after dragging ${dx},${dy}: ${JSON.stringify(m)}`).toBeGreaterThanOrEqual(-0.5);
  }
});

test('scroll bars are square, thin and follow the theme, with hover and active states', async ({ page, bridge }) => {
  void bridge;
  await ready(page);
  const read = theme => page.evaluate(t => {
    document.querySelector('.workbench').dataset.theme = t;
    const el = document.querySelector('aside');
    const part = (pseudo, state = '') => getComputedStyle(el, `::-webkit-scrollbar${pseudo}${state}`);
    return { width: part('').width, track: part('-track').backgroundColor, thumb: part('-thumb').backgroundColor, hover: part('-thumb', ':hover').backgroundColor, active: part('-thumb', ':active').backgroundColor, radius: part('-thumb').borderTopLeftRadius };
  }, theme);
  const dark = await read('dark');
  const light = await read('light');
  const contrast = await read('contrast');
  for (const s of [dark, light, contrast]) {
    expect(s.width).toBe('11px');
    expect(s.radius).toBe('0px');
    expect(new Set([s.thumb, s.hover]).size, 'hover looks different from rest').toBe(2);
  }
  expect(dark.track).toBe('rgb(7, 9, 10)');
  expect(light.track).toBe('rgb(232, 231, 221)');
  expect(contrast.track).toBe('rgb(0, 0, 0)');
  expect(contrast.thumb).toBe('rgb(255, 255, 255)');
  expect(new Set([dark.thumb, light.thumb, contrast.thumb]).size).toBe(3);
});

test('the right sidebar starts with only the operative controls open, shows a summary on every collapsed section and remembers what was opened', async ({ page, bridge }) => {
  void bridge;
  // A first run: nothing remembered yet (cleared once, so a reload keeps what the page stored).
  await page.addInitScript(() => { if (!sessionStorage.getItem('first-run')) { sessionStorage.setItem('first-run', '1'); localStorage.removeItem('aw.sections'); } });
  await ready(page);
  const section = name => page.getByRole('region', { name, exact: true });
  const header = name => section(name).getByRole('button').first();
  // Open by default: starting a session and writing a prompt.
  await expect(header('New session')).toHaveAttribute('aria-expanded', 'true');
  await expect(header('Prompt')).toHaveAttribute('aria-expanded', 'true');
  await expect(page.getByLabel('Session label', { exact: true })).toBeVisible();
  // Collapsed by default, each with a one-line summary.
  for (const name of ['Sessions', 'Run details', 'Layout draft']) await expect(header(name)).toHaveAttribute('aria-expanded', 'false');
  await expect(header('Sessions')).toContainText('0 of 16');
  await expect(header('Layout draft')).toContainText('0 panels · 0 containers');
  await expect(header('Run details')).toContainText('0 of ');
  await expect(page.locator('.session-roster')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Add root container · draft only' })).toHaveCount(0);
  // The simulated wording is gone from the visible chrome (the test runner is still selectable).
  await expect(page.locator('aside')).not.toContainText('SIMULATED');
  await expect(page.locator('.rail')).not.toContainText('SIMULATED');
  // Create a session, open the roster: it lists it. The choice survives a reload.
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(panel(page)).toHaveCount(1);
  await expect(header('Sessions')).toContainText('1 of 16');
  await expect(header('Layout draft')).toContainText('1 panel · 0 containers');
  await header('Sessions').click();
  await expect(page.locator('.session-roster li')).toHaveCount(1);
  await page.reload();
  await expect(header('Sessions')).toHaveAttribute('aria-expanded', 'true');
  await expect(header('Layout draft')).toHaveAttribute('aria-expanded', 'false');
  await header('Sessions').click();
  await expect(page.locator('.session-roster')).toHaveCount(0);
});

test('both sidebars collapse to compact and flat strips, the workspace takes the room, and the state is remembered', async ({ page, bridge }) => {
  void bridge;
  await page.addInitScript(() => { if (!sessionStorage.getItem('bars-first-run')) { sessionStorage.setItem('bars-first-run', '1'); localStorage.removeItem('aw.sidebars'); } });
  await ready(page);
  const canvasWidth = async () => (await page.locator('.canvas').boundingBox()).width;
  const rail = page.locator('nav.rail');
  const aside = page.locator('aside');
  const full = await canvasWidth();
  expect((await rail.boundingBox()).width).toBeGreaterThan(150);
  // Sessions panel: full -> compact -> flat.
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  await expect(aside).toHaveClass(/\bcompact\b/);
  expect((await aside.boundingBox()).width).toBeLessThan(60);
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toHaveCount(0);
  const wide = await canvasWidth();
  expect(wide).toBeGreaterThan(full + 200);
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  await expect(aside).toHaveClass(/\bflat\b/);
  expect((await aside.boundingBox()).width).toBeLessThan(30);
  // Module list: full -> compact (icons only, still usable).
  await page.getByRole('button', { name: 'Collapse the module list' }).click();
  await expect(rail).toHaveClass(/\bcompact\b/);
  expect((await rail.boundingBox()).width).toBeLessThan(60);
  await expect(rail.locator('.nav-label').first()).toBeHidden();
  expect(await canvasWidth()).toBeGreaterThan(wide + 100);
  await rail.getByRole('button', { name: 'Terminals' }).click();
  await expect(page.locator('section.terminals')).toBeVisible();
  // Remembered across a reload.
  await page.reload();
  await expect(rail).toHaveClass(/\bcompact\b/);
  await expect(aside).toHaveClass(/\bflat\b/);
  // Expanding steps back to full and the workspace gives the room back.
  await page.getByRole('button', { name: 'Expand the sessions panel' }).click();
  await page.getByRole('button', { name: 'Expand the sessions panel' }).click();
  await page.getByRole('button', { name: 'Expand the module list' }).click();
  await expect(aside).not.toHaveClass(/\b(compact|flat)\b/);
  await expect(rail).not.toHaveClass(/\b(compact|flat)\b/);
  await expect(page.getByRole('button', { name: 'Create session', exact: true })).toBeVisible();
  expect(Math.abs((await canvasWidth()) - full)).toBeLessThan(2);
});

test('a collapsed sidebar can be dragged to the top or bottom edge like a taskbar, and everything else reflows', async ({ page, bridge }) => {
  void bridge;
  await page.addInitScript(() => { if (!sessionStorage.getItem('bars-first-run')) { sessionStorage.setItem('bars-first-run', '1'); localStorage.removeItem('aw.sidebars'); } });
  await ready(page);
  const shell = page.locator('.shell');
  const rail = page.locator('nav.rail');
  const aside = page.locator('aside');
  await page.getByRole('button', { name: 'Collapse the module list' }).click();
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  const before = await page.locator('.canvas').boundingBox();
  const area = await shell.boundingBox();
  // Drag the module list's grip to the top edge.
  let grip = await rail.getByRole('button', { name: /^Move the module list/ }).boundingBox();
  await page.mouse.move(grip.x + grip.width / 2, grip.y + grip.height / 2);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width / 2, area.y + 20, { steps: 6 });
  await expect(page.locator('.dock-zones .dz.top')).toHaveClass(/\bon\b/);
  await page.mouse.up();
  await expect(rail).toHaveClass(/\bh\b/);
  const bar = await rail.boundingBox();
  expect(Math.abs(bar.y - area.y)).toBeLessThan(2);
  expect(bar.width).toBeGreaterThan(area.width - 4);
  expect(bar.height).toBeLessThan(60);
  // The modules are still reachable in the strip, and the workspace sits below it.
  await expect(rail.getByRole('button', { name: 'Canvas' })).toBeVisible();
  const canvas = await page.locator('.canvas').boundingBox();
  expect(canvas.y).toBeGreaterThanOrEqual(bar.y + bar.height - 1);
  expect(canvas.x).toBeLessThan(before.x - 20); // the freed left strip went to the workspace
  // The sessions panel to the bottom, by keyboard (Down from its side).
  await aside.getByRole('button', { name: /^Move the sessions panel/ }).focus();
  await page.keyboard.press('ArrowDown');
  await expect(aside).toHaveClass(/\bh\b/);
  const bottom = await aside.boundingBox();
  expect(Math.abs(bottom.y + bottom.height - (area.y + area.height))).toBeLessThan(2);
  expect(bottom.width).toBeGreaterThan(area.width - 4);
  // Nothing is covered: the workspace is between the two strips.
  const mid = await page.locator('.canvas').boundingBox();
  expect(mid.y + mid.height).toBeLessThanOrEqual(bottom.y + 1);
  expect(mid.width).toBeGreaterThan(area.width - 4);
  // Remembered across a reload.
  await page.reload();
  await expect(rail).toHaveClass(/\bh\b/);
  await expect(aside).toHaveClass(/\bh\b/);
  // Expanding a docked bar puts it back on its side, full.
  await page.getByRole('button', { name: 'Expand the sessions panel' }).click();
  await expect(aside).not.toHaveClass(/\b(h|compact|flat)\b/);
  expect((await aside.boundingBox()).x).toBeGreaterThan(area.x + area.width / 2);
  // Up from the top returns a docked bar to its side too.
  await rail.getByRole('button', { name: /^Move the module list/ }).focus();
  await page.keyboard.press('ArrowDown');
  await expect(rail).not.toHaveClass(/\bh\b/);
});

test('pressing and holding anywhere on a sidebar picks it up to drag it to an edge; quick clicks and moving presses behave as before', async ({ page, bridge }) => {
  void bridge;
  await page.addInitScript(() => { if (!sessionStorage.getItem('bars-first-run')) { sessionStorage.setItem('bars-first-run', '1'); localStorage.removeItem('aw.sidebars'); } });
  await ready(page);
  const shell = await page.locator('.shell').boundingBox();
  const rail = page.locator('nav.rail');
  const aside = page.locator('aside');
  // A quick click on a module still works (and no drag starts).
  await rail.getByRole('button', { name: /Terminals/ }).click();
  await expect(page.locator('section.terminals')).toBeVisible();
  await rail.getByRole('button', { name: /^C\s*Canvas/ }).click();
  // A press that moves before the hold time is not a pickup.
  let box = await rail.getByRole('button', { name: /Logs/ }).boundingBox();
  await page.mouse.move(box.x + 20, box.y + 10);
  await page.mouse.down();
  await page.mouse.move(box.x + 60, box.y + 80, { steps: 3 });
  await page.waitForTimeout(450);
  await expect(page.locator('.dock-zones')).toHaveCount(0);
  await page.mouse.up();
  await expect(rail).not.toHaveClass(/\bh\b/);
  // Holding in a text field never picks the sidebar up.
  box = await page.getByLabel('Session label', { exact: true }).boundingBox();
  await page.mouse.move(box.x + 30, box.y + 10);
  await page.mouse.down();
  await page.waitForTimeout(450);
  await expect(page.locator('.dock-zones')).toHaveCount(0);
  await page.mouse.up();
  // Hold on a module button, then drag to the top edge: the sidebar docks (collapsed to compact) and the button is not clicked.
  box = await rail.getByRole('button', { name: /Terminals/ }).boundingBox();
  await page.mouse.move(box.x + 30, box.y + 10);
  await page.mouse.down();
  await page.waitForTimeout(450);
  await expect(page.locator('.dock-zones')).toHaveCount(1);
  await page.mouse.move(shell.x + shell.width / 2, shell.y + 15, { steps: 6 });
  await expect(page.locator('.dock-zones .dz.top')).toHaveClass(/\bon\b/);
  await page.mouse.up();
  await expect(rail).toHaveClass(/\bh\b/);
  await expect(rail).toHaveClass(/\bcompact\b/);
  await expect(page.locator('section.terminals')).toHaveCount(0);
  // Hold on empty space of the sessions panel and drag it to the bottom.
  box = await aside.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height - 12);
  await page.mouse.down();
  await page.waitForTimeout(450);
  await page.mouse.move(shell.x + shell.width / 2, shell.y + shell.height - 12, { steps: 6 });
  await page.mouse.up();
  await expect(aside).toHaveClass(/\bh\b/);
  // And a docked strip can be held and dragged back to its side.
  box = await aside.boundingBox();
  await page.mouse.move(box.x + 40, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(450);
  await page.mouse.move(shell.x + shell.width / 2, shell.y + shell.height / 2, { steps: 6 });
  await page.mouse.up();
  await expect(aside).not.toHaveClass(/\bh\b/);
  await expect(aside).toHaveClass(/\bcompact\b/);
});

test('the toolbar is gone: the layout badge, Save layout and Refresh sit at the foot of the sessions bar and stay there on every tab', async ({ page, bridge }) => {
  void bridge;
  await page.addInitScript(() => { if (!sessionStorage.getItem('bars-first-run')) { sessionStorage.setItem('bars-first-run', '1'); localStorage.removeItem('aw.sidebars'); } });
  await ready(page);
  await expect(page.getByText('Your working surface')).toHaveCount(0);
  await expect(page.locator('.toolbar')).toHaveCount(0);
  const aside = page.locator('aside');
  const foot = aside.locator('.aside-foot');
  await expect(foot.locator('.layout-actions .badge')).toHaveAttribute('title', /SAVED/);
  await expect(foot.locator('.layout-actions .badge')).toHaveText(/^\d+$/);
  await expect(foot.getByRole('button', { name: 'Save layout' })).toBeVisible();
  await expect(foot.getByRole('button', { name: 'Refresh' })).toBeVisible();
  // At the bottom edge of the bar, and the bar's own content scrolls above it.
  const a = await aside.boundingBox();
  const f = await foot.boundingBox();
  expect(Math.abs(f.y + f.height - (a.y + a.height))).toBeLessThan(2);
  // Still there on the Terminals and Inspector tabs.
  await page.locator('nav.rail').getByRole('button', { name: /Terminals/ }).click();
  await expect(foot.getByRole('button', { name: 'Save layout' })).toBeVisible();
  await page.locator('nav.rail').getByRole('button', { name: /Inspector/ }).click();
  await expect(foot.getByRole('button', { name: 'Save layout' })).toBeVisible();
  await page.locator('nav.rail').getByRole('button', { name: /^C\s*Canvas/ }).click();
  // Collapsed, the bar keeps a Save button that lights up when there is something to save.
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  const save = aside.getByRole('button', { name: 'Save layout' });
  await expect(save).toBeVisible();
  await expect(save).not.toHaveClass(/\bpending\b/);
  await page.getByRole('button', { name: 'Expand the sessions panel' }).click();
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(foot.locator('.layout-actions .badge')).toHaveAttribute('title', 'STAGED · UNSAVED');
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  await expect(save).toHaveClass(/\bpending\b/);
  await save.click();
  await page.getByRole('button', { name: 'Expand the sessions panel' }).click();
  await expect(foot.locator('.layout-actions .badge')).toHaveAttribute('title', /SAVED/);
  await expect(foot.locator('.layout-actions .badge')).toHaveText(/^\d+$/);
});

test('the layout controls are icons, the saved revision shows as just its number, and all three follow a collapsed or docked sidebar', async ({ page, bridge }) => {
  void bridge;
  await page.addInitScript(() => { if (!sessionStorage.getItem('bars-first-run')) { sessionStorage.setItem('bars-first-run', '1'); localStorage.removeItem('aw.sidebars'); } });
  await ready(page);
  const aside = page.locator('aside');
  const badge = aside.locator('.layout-actions .badge');
  const save = aside.getByRole('button', { name: 'Save layout' });
  const refresh = aside.getByRole('button', { name: 'Refresh' });
  // Icons, not words.
  await expect(save.locator('svg')).toHaveCount(1);
  await expect(refresh.locator('svg')).toHaveCount(1);
  await expect(save).toHaveText('');
  await expect(refresh).toHaveText('');
  await expect(badge).toHaveText(/^\d+$/);
  await expect(badge).toHaveAttribute('title', /^SAVED · REV \d+$/);
  // Unsaved changes say so, and the Save icon is marked.
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(badge).toHaveAttribute('title', 'STAGED · UNSAVED');
  await expect(save).toHaveClass(/\bpending\b/);
  await save.click();
  await expect(badge).toHaveText(/^\d+$/);
  // Collapsed to the side: a column of the same three controls, inside the strip.
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  const strip = await aside.boundingBox();
  for (const control of [badge, save, refresh]) {
    const b = await control.boundingBox();
    expect(b.x).toBeGreaterThanOrEqual(strip.x - 1);
    expect(b.x + b.width).toBeLessThanOrEqual(strip.x + strip.width + 1);
  }
  const col = [await badge.boundingBox(), await save.boundingBox(), await refresh.boundingBox()];
  expect(col[0].y).toBeLessThan(col[1].y);
  expect(col[1].y).toBeLessThan(col[2].y);
  // Flat: icons only (still two buttons), and Refresh works.
  await page.getByRole('button', { name: 'Collapse the sessions panel' }).click();
  await expect(aside).toHaveClass(/\bflat\b/);
  await expect(save).toBeVisible();
  await expect(refresh).toBeVisible();
  // Docked along the bottom: the three sit in a row.
  await aside.getByRole('button', { name: /^Move the sessions panel/ }).focus();
  await page.keyboard.press('ArrowDown');
  await expect(aside).toHaveClass(/\bh\b/);
  const row = [await badge.boundingBox(), await save.boundingBox(), await refresh.boundingBox()];
  expect(Math.abs(row[1].y - row[2].y)).toBeLessThan(3);
  expect(row[1].x).toBeLessThan(row[2].x);
  const bar = await aside.boundingBox();
  for (const b of row) expect(b.y + b.height).toBeLessThanOrEqual(bar.y + bar.height + 1);
  await refresh.click();
});
