// Terminals view in a fresh headless browser against an in-page fake terminal service. No desktop app,
// physical input or real terminal host is involved: this is the only way the 16/32-pane behaviour is
// exercised automatically (layout geometry, splitting, tiling, focus keys, dividers, polling load).
import { test, expect } from './fixtures.mjs';

const hex = n => (0x1000 + n).toString(16).padStart(8, '0');
const fakeSessions = count => Array.from({ length: count }, (_, i) => ({
  id: hex(i), label: `T${i + 1}`, state: 'running', persistent: true, cols: 100, rows: 30, host_pid: 1000 + i
}));

async function openTerminals(page, count) {
  await page.goto('/');
  await page.evaluate(sessions => { globalThis.__fakeTerminals.sessions = sessions; }, fakeSessions(count));
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(page.locator('section.terminals')).toBeVisible();
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(count);
}
const panes = page => page.locator('.panes .pane');
const boxes = async page => (await panes(page).evaluateAll(els => els.map(e => { const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height, focused: e.classList.contains('focused') }; })));
const overlap = (a, b) => Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x) > 1 && Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y) > 1;

test('Tile all shows 32 terminals at once, side by side without overlap, and polling stays light', async ({ page }) => {
  await openTerminals(page, 32);
  await page.getByRole('button', { name: /^Tile all \(32\)/ }).click();
  await expect(panes(page)).toHaveCount(32);
  await expect(page.locator('.panes .pane-body canvas, .panes .pane-body .xterm')).toHaveCount(32);
  const all = await boxes(page);
  const area = await page.locator('.panes').boundingBox();
  for (const b of all) {
    expect(b.w).toBeGreaterThan(50);
    expect(b.h).toBeGreaterThan(30);
    expect(b.x).toBeGreaterThanOrEqual(area.x - 1);
    expect(b.y).toBeGreaterThanOrEqual(area.y - 1);
    expect(b.x + b.w).toBeLessThanOrEqual(area.x + area.width + 1);
    expect(b.y + b.h).toBeLessThanOrEqual(area.y + area.height + 1);
  }
  for (let i = 0; i < all.length; i++) for (let j = i + 1; j < all.length; j++) expect(overlap(all[i], all[j]), `panes ${i} and ${j} overlap`).toBe(false);
  await expect(page.locator('.pane-count')).toHaveText('32 panes');
  // The arrangement was saved natively (in the fake), in the current format.
  const saved = await page.evaluate(() => globalThis.__fakeTerminals.layout);
  expect(saved.version).toBe(2);
  // Idle polling backs off: after a warm-up, 32 idle panes make far fewer reads than a fixed 70 ms poll would.
  await page.waitForTimeout(4000);
  const before = await page.evaluate(() => globalThis.__fakeTerminals.reads);
  await page.waitForTimeout(3000);
  const reads = (await page.evaluate(() => globalThis.__fakeTerminals.reads)) - before;
  expect(reads, `${reads} reads in 3 s`).toBeLessThan(500); // fixed 70 ms polling would be ~1370
});

test('the tmux prefix opens and closes the Files panel with f', async ({ page }) => {
  await openTerminals(page, 1);
  // The shared fixture starts with Sessions open (most tests drive it); f switches the one panel slot to Files.
  const files = page.getByRole('complementary', { name: 'Files panel' });
  await expect(files).toHaveCount(0);
  await page.locator('.terminal-list li').first().locator('button.pick').click();
  await page.locator('.pane .xterm').first().click();
  await page.keyboard.press('Control+b');
  await page.keyboard.press('f');
  await expect(files).toBeVisible();
  await page.keyboard.press('Control+b');
  await page.keyboard.press('f');
  await expect(files).toHaveCount(0);
});

test('splitting, focus keys, dividers and closing work like tmux, and the layout is remembered across a reload', async ({ page }) => {
  await openTerminals(page, 3);
  await expect(panes(page)).toHaveCount(1);
  // Show T1 in the only pane, split right, show T2 there, split that pane down, show T3.
  await page.locator('.terminal-list li').nth(0).locator('button.pick').click();
  await page.getByRole('button', { name: 'Split pane 1 to the right' }).click();
  await expect(panes(page)).toHaveCount(2);
  await page.locator('.terminal-list li').nth(1).locator('button.pick').click();
  await page.getByRole('button', { name: 'Split pane 2 downwards' }).click();
  await expect(panes(page)).toHaveCount(3);
  await page.locator('.terminal-list li').nth(2).locator('button.pick').click();
  await expect(page.locator('.pane-count')).toHaveText('3 panes');
  let b = await boxes(page);
  // pane 1 occupies the left half; panes 2 and 3 stack on the right.
  expect(b[0].x).toBeLessThan(b[1].x);
  expect(b[1].y).toBeLessThan(b[2].y);
  expect(Math.abs(b[1].x - b[2].x)).toBeLessThan(2);
  expect(b[0].h).toBeGreaterThan(b[1].h * 1.5);

  // Alt+arrows move focus spatially; Alt+n jumps.
  expect(b[2].focused).toBe(true);
  await page.keyboard.press('Alt+ArrowUp');
  b = await boxes(page); expect(b[1].focused).toBe(true);
  await page.keyboard.press('Alt+ArrowLeft');
  b = await boxes(page); expect(b[0].focused).toBe(true);
  await page.keyboard.press('Alt+3');
  b = await boxes(page); expect(b[2].focused).toBe(true);
  await page.keyboard.press('Alt+9'); // no such pane: nothing changes
  b = await boxes(page); expect(b[2].focused).toBe(true);

  // Dragging the vertical divider resizes the left pane and is saved on release.
  const before = (await boxes(page))[0].w;
  const divider = page.locator('.divider.row');
  const d = await divider.boundingBox();
  // At the T-junction the vertical divider is the element under the pointer (not covered by the horizontal one).
  expect(await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.className ?? '', { x: d.x + d.width / 2, y: d.y + d.height / 2 })).toContain('row');
  await page.mouse.move(d.x + d.width / 2, d.y + d.height / 2);
  await page.mouse.down();
  await page.mouse.move(d.x + d.width / 2 + 200, d.y + d.height / 2, { steps: 4 });
  await page.mouse.up();
  await expect.poll(async () => (await boxes(page))[0].w, { message: 'left pane grows with the divider' }).toBeGreaterThan(before + 100);
  await divider.dblclick();
  await expect.poll(async () => Math.abs((await boxes(page))[0].w - before) < 3).toBe(true);

  // Reload: the saved arrangement comes back with the same shape and the terminals in their panes.
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(panes(page)).toHaveCount(3);
  await expect(page.locator('.pane-head').nth(0)).toContainText('T1');
  await expect(page.locator('.pane-head').nth(2)).toContainText('T3');

  // Closing a pane gives its space to the sibling and never ends the terminal.
  await page.getByRole('button', { name: 'Close pane 3' }).click();
  await expect(panes(page)).toHaveCount(2);
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(3);
  // The last pane is only emptied.
  await page.getByRole('button', { name: 'Close pane 2' }).click();
  await page.getByRole('button', { name: 'Close pane 1' }).click();
  await expect(panes(page)).toHaveCount(1);
  await expect(page.locator('.pane-head').first()).not.toContainText('T1');
});

test('a split is refused when there is no room, like tmux, and the pane cap holds', async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 700 });
  await openTerminals(page, 1);
  let refused = false;
  for (let i = 0; i < 70 && !refused; i++) {
    const count = await panes(page).count();
    await page.getByRole('button', { name: /^Split pane \d+ to the right$/ }).first().click();
    const now = await panes(page).count();
    if (now === count) refused = true;
  }
  expect(refused).toBe(true);
  await expect(page.locator('.terminal-note').filter({ hasText: /No space for a new pane|No more panes/ })).toBeVisible();
  expect(await panes(page).count()).toBeLessThanOrEqual(64);
  // Every pane that exists is still usable-sized.
  for (const b of await boxes(page)) expect(b.w).toBeGreaterThan(100);
});

test('an arrangement saved by the earlier fixed-layout build is converted, not lost', async ({ page }) => {
  await openTerminals(page, 4);
  await page.evaluate(ids => {
    globalThis.__fakeTerminals.layout = { layout: 'columns', colRatio: 0.3, rowRatio: 0.5, slots: [ids[1], ids[0], null, null], focused: 1 };
  }, [hex(0), hex(1)]);
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(panes(page)).toHaveCount(2);
  await expect(page.locator('.pane-head').nth(0)).toContainText('T2');
  await expect(page.locator('.pane-head').nth(1)).toContainText('T1');
  const b = await boxes(page);
  expect(b[0].w / (b[0].w + b[1].w)).toBeCloseTo(0.3, 1);
});

const launchForm = page => page.getByRole('form', { name: 'Launch an agent in a terminal' });

test('an agent is launched in a terminal, shown in a pane, and its transcript is linked when it appears', async ({ page }) => {
  await openTerminals(page, 1);
  const form = launchForm(page);
  // Nothing is launched until the folder is valid; the form says what is wrong.
  await expect(form.getByRole('button', { name: 'Launch' })).toBeDisabled();
  await form.getByLabel('In folder').fill('relative\\dir');
  await expect(form.getByRole('status')).toContainText('absolute path');
  await expect(form.getByRole('button', { name: 'Launch' })).toBeDisabled();
  await form.getByLabel('In folder').fill("C:\\it's");
  await expect(form.getByRole('button', { name: 'Launch' })).toBeDisabled();
  await form.getByLabel('Agent to launch').selectOption('pi');
  await form.getByLabel('In folder').fill('C:\\Users\\me\\proj');
  await form.getByRole('button', { name: 'Launch' }).click();
  // The new terminal is created, tagged as the agent, and shown in the focused pane.
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(2);
  await expect(page.locator('.terminal-list .badge.agent-tag')).toContainText('Pi');
  // The tag sits beside the tab's title, never over it.
  const tag = await page.locator('.terminal-list .badge.agent-tag').boundingBox();
  const pick = await page.locator('.terminal-list li:has(.badge.agent-tag) .pick').boundingBox();
  expect(tag.x).toBeGreaterThanOrEqual(pick.x + pick.width - 1);
  await expect(page.locator('.panes .pane-head').first()).toContainText('Pi · proj');
  expect(await page.evaluate(() => globalThis.__fakeTerminals.launches)).toEqual([{ agent: 'pi', cwd: 'C:\\Users\\me\\proj' }]);
  // The folder is remembered for next time.
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(launchForm(page).getByLabel('In folder')).toHaveValue('C:\\Users\\me\\proj');
  // The pane has a Terminal | Activity switch; until the transcript exists Activity says it is waiting.
  const pane = page.locator('.panes .pane').filter({ hasText: 'Pi · proj' });
  await expect(pane.locator('.agent-tag')).toContainText('waiting for transcript');
  await pane.getByRole('button', { name: 'Activity', exact: true }).click();
  await expect(pane.locator('.activity')).toContainText("Waiting for the agent's transcript");
  // Once the transcript exists the link resolves by itself and Activity shows the agent's work.
  const terminal = await page.evaluate(() => globalThis.__fakeTerminals.agents[0].terminal);
  await page.evaluate(id => { globalThis.__fakeTerminals.linked = [id]; }, terminal);
  await expect(pane.locator('.agent-tag')).toContainText('transcript linked', { timeout: 15000 });
  await expect(pane.locator('.activity')).toContainText('Working');
  await expect(pane.locator('.activity')).toContainText('Bash');
  await expect(page.locator('.terminal-list .badge.agent-tag')).toContainText('linked');
  // Back to the terminal view; an ordinary terminal has no switch at all.
  await pane.getByRole('button', { name: 'Terminal', exact: true }).click();
  await expect(pane.locator('.xterm')).toHaveCount(1);
  // The Model tab and the window title now carry the model from the transcript; the title names the task.
  await pane.locator('.xterm-helper-textarea').focus();
  await expect(pane.locator('.picker > button')).toContainText('sonnet-5-5');
  await expect(page.locator('.title-context')).toContainText('[Pi/sonnet-5-5]');
  await expect(page.locator('.title-context')).toContainText('* Test User');
  await expect(page.locator('.title-context')).toContainText('Fix the files panel');
  // Ending the terminal forgets the agent entry.
  await page.locator('.terminal-list li').filter({ hasText: 'Pi · proj' }).getByRole('button', { name: 'End session' }).click();
  await page.locator('.terminal-list li').filter({ hasText: 'Pi · proj' }).getByRole('button', { name: 'Confirm end' }).click();
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.agents.length), { timeout: 15000 }).toBe(0);
});

test('the Model tab fills the input with /model <name> without sending it, and keeps favourites', async ({ page }) => {
  await openTerminals(page, 1);
  const form = launchForm(page);
  await form.getByLabel('In folder').fill('C:\\Users\\me\\proj');
  await form.getByRole('button', { name: 'Launch' }).click();
  const pane = page.locator('.panes .pane').filter({ hasText: 'Claude Code' });
  await pane.getByRole('button', { name: 'Model', exact: true }).click();
  const pop = pane.getByRole('dialog', { name: 'Choose a model' });
  await pop.getByLabel('Search models').fill('son');
  await expect(pop.getByRole('option')).toHaveCount(4); // sonnet, opusplan (its note), claude-sonnet-5-5 and the typed "son"
  await pop.getByRole('button', { name: 'Star sonnet', exact: true }).click();
  // Starred models come first, and the star survives a reload.
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  const again = page.locator('.panes .pane').filter({ hasText: 'Claude Code' });
  await again.getByRole('button', { name: 'Model', exact: true }).click();
  await expect(again.getByRole('option').first()).toContainText('sonnet');
  // Picking types the command with no Enter (no newline in the text), then closes the list.
  await page.evaluate(() => { globalThis.__fakeTerminals.inputs = []; });
  await again.getByRole('option').first().click();
  expect(await page.evaluate(() => globalThis.__fakeTerminals.inputs.map(i => i.text))).toEqual(['/model sonnet']);
  await expect(again.getByRole('dialog', { name: 'Choose a model' })).toHaveCount(0);
  // A typed name that is not in the list can be used; an invalid one cannot.
  await again.getByRole('button', { name: 'Model', exact: true }).click();
  await again.getByLabel('Search models').fill('my-model_1');
  await expect(again.getByRole('option')).toHaveCount(1);
  await again.getByLabel('Search models').fill('bad name; rm');
  await expect(again.getByRole('option')).toHaveCount(0);
});

test('an agent that is waiting for a person flashes its edge only while its terminal is not the one in use', async ({ page }) => {
  await openTerminals(page, 1);
  const form = launchForm(page);
  await form.getByLabel('In folder').fill('C:\\Users\\me\\proj');
  await form.getByRole('button', { name: 'Launch' }).click();
  const terminal = await page.evaluate(() => globalThis.__fakeTerminals.agents[0].terminal);
  await page.evaluate(id => { globalThis.__fakeTerminals.linked = [id]; globalThis.__fakeTerminals.snapshotState = 'waiting_for_human'; }, terminal);
  // In use (focused): no flash, even though the agent is waiting.
  await expect(page.locator('.agent-body .agent-tag')).toContainText('transcript linked', { timeout: 15000 });
  await page.waitForTimeout(500);
  await expect(page.locator('.agent-body.flash')).toHaveCount(0);
  // Focus moves elsewhere: the edge flashes.
  await page.getByRole('button', { name: 'Split pane 1 to the right' }).click();
  await expect(page.locator('.agent-body.flash')).toHaveCount(1, { timeout: 15000 });
  // The flash never catches the mouse and stops once the agent is no longer waiting.
  expect(await page.locator('.agent-body.flash').evaluate(el => getComputedStyle(el, '::after').pointerEvents)).toBe('none');
  await page.evaluate(() => { globalThis.__fakeTerminals.snapshotState = 'working'; });
  await expect(page.locator('.agent-body.flash')).toHaveCount(0, { timeout: 15000 });
});

test('the Model tab follows the CLI now running in the terminal: the list for another agent can be chosen and is remembered', async ({ page }) => {
  await openTerminals(page, 1);
  const form = launchForm(page);
  await form.getByLabel('In folder').fill('C:\\Users\\me\\proj');
  await form.getByRole('button', { name: 'Launch' }).click();
  const pane = () => page.locator('.panes .pane').filter({ hasText: 'Claude Code' });
  await pane().getByRole('button', { name: 'Model', exact: true }).click();
  // It starts with the agent that was launched (Claude: aliases and ids).
  await expect(pane().getByRole('button', { name: 'Claude Code', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(pane().getByRole('option').first()).toContainText('default');
  // The person quit Claude and started Codex in the same terminal: choosing Codex gives Codex's own list.
  await pane().getByRole('group', { name: 'Models for' }).getByRole('button', { name: 'Codex', exact: true }).click();
  await expect(pane().getByRole('option')).toHaveCount(1);
  await page.evaluate(() => { globalThis.__fakeTerminals.inputs = []; });
  await pane().getByRole('option').first().click();
  expect(await page.evaluate(() => globalThis.__fakeTerminals.inputs.map(i => i.text))).toEqual(['/model']);
  // Remembered for this terminal across a reload, and a different terminal is unaffected.
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  await pane().getByRole('button', { name: 'Model', exact: true }).click();
  await expect(pane().getByRole('button', { name: 'Codex', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(pane().getByRole('option')).toHaveCount(1);
  expect(await page.evaluate(() => Object.keys(JSON.parse(localStorage.getItem('aw.model-agent') ?? '{}')).length)).toBe(1);
});

const wins = page => page.locator('.float-layer .float-win');
const rail = (page, name) => page.locator('.rail button', { hasText: name });
const tiles = page => page.locator('.ins-tiles .tile');
/** Select a session's tile, then click one of the actions in the details beneath it. */
async function act(page, index, name, exact = false) {
  await tiles(page).nth(index).click();
  await page.locator('.ins-detail .sel-actions').getByRole('button', { name, exact }).click();
}
// Windows are found by their terminal label: dragging one raises it, which reorders them in the DOM.
const winOf = (page, n) => page.locator('.float-layer .float-win').filter({ hasText: `T${n + 1}` });
const gripBox = async (page, n) => (await winOf(page, n).locator('.float-grip').boundingBox());
async function dragGrip(page, n, toX, toY) {
  const g = await gripBox(page, n);
  await page.mouse.move(g.x + 20, g.y + g.height / 2);
  await page.mouse.down();
  await page.mouse.move(toX + 20, toY + g.height / 2, { steps: 5 });
  await page.mouse.up();
}

test('terminals float on the Canvas, take any position (a star of five) and are remembered', async ({ page }) => {
  await openTerminals(page, 5);
  for (let i = 0; i < 5; i++) await page.locator('.terminal-list li').nth(i).locator('button.float-out').click();
  // Floating takes them out of the grid and the tab shows it.
  await expect(page.locator('.terminal-list li button.float-out').first()).toHaveText('Floating');
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(5);
  const area = await page.locator('.float-layer').boundingBox();
  // A star: one in the centre and four around it, free of any grid.
  const cx = area.x + area.width / 2, cy = area.y + area.height / 2;
  const spots = [[cx - 280, cy - 170], [area.x + 30, area.y + 20], [area.x + area.width - 600, area.y + 20], [area.x + 30, area.y + area.height - 380], [area.x + area.width - 600, area.y + area.height - 380]];
  // Order matters only because windows overlap: grab each title bar before another window lands on top of it.
  for (const i of [0, 2, 3, 4, 1]) await dragGrip(page, i, spots[i][0], spots[i][1]);
  const rectOf = async n => await winOf(page, n).boundingBox();
  const placed = [];
  for (let i = 0; i < 5; i++) placed.push(await rectOf(i));
  placed.forEach((b, i) => { expect(Math.abs(b.x - spots[i][0])).toBeLessThan(3); expect(Math.abs(b.y - spots[i][1])).toBeLessThan(3); });
  // Free-form means windows may overlap; every window has its own stacking level.
  const z = await wins(page).evaluateAll(els => els.map(e => Number(e.style.zIndex)));
  expect(new Set(z).size).toBe(5);
  // Every window holds a live terminal and the arrangement is saved natively.
  await expect(page.locator('.float-win .xterm')).toHaveCount(5);
  const saved = await page.evaluate(() => globalThis.__fakeTerminals.windows);
  expect(saved.version).toBe(2);
  expect(saved.windows).toHaveLength(5);
  // Reload: same windows, same places.
  await page.reload();
  await expect(wins(page)).toHaveCount(5);
  const again = [];
  for (let i = 0; i < 5; i++) again.push(await rectOf(i));
  again.forEach((b, i) => { expect(Math.abs(b.x - placed[i].x)).toBeLessThan(3); expect(Math.abs(b.y - placed[i].y)).toBeLessThan(3); });
  // Resize from the corner, and keep the title bar reachable when dragged far off the canvas.
  await winOf(page, 0).locator('.float-grip').focus(); // keyboard focus raises it above the overlapping windows
  const w0 = await rectOf(0);
  const handle = await winOf(page, 0).locator('.float-resize').boundingBox();
  await page.mouse.move(handle.x + 8, handle.y + 8);
  await page.mouse.down();
  await page.mouse.move(handle.x + 128, handle.y + 88, { steps: 4 });
  await page.mouse.up();
  await expect.poll(async () => (await rectOf(0)).width).toBeGreaterThan(w0.width + 100);
  const g = await gripBox(page, 0);
  await page.mouse.move(g.x + 20, g.y + 10);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width + 900, area.y + area.height + 900, { steps: 4 });
  await page.mouse.up();
  const far = await rectOf(0);
  expect(far.x).toBeLessThan(area.x + area.width);
  expect(far.y).toBeLessThan(area.y + area.height);
  // Closing a window hides it but never ends the terminal; the Terminals tab can show it in the grid again.
  await winOf(page, 0).getByRole('button', { name: /^Close terminal window/ }).click();
  await expect(wins(page)).toHaveCount(4);
  await rail(page, 'Terminals').click();
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(5);
  await page.locator('.terminal-list li').nth(0).locator('button.pick').click();
  await expect(page.locator('.panes .pane-body .xterm')).toHaveCount(1);
  // Showing a floating terminal in the grid takes it off the canvas (one place at a time).
  await page.locator('.terminal-list li').nth(1).locator('button.pick').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(3);
});

test('a floating agent terminal minimizes to a bar, then a block, keeps flashing while it waits, and restores', async ({ page }) => {
  await openTerminals(page, 1);
  const form = launchForm(page);
  await form.getByLabel('In folder').fill('C:\\Users\\me\\proj');
  await form.getByRole('button', { name: 'Launch' }).click();
  const terminal = await page.evaluate(() => globalThis.__fakeTerminals.agents[0].terminal);
  await expect(page.locator('.terminal-list li button.float-out')).toHaveCount(2);
  for (let i = 0; i < 2; i++) await page.locator('.terminal-list li').nth(i).locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(2);
  const label = await wins(page).filter({ has: page.locator('.agent-body') }).getAttribute('aria-label');
  const name = label.replace(/^Terminal window: /, '');
  const win = wins(page).filter({ has: page.getByRole('button', { name: `Move terminal ${name};` }) });
  // Full → bar: only the title row, no terminal drawn (it keeps running).
  await win.getByRole('button', { name: `Minimize ${name} to a bar` }).click();
  await expect(win).toHaveClass(/\bbar\b/);
  expect((await win.boundingBox()).height).toBeLessThan(32);
  await expect(win.locator('.xterm')).toHaveCount(0);
  // The agent waits for a person: the minimized window's own edge flashes.
  await page.evaluate(id => { globalThis.__fakeTerminals.linked = [id]; globalThis.__fakeTerminals.snapshotState = 'waiting_for_human'; }, terminal);
  await expect(win).toHaveClass(/\bflash\b/, { timeout: 15000 });
  // Bar → block: small, still flashing, remembered across a reload.
  await win.getByRole('button', { name: `Minimize ${name} to a block` }).click();
  await expect(win).toHaveClass(/\bblock\b/);
  const box = await win.boundingBox();
  expect(box.width).toBeLessThan(80);
  expect(box.height).toBeLessThan(56);
  await expect(win).toHaveClass(/\bflash\b/);
  expect(await page.evaluate(id => globalThis.__fakeTerminals.windows.windows.find(w => w.terminal === id).stage, terminal)).toBe('block');
  await page.reload();
  await expect(win).toHaveClass(/\bblock\b/);
  // Block → full: the terminal is back at its size; the other window was never touched.
  await win.getByRole('button', { name: `Restore ${name}` }).click();
  await expect(win.locator('.agent-body')).toHaveCount(1);
  expect((await win.boundingBox()).height).toBeGreaterThan(120);
  await expect(wins(page).locator('.xterm')).toHaveCount(2);
  expect(await page.evaluate(id => 'stage' in globalThis.__fakeTerminals.windows.windows.find(w => w.terminal === id), terminal)).toBe(false);
});

test('a session panel links to a terminal and flips between its transcript and that terminal', async ({ page, bridge }) => {
  await openTerminals(page, 3);
  await rail(page, 'Canvas').click();
  await page.getByLabel('Session label', { exact: true }).fill('Linked session');
  await page.getByRole('button', { name: 'Create session', exact: true }).click();
  await expect(page.locator('section.agent')).toHaveCount(1);
  const panel = page.locator('section.agent');
  const select = panel.getByLabel(/^Terminal linked to/);
  await expect(select).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Show terminal' })).toHaveCount(0);
  await select.selectOption({ index: 2 }); // the second terminal
  await expect(panel.getByRole('button', { name: 'Show terminal' })).toBeVisible();
  await panel.getByRole('button', { name: 'Show terminal' }).click();
  await expect(panel.locator('.panel-terminal .xterm')).toHaveCount(1);
  await expect(panel.getByText('Selected / historical run')).toHaveCount(0);
  // The terminal is now in this panel only: floating it moves it out of the panel (back to the transcript).
  await rail(page, 'Terminals').click();
  await page.locator('.terminal-list li').nth(1).locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(1);
  await expect(panel.locator('.panel-terminal')).toHaveCount(0);
  await expect(panel.getByRole('button', { name: 'Show terminal' })).toBeVisible();
  // Flipping it back takes it out of the window again (the window overlaps the panel here, so click via the DOM).
  await panel.getByRole('button', { name: 'Show terminal' }).dispatchEvent('click');
  await expect(wins(page)).toHaveCount(0);
  await expect(panel.locator('.panel-terminal .xterm')).toHaveCount(1);
  await panel.getByRole('button', { name: 'Show transcript' }).click();
  await expect(panel.locator('.panel-terminal')).toHaveCount(0);
  // The link survives a reload; unlinking removes the flip button.
  await page.getByRole('button', { name: 'Save layout', exact: true }).click(); // the panel itself is part of the saved layout
  await expect(page.locator('.layout-actions > .badge')).toHaveAttribute('title', 'SAVED · REV 1');
  await page.reload();
  await expect(page.locator('section.agent').getByRole('button', { name: 'Show terminal' })).toBeVisible();
  await page.locator('section.agent').getByLabel(/^Terminal linked to/).selectOption('');
  await expect(page.locator('section.agent').getByRole('button', { name: 'Show terminal' })).toHaveCount(0);
  expect(bridge.counts.start_fake_run).toBe(0);
});

test('an existing agent session can be watched in a floating window (Activity only), moved, and remembered', async ({ page }) => {
  await openTerminals(page, 1);
  await rail(page, 'Inspector').click();
  await expect(tiles(page)).toHaveCount(4);
  await tiles(page).first().click();
  await expect(page.locator('.ins-detail .sel-actions').getByRole('button', { name: 'Watch', exact: true })).toBeVisible();
  await act(page, 0, 'Watch', true);
  await expect(page.locator('.ins-detail .sel-actions').getByRole('button', { name: 'Watching' })).toBeVisible();
  await act(page, 1, 'Watch', true);
  // On the Canvas both appear as dashed "watching" windows showing the agent's Activity; no terminal is involved.
  await rail(page, 'Canvas').click();
  const watches = page.locator('.float-layer .float-win.watch');
  await expect(watches).toHaveCount(2);
  await expect(watches.first()).toContainText('Claude Code');
  await expect(watches.first().locator('.activity')).toContainText('Working', { timeout: 15000 });
  await expect(watches.first().locator('.activity')).toContainText('Bash');
  await expect(page.locator('.float-win .xterm')).toHaveCount(0);
  const saved = await page.evaluate(() => globalThis.__fakeTerminals.windows);
  expect(saved.version).toBe(2);
  expect(saved.watches.map(w => w.agent).sort()).toEqual(['claude', 'pi']);
  // Drag one by its title bar; the position is kept across a reload.
  const claude = page.locator('.float-layer .float-win.watch').filter({ hasText: 'Claude Code' });
  const grip = await claude.locator('.float-grip').boundingBox();
  const before = await claude.boundingBox();
  await page.mouse.move(grip.x + 20, grip.y + grip.height / 2);
  await page.mouse.down();
  await page.mouse.move(grip.x + 220, grip.y + 170, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => Math.round((await claude.boundingBox()).x - before.x)).toBe(200);
  const moved = await claude.boundingBox();
  await page.reload();
  await expect(page.locator('.float-layer .float-win.watch')).toHaveCount(2);
  const again = await page.locator('.float-layer .float-win.watch').filter({ hasText: 'Claude Code' }).boundingBox();
  expect(Math.abs(again.x - moved.x)).toBeLessThan(3);
  expect(Math.abs(again.y - moved.y)).toBeLessThan(3);
  // Closing a watch window only closes the window.
  await page.getByRole('button', { name: /^Stop watching/ }).first().click();
  await expect(page.locator('.float-layer .float-win.watch')).toHaveCount(1);
});

const propsButton = page => page.getByRole('button', { name: 'Open properties' });
const propsDialog = page => page.getByRole('dialog', { name: 'Properties' });

test('Properties opens from the title bar, is split into tabs, works by keyboard and closes with Escape', async ({ page }) => {
  await openTerminals(page, 0);
  await propsButton(page).click();
  const dialog = propsDialog(page);
  await expect(dialog).toBeVisible();
  const tabs = dialog.getByRole('tab');
  await expect(tabs).toHaveText(['Terminals', 'Agents', 'Appearance', 'Data']);
  await expect(dialog.getByRole('tab', { name: 'Terminals' })).toHaveAttribute('aria-selected', 'true');
  await expect(dialog.getByLabel('Start new terminals in')).toBeVisible();
  await expect(dialog.getByLabel('Start the terminal service when I sign in')).toBeVisible();
  // Arrow keys move between tabs; each tab shows its own options.
  await dialog.getByRole('tab', { name: 'Terminals' }).focus();
  await page.keyboard.press('ArrowDown');
  await expect(dialog.getByRole('tab', { name: 'Agents' })).toHaveAttribute('aria-selected', 'true');
  await expect(dialog.getByLabel('Default folder for Launch agent')).toBeVisible();
  await page.keyboard.press('ArrowDown');
  await expect(dialog.getByRole('combobox', { name: 'Theme', exact: true })).toBeVisible();
  await expect(dialog.getByText('Native background')).toBeVisible();
  await page.keyboard.press('Home');
  await expect(dialog.getByRole('tab', { name: 'Terminals' })).toHaveAttribute('aria-selected', 'true');
  // Tab stays inside the dialog, Escape closes it and focus goes back to the button that opened it.
  for (let i = 0; i < 12; i++) await page.keyboard.press('Tab');
  expect(await page.evaluate(() => !!document.activeElement?.closest('[role=dialog]'))).toBe(true);
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(propsButton(page)).toBeFocused();
  // The theme and background controls are no longer in the sidebar.
  await expect(page.getByRole('combobox', { name: 'Theme', exact: true })).toHaveCount(0);
});

test('the terminal start folder and the other terminal options are checked, saved and remembered', async ({ page }) => {
  await openTerminals(page, 0);
  await propsButton(page).click();
  const dialog = propsDialog(page);
  const start = dialog.getByLabel('Start new terminals in');
  const saved = () => page.evaluate(() => globalThis.__fakeTerminals.settings);
  // A bad folder says why and is never saved.
  await start.fill('relative\\dir');
  await start.press('Enter');
  await expect(dialog.getByRole('status').filter({ hasText: 'absolute path' })).toBeVisible();
  expect((await saved()).terminals.start_folder).toBe('');
  // A good one is saved when you press Enter (or leave the field).
  await start.fill('D:\\work\\my project');
  await start.press('Enter');
  await expect.poll(async () => (await saved()).terminals.start_folder).toBe('D:\\work\\my project');
  // A folder the native side refuses (it does not exist) is reported and the old value stays.
  await start.fill('C:\\does\\not\\exist');
  await start.press('Enter');
  await expect(dialog.getByRole('status').filter({ hasText: 'does not exist' }).first()).toBeVisible();
  expect((await saved()).terminals.start_folder).toBe('D:\\work\\my project');
  await start.fill('D:\\work\\my project');
  // The extended-history default is saved at once.
  await dialog.getByLabel('Keep extended history by default (32 MiB per terminal)').check();
  await expect.poll(async () => (await saved()).terminals.extended_history).toBe(true);
  // It, and the start folder, survive a reload; the "use my user folder" button clears the folder.
  await page.reload();
  await propsButton(page).click();
  await expect(propsDialog(page).getByLabel('Start new terminals in')).toHaveValue('D:\\work\\my project');
  await expect(propsDialog(page).getByLabel('Keep extended history by default (32 MiB per terminal)')).toBeChecked();
  await propsDialog(page).getByRole('button', { name: 'Use my user folder' }).click();
  await expect.poll(async () => (await saved()).terminals.start_folder).toBe('');
  await page.keyboard.press('Escape');
  // The Terminals tab's "Extended history" box starts ticked because of the default.
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(page.getByLabel('Extended history (32 MiB)')).toBeChecked();
});

test('the agent launch folder lives in Properties, prefills the Launch row and follows the last folder used', async ({ page }) => {
  await openTerminals(page, 0);
  await propsButton(page).click();
  const folder = propsDialog(page).getByLabel('Default folder for Launch agent');
  await propsDialog(page).getByRole('tab', { name: 'Agents' }).click();
  await folder.fill("C:\\it's");
  await folder.press('Enter');
  await expect(propsDialog(page).getByRole('status').filter({ hasText: 'not allowed' }).first()).toBeVisible();
  await folder.fill('C:\\Users\\me\\proj');
  await folder.press('Enter');
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.settings.agents.launch_folder)).toBe('C:\\Users\\me\\proj');
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: /Terminals/ }).click();
  const row = launchForm(page);
  await expect(row.getByLabel('In folder')).toHaveValue('C:\\Users\\me\\proj');
  // Launching in another folder makes that the new default.
  await row.getByLabel('In folder').fill('C:\\Users\\me\\other');
  await row.getByRole('button', { name: 'Launch' }).click();
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.settings.agents.launch_folder)).toBe('C:\\Users\\me\\other');
});

test('with no session panels the canvas has no big notice, just a small line under the counts', async ({ page, bridge }) => {
  await page.goto('/');
  const footer = page.locator('.canvas-footer');
  await expect(footer).toContainText('0/16 PANELS');
  await expect(footer.locator('.canvas-hint')).toContainText('No session panels open');
  await expect(page.locator('.canvas-empty')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'No session panels open' })).toHaveCount(0);
  // Bottom left, small, and below the counts.
  const hint = await footer.locator('.canvas-hint').boundingBox();
  const canvas = await page.locator('.canvas').boundingBox();
  expect(hint.x - canvas.x).toBeLessThan(40);
  expect(canvas.y + canvas.height - (hint.y + hint.height)).toBeLessThan(30);
  const size = await footer.locator('.canvas-hint').evaluate(e => parseFloat(getComputedStyle(e).fontSize));
  expect(size).toBeLessThanOrEqual(10);
});

test('the folder buttons open the folder picker and fill the box; cancelling changes nothing', async ({ page }) => {
  await openTerminals(page, 0);
  await propsButton(page).click();
  const dialog = propsDialog(page);
  const saved = () => page.evaluate(() => globalThis.__fakeTerminals.settings);
  const browse = dialog.getByRole('button', { name: 'Browse for a folder' });
  await expect(browse).toBeVisible();
  // The button sits inside the right edge of the text box.
  const box = await dialog.getByLabel('Start new terminals in').boundingBox();
  const icon = await browse.boundingBox();
  const frame = await dialog.locator('.field-box').boundingBox();
  // The button is part of the box's own layout: it ends at the box's right edge and never pokes out above or below it,
  // also while hovered; the text field takes the rest and does not run under it.
  const inside = (b, f) => b.x >= f.x - 0.5 && b.y >= f.y - 0.5 && b.x + b.width <= f.x + f.width + 0.5 && b.y + b.height <= f.y + f.height + 0.5;
  expect(inside(icon, frame)).toBe(true);
  expect(frame.x + frame.width - (icon.x + icon.width)).toBeLessThan(3);
  expect(box.x + box.width).toBeLessThanOrEqual(icon.x + 0.5);
  await browse.hover();
  expect(inside(await browse.boundingBox(), frame)).toBe(true);
  expect(Math.abs(icon.width - icon.height)).toBeLessThan(2);
  // Cancelled: nothing changes.
  await browse.click();
  expect((await saved()).terminals.start_folder).toBe('');
  // A chosen folder is checked and saved like a typed one, and the picker opened at what was in the box.
  await page.evaluate(() => { globalThis.__fakeTerminals.picked = 'D:\\picked\\place'; });
  await browse.click();
  await expect(dialog.getByLabel('Start new terminals in')).toHaveValue('D:\\picked\\place');
  await expect.poll(async () => (await saved()).terminals.start_folder).toBe('D:\\picked\\place');
  await browse.click();
  expect(await page.evaluate(() => globalThis.__fakeTerminals.pickedStart)).toBe('D:\\picked\\place');
  // A folder that is not acceptable (a network path) is refused with the reason and not saved.
  await page.evaluate(() => { globalThis.__fakeTerminals.picked = '\\\\server\\share'; });
  await browse.click();
  await expect(dialog.getByRole('status').filter({ hasText: 'absolute path' })).toBeVisible();
  expect((await saved()).terminals.start_folder).toBe('D:\\picked\\place');
  await page.keyboard.press('Escape');
  // The Launch agent row has one too, and it only fills the box.
  await page.getByRole('button', { name: /Terminals/ }).click();
  await page.evaluate(() => { globalThis.__fakeTerminals.picked = 'C:\\Users\\me\\agentwork'; });
  const row = launchForm(page);
  await row.getByRole('button', { name: 'Browse for the agent folder' }).click();
  await expect(row.getByLabel('In folder')).toHaveValue('C:\\Users\\me\\agentwork');
});

const tinyPng = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');
const asset = { status: 'selected', kind: 'image', asset_id: '0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d', asset_url: 'http://background.localhost/0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d', display_name: 'wall.png', mime_type: 'image/png', width: 1, height: 1 };

test('choosing a background image shows it and says so; a refused file says why in the Appearance tab', async ({ page, bridge }) => {
  await page.route('http://background.localhost/**', route => route.fulfill({ contentType: 'image/png', body: tinyPng }));
  await page.goto('/');
  await propsButton(page).click();
  const dialog = propsDialog(page);
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  // A file the native side refuses: the reason appears right here, and the solid background stays.
  await page.evaluate(() => { globalThis.__fakeTerminals.background = 'Images are limited to 8 MiB and videos to 64 MiB in this spike.'; });
  await dialog.getByRole('combobox', { name: 'Mode' }).selectOption('image');
  await expect(dialog.getByRole('status').filter({ hasText: 'limited to 8 MiB' })).toBeVisible();
  await expect(page.locator('img.media')).toHaveCount(0);
  // A good file: the image element loads, and the note names it and the opacity in use.
  await page.evaluate(asset => { globalThis.__fakeTerminals.background = asset; }, asset);
  await dialog.getByRole('button', { name: /Choose local image/ }).click();
  await expect(dialog.getByRole('status').filter({ hasText: 'wall.png' })).toContainText('% opacity');
  await expect(page.locator('img.media')).toHaveCount(1);
  expect(await page.locator('img.media').evaluate(img => img.complete && img.naturalWidth > 0)).toBe(true);
  await page.getByRole('button', { name: 'Close properties' }).click();
  // The background is behind every tab, not only the Canvas: the views let it through.
  const clear = el => getComputedStyle(el).backgroundColor === 'rgba(0, 0, 0, 0)';
  for (const [name, view] of [['Inspector', '.inspector'], ['Terminals', '.terminals'], ['Tasks', '.run-board'], ['Models', '.usage-overview'], ['Events', '.event-timeline'], ['Logs', '.workspace-diagnostics']]) {
    await rail(page, name).click();
    await expect(page.locator(view)).toBeVisible();
    expect(await page.locator(view).evaluate(clear), `${name} lets the background through`).toBe(true);
    const media = await page.locator('img.media').boundingBox();
    const area = await page.locator('.shell').boundingBox();
    expect(media && Math.abs(media.width - area.width) < 2 && Math.abs(media.height - area.height) < 2, `${name} shows the image across the workspace`).toBeTruthy();
    await expect(page.locator('img.media')).toBeVisible();
  }
  // Back to a solid background: the views are opaque again.
  await propsButton(page).click();
  await propsDialog(page).getByRole('tab', { name: 'Appearance' }).click();
  await propsDialog(page).getByRole('combobox', { name: 'Mode' }).selectOption('solid');
  await page.getByRole('button', { name: 'Close properties' }).click();
  await expect(page.locator('img.media')).toHaveCount(0);
  expect(await page.locator('.workspace-diagnostics').evaluate(clear)).toBe(false);
});

const resumes = page => page.evaluate(() => globalThis.__fakeTerminals.resumes);

test('Inspector: a session opens in a terminal from its button or its right-click menu, and lands on the Canvas', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  const rows = tiles(page);
  // The button in the details of the first session.
  await act(page, 0, 'Open in terminal');
  await expect(rail(page, 'Canvas')).toHaveAttribute('aria-current', 'page');
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toHaveCount(1);
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toContainText('Claude Code');
  expect(await resumes(page)).toEqual([{ source: '0123456789abcdef', force: false }]);
  // The right-click menu on the second row offers the same, plus Watch.
  await rail(page, 'Inspector').click();
  await rows.nth(1).click({ button: 'right' });
  const menu = page.getByRole('menu', { name: /^Actions for/ });
  await expect(menu.getByRole('menuitem')).toHaveText(['Open in terminal', 'Watch on the Canvas', 'Tag red', 'Tag orange', 'Tag yellow', 'Tag green', 'Tag blue', 'Tag purple']);
  await expect(menu.getByRole('menuitem').first()).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(menu).toHaveCount(0);
  await rows.nth(1).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Open in terminal' }).click();
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toHaveCount(2);
  expect((await resumes(page)).map(r => r.source)).toEqual(['0123456789abcdef', 'fedcba9876543210']);
  // Menu opened near a corner stays on screen.
  await rail(page, 'Inspector').click();
  await rows.first().click({ button: 'right', position: { x: 20, y: 10 } });
  const box = await menu.boundingBox();
  const view = page.viewportSize();
  expect(box.x + box.width).toBeLessThanOrEqual(view.width);
  expect(box.y + box.height).toBeLessThanOrEqual(view.height);
});

test('a watch window has an Open in terminal button and a right-click menu, and turns into the terminal window', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Watch', true);
  await act(page, 1, 'Watch', true);
  await rail(page, 'Canvas').click();
  const watches = page.locator('.float-layer .float-win.watch');
  await expect(watches).toHaveCount(2);
  // The button: this window becomes a terminal window in the same place and size.
  const claude = watches.filter({ hasText: 'Claude Code' });
  const was = await claude.boundingBox();
  await claude.getByRole('button', { name: /in a terminal$/ }).click();
  await expect(watches).toHaveCount(1);
  const terminal = page.locator('.float-layer .float-win:not(.watch)');
  await expect(terminal).toHaveCount(1);
  const now = await terminal.boundingBox();
  expect(Math.abs(now.x - was.x) + Math.abs(now.y - was.y) + Math.abs(now.width - was.width) + Math.abs(now.height - was.height)).toBeLessThan(3);
  // The right-click menu on the other watch window does the same.
  await watches.first().click({ button: 'right', position: { x: 40, y: 12 } });
  await expect(page.getByRole('menuitem')).toHaveText(['Open in terminal', 'Stop watching']);
  await page.getByRole('menuitem', { name: 'Open in terminal' }).click();
  await expect(watches).toHaveCount(0);
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toHaveCount(2);
  expect((await resumes(page)).map(r => r.force)).toEqual([false, false]);
  // Right-click then Stop watching only closes a watch window.
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Watch', true);
  await rail(page, 'Canvas').click();
  await watches.first().click({ button: 'right', position: { x: 40, y: 12 } });
  await page.getByRole('menuitem', { name: 'Stop watching' }).click();
  await expect(watches).toHaveCount(0);
});

test('a session that may still be running asks first; Cancel changes nothing, Open anyway resumes it', async ({ page }) => {
  await openTerminals(page, 0);
  await page.evaluate(() => { globalThis.__fakeTerminals.resumeMode = 'busy'; });
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Open in terminal');
  const dialog = page.getByRole('alertdialog', { name: 'This session may still be running' });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText('12 seconds ago');
  await expect(dialog.getByRole('button', { name: 'Cancel' })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  expect(await resumes(page)).toEqual([{ source: '0123456789abcdef', force: false }]);
  await expect(page.locator('.float-layer .float-win')).toHaveCount(0);
  await act(page, 0, 'Open in terminal');
  await dialog.getByRole('button', { name: 'Open anyway' }).click();
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toHaveCount(1);
  expect((await resumes(page)).map(r => r.force)).toEqual([false, false, true]);
});

test('when a session cannot be opened in a terminal the reason is shown', async ({ page }) => {
  await openTerminals(page, 0);
  await page.evaluate(() => { globalThis.__fakeTerminals.resumeMode = 'error'; });
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Open in terminal');
  const dialog = page.getByRole('alertdialog', { name: 'Could not open in a terminal' });
  await expect(dialog).toContainText('no longer exists');
  await dialog.getByRole('button', { name: 'OK' }).click();
  await expect(dialog).toHaveCount(0);
  await expect(rail(page, 'Inspector')).toHaveAttribute('aria-current', 'page');
});

test('a session open in another window says so, instead of guessing from file age', async ({ page }) => {
  await openTerminals(page, 0);
  await page.evaluate(() => { globalThis.__fakeTerminals.resumeMode = 'running'; });
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Open in terminal');
  const dialog = page.getByRole('alertdialog', { name: 'This session is running in another window' });
  await expect(dialog).toContainText('open in another terminal window right now');
  await expect(dialog).not.toContainText('seconds ago');
  await dialog.getByRole('button', { name: 'Open anyway' }).click();
  await expect(page.locator('.float-layer .float-win:not(.watch)')).toHaveCount(1);
});

test('Inspector: sessions are small tiles grouped by state in tabs, with the last line each one wrote', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  const tabs = page.getByRole('tablist', { name: 'Session state' }).getByRole('tab');
  await expect(tabs).toHaveText(['All4', 'Active1', 'Idle1', 'Inactive1', 'Other1']);
  await expect(tiles(page)).toHaveCount(4);
  // Each tile shows the folder, the agent, the model and the last line.
  const first = tiles(page).first();
  await expect(first).toContainText('proj');
  await expect(first).toContainText('Claude Code');
  await expect(first).toContainText('opus-5');
  await expect(first).toContainText('Done: all tests pass');
  await expect(first).toContainText('background');
  // Tabs filter; Other is last and holds the nearly empty transcript.
  await page.getByRole('tab', { name: /^Idle/ }).click();
  await expect(tiles(page)).toHaveCount(1);
  await expect(tiles(page).first()).toContainText('Waiting for your review');
  await page.getByRole('tab', { name: /^Other/ }).click();
  await expect(tiles(page)).toHaveCount(1);
  await expect(tiles(page).first()).toContainText('no output yet');
  await page.getByRole('tab', { name: /^Inactive/ }).click();
  await expect(tiles(page).first()).toContainText('Patch applied');
  // Arrow keys move between tabs.
  await page.getByRole('tab', { name: /^Inactive/ }).focus();
  await page.keyboard.press('ArrowLeft');
  await expect(page.getByRole('tab', { name: /^Idle/ })).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Home');
  await expect(page.getByRole('tab', { name: /^All/ })).toHaveAttribute('aria-selected', 'true');
  // The agent filter narrows the counts.
  await page.getByLabel('Filter by agent').selectOption('claude');
  await expect(tabs).toHaveText(['All2', 'Active1', 'Idle0', 'Inactive0', 'Other1']);
  await page.getByLabel('Filter by agent').selectOption('all');
  await tiles(page).first().click();
  // Compact: a tile is small, and the grid wraps instead of scrolling sideways.
  const box = await first.boundingBox();
  expect(box.height).toBeLessThan(120);
  expect(await page.locator('.ins-tiles').evaluate(el => el.scrollWidth <= el.clientWidth + 1)).toBe(true);
});

test('Inspector: sorting groups the tiles into labelled sections by agent, model or folder', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  await page.getByLabel('Sort sessions by').selectOption('agent');
  await expect(page.locator('.ins-tiles h2.sec')).toHaveText(['Claude Code 2', 'Pi 1', 'Codex 1']);
  await page.getByLabel('Sort sessions by').selectOption('model');
  await expect(page.locator('.ins-tiles h2.sec').first()).toContainText('claude-opus-5');
  await expect(page.locator('.ins-tiles h2.sec').last()).toContainText('Model not named');
  await page.getByLabel('Sort sessions by').selectOption('folder');
  await expect(page.locator('.ins-tiles h2.sec')).toHaveCount(4);
  await page.getByLabel('Sort sessions by').selectOption('newest');
  await expect(page.locator('.ins-tiles h2.sec')).toHaveCount(0);
});

test('Inspector: selecting a tile shows its last output and details; colour tags are saved and shown', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  await expect(page.locator('.ins-detail')).toContainText('Select a session above');
  await tiles(page).first().click();
  const sel = page.locator('.ins-detail .sel');
  await expect(sel).toContainText('LAST OUTPUT');
  await expect(sel).toContainText('Done: all tests pass');
  await expect(sel).toContainText('running in the background');
  await expect(page.locator('.ins-detail')).toContainText('Where the time goes', { timeout: 15000 });
  // Tag it from the swatches; the tile and the saved file follow.
  await sel.getByRole('button', { name: 'Tag green' }).click();
  await expect.poll(() => page.evaluate(() => Object.values(globalThis.__fakeTerminals.tags.tags))).toEqual(['green']);
  await expect(tiles(page).first()).toContainText('green');
  await expect(sel.getByRole('button', { name: 'Tag green' })).toHaveAttribute('aria-pressed', 'true');
  // From the right-click menu too, and a tag can be removed.
  await tiles(page).nth(1).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Tag purple' }).click();
  await expect.poll(() => page.evaluate(() => Object.values(globalThis.__fakeTerminals.tags.tags).sort())).toEqual(['green', 'purple']);
  await tiles(page).nth(1).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Remove tag' }).click();
  await expect.poll(() => page.evaluate(() => Object.values(globalThis.__fakeTerminals.tags.tags))).toEqual(['green']);
  // Tags survive a reload.
  await page.reload();
  await rail(page, 'Inspector').click();
  await expect(tiles(page).first()).toContainText('green');
});

const SUBAGENT = { id: 'aaaaaaaaaaaaaaaa', agent: 'claude', label: 'C--Users-me-proj', modified_ms: Date.now() - 3000, bytes: 4096, active: true, running: null, model: null, last_line: 'Reading the files', folder: 'proj', kind: 'subagent', parent: '0123456789abcdef' };
const mapNodes = page => page.locator('.ins-map g.node.agent');
const toolNodes = page => page.locator('.ins-map g.node[data-node*="/t:"], .ins-map g.node[data-node*="/g:"]');

test('Inspector: the Map shows every session and subagent as a glyph, keeps its selection across rail switches, and is keyboard reachable', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 900 });
  await openTerminals(page, 0);
  await page.evaluate(sub => { globalThis.__fakeTerminals.extraSources = [sub]; }, SUBAGENT);
  await rail(page, 'Inspector').click();
  await expect(page.getByRole('tablist', { name: 'Session state' }).getByRole('tab').first()).toHaveText('All5');
  // Tiles is the default; the subagent tile is named after its parent and cannot be resumed on its own.
  await expect(tiles(page).filter({ hasText: 'sub aaaa' })).toHaveCount(1);
  await tiles(page).filter({ hasText: 'sub aaaa' }).click();
  await expect(page.locator('.sel-actions').getByRole('button', { name: 'Open in terminal' })).toBeDisabled();
  await expect(page.locator('.sel-actions').getByRole('button', { name: 'Open in terminal' })).toHaveAttribute('title', /subagent cannot be resumed/);
  // Switch to the Map: one glyph per session, same selection, same details.
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  await expect(page.getByRole('tab', { name: 'Map', exact: true })).toHaveAttribute('aria-selected', 'true');
  await expect(mapNodes(page)).toHaveCount(5);
  const svg = page.locator('.ins-map svg');
  await expect(svg).toHaveAttribute('aria-label', /^5 agents \(\d+ active\), 1 subagents, \d+ tools$/);
  await expect(svg).toHaveAttribute('shape-rendering', 'crispEdges');
  await expect(page.locator('.ins-map g.node.agent[aria-pressed="true"]')).toHaveCount(1);
  await expect(page.locator('.ins-map g.node.agent[aria-pressed="true"]')).toHaveAttribute('aria-label', /sub aaaa/);
  await expect(page.locator('.sel-title strong')).toContainText('sub aaaa');
  await expect(page.locator('.sel-actions').getByRole('button', { name: 'Open in terminal' })).toBeDisabled();
  // Glyphs are 20 px; nothing else moves them (no animation on the map).
  const box = await mapNodes(page).first().locator('rect, polygon').nth(1).boundingBox();
  expect(Math.round(box.width)).toBeGreaterThanOrEqual(20);
  expect(await page.locator('.ins-map').evaluate(el => el.querySelectorAll('animate, animateTransform, style').length)).toBe(0);
  // The choice survives opening another rail item and coming back.
  await rail(page, 'Terminals').click();
  await rail(page, 'Inspector').click();
  await expect(page.getByRole('tab', { name: 'Map', exact: true })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.ins-map g.node.agent[aria-pressed="true"]')).toHaveAttribute('aria-label', /sub aaaa/);
  await expect(page.locator('.sel-title strong')).toContainText('sub aaaa');
  // Back to Tiles shows the same selected tile.
  await page.getByRole('tab', { name: 'Tiles', exact: true }).click();
  await expect(tiles(page).filter({ hasText: 'sub aaaa' })).toHaveAttribute('aria-pressed', 'true');
});

test('Inspector: the Map walks by keyboard and gets richer as it gets wider', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 900 });
  await openTerminals(page, 0);
  await page.evaluate(sub => { globalThis.__fakeTerminals.extraSources = [sub]; }, SUBAGENT);
  await rail(page, 'Inspector').click();
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  await expect(mapNodes(page)).toHaveCount(5);
  // Exactly one glyph is in the tab order, and every glyph announces a name.
  await expect(page.locator('.ins-map g.node.agent[tabindex="0"]')).toHaveCount(1);
  for (const label of await mapNodes(page).evaluateAll(els => els.map(e => e.getAttribute('aria-label')))) expect(label).toMatch(/, (active|idle|inactive|other), /);
  // Home and End jump to the ends; Enter selects the focused glyph.
  await page.locator('.ins-map g.node.agent[tabindex="0"]').focus();
  await page.keyboard.press('End');
  const last = await page.evaluate(() => document.activeElement?.getAttribute('data-node'));
  await page.keyboard.press('Home');
  const first = await page.evaluate(() => document.activeElement?.getAttribute('data-node'));
  expect(first).not.toBe(last);
  expect(first).toBe(await mapNodes(page).first().getAttribute('data-node'));
  await page.keyboard.press('Enter');
  await expect(page.locator('.ins-map g.node.agent[aria-pressed="true"]')).toHaveAttribute('data-node', first);
  // Arrow keys move between neighbours without leaving the map.
  const before = await page.evaluate(() => document.activeElement?.getAttribute('data-node'));
  for (const key of ['ArrowDown', 'ArrowRight']) {
    await page.keyboard.press(key);
    if ((await page.evaluate(() => document.activeElement?.getAttribute('data-node'))) !== before) break;
  }
  expect(await page.evaluate(() => document.activeElement?.getAttribute('data-node'))).not.toBe(before);
  // The tool facts arrive from the native side and sit outside the block of agents. How much is drawn depends on the width:
  // families when narrow, single tools when wide. The picture is never wider than its pane, however narrow.
  const toolIds = () => page.locator('.ins-map g.node[data-node^="t:"]').evaluateAll(els => els.map(e => e.getAttribute('data-node')).sort());
  const fits = () => page.locator('.ins-map').evaluate(el => el.scrollWidth <= el.clientWidth + 1);
  await page.setViewportSize({ width: 700, height: 900 });
  await expect.poll(toolIds).toEqual(['t:file', 't:shell']);
  expect(await fits()).toBe(true);
  await page.setViewportSize({ width: 2600, height: 900 });
  await expect.poll(toolIds).toEqual(['t:Bash', 't:Read']);
  expect(await fits()).toBe(true);
  // The tools are outside the block of agents, one on each side.
  const frame = await page.locator('.ins-map .frame').boundingBox();
  const bash = await page.locator('.ins-map g.node[data-node="t:Bash"]').boundingBox();
  const read = await page.locator('.ins-map g.node[data-node="t:Read"]').boundingBox();
  expect(Math.min(bash.x, read.x)).toBeLessThan(frame.x);
  expect(Math.max(bash.x, read.x)).toBeGreaterThan(frame.x + frame.width);
  // Selecting an agent lights its routes to the tools it used.
  await page.locator('.ins-map g.node.agent').first().click();
  await expect(page.locator('.ins-map polyline.edge.call')).toHaveCount(2);
});

test('the map shows what is being used: tool names with usage bars, a lit route for a running call, and hover and click that follow tools and agents', async ({ page }) => {
  await page.setViewportSize({ width: 2400, height: 900 });
  await openTerminals(page, 0);
  await page.evaluate(() => {
    const ids = ['0123456789abcdef', 'fedcba9876543210', '1111111111111111', '2222222222222222'];
    globalThis.__fakeTerminals.mapPayload = { generated_ms: Date.now(), reports: [], sessions: [
      { id: ids[0], open_calls: 1, open_tools: ['Bash'], tools: [{ name: 'Bash', calls: 10, errors: 0 }, { name: 'Read', calls: 4, errors: 0 }] },
      { id: ids[1], open_calls: 0, open_tools: [], tools: [{ name: 'Read', calls: 6, errors: 1 }] },
      { id: ids[2], open_calls: 0, open_tools: [], tools: [{ name: 'Grep', calls: 2, errors: 0 }] },
      { id: ids[3], open_calls: 0, open_tools: [], tools: [] }] };
  });
  await rail(page, 'Inspector').click();
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  const names = page.locator('.ins-map .tool-name');
  await expect.poll(() => names.allTextContents().then(t => t.sort())).toEqual(['Bash', 'Grep', 'Read']);
  // Names are drawn beside the glyphs with a bar for how much each is used: Read (10 calls over 2 agents) is the busiest.
  const bars = await page.locator('.ins-map .tool-bar').evaluateAll(els => els.map(e => Number(e.getAttribute('width'))));
  expect(Math.max(...bars)).toBe(92);
  expect(Math.min(...bars)).toBeGreaterThanOrEqual(2);
  // A call that is running right now is lit from the agent to the tool, with nothing selected.
  await expect(page.locator('.ins-map polyline.edge.live')).toHaveCount(1);
  await expect(page.locator('.ins-map g.node[data-node="t:Bash"] .live-ring')).toHaveCount(1);
  await expect(page.locator('.ins-map g.node[data-node="t:Read"] .live-ring')).toHaveCount(0);
  await expect(page.locator('.ins-map .tool-name.live')).toHaveText('Bash');
  // Pointing at an agent lights its routes to the tools it uses and dims the other tools.
  await page.locator('.ins-map g.node.agent[data-node="s:0123456789abcdef"]').hover();
  await expect(page.locator('.ins-map polyline.edge.call')).toHaveCount(2);
  await expect(page.locator('.ins-map g.node.tool.dim')).toHaveCount(1); // Grep
  await page.mouse.move(5, 5);
  await expect(page.locator('.ins-map polyline.edge.call')).toHaveCount(0);
  // Pointing at a tool dims the agents that do not use it; clicking pins that.
  await page.locator('.ins-map g.node.tool[data-node="t:Read"]').hover();
  await expect(page.locator('.ins-map g.node.agent.dim')).toHaveCount(2); // the Grep user and the agent with no tools
  await page.locator('.ins-map g.node.tool[data-node="t:Read"]').click();
  await page.mouse.move(5, 5);
  await expect(page.locator('.ins-map g.node.agent.dim')).toHaveCount(2);
  await expect(page.locator('.ins-map g.node.tool[data-node="t:Read"]')).toHaveAttribute('aria-pressed', 'true');
  await page.locator('.ins-map g.node.tool[data-node="t:Read"]').click();
  await page.mouse.move(5, 5);
  await expect(page.locator('.ins-map g.node.agent.dim')).toHaveCount(0);
  // The tooltip says who uses a tool and how much, in words.
  await expect(page.locator('.ins-map g.node.tool[data-node="t:Read"] title')).toHaveText('Read: 2 agents, 10 calls, 1 error');
});

test('Inspector: the Map can be limited to a state group, like the tiles', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 900 });
  await openTerminals(page, 0);
  await page.evaluate(sub => { globalThis.__fakeTerminals.extraSources = [sub]; }, SUBAGENT);
  await rail(page, 'Inspector').click();
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  const tabs = page.getByRole('tablist', { name: 'Session state' }).getByRole('tab');
  await expect(tabs).toHaveText(['All5', 'Active2', 'Idle1', 'Inactive1', 'Other1']);
  await expect(mapNodes(page)).toHaveCount(5);
  // Active: the agent running in the background and the subagent that just wrote.
  await page.getByRole('tab', { name: /^Active/ }).click();
  await expect(mapNodes(page)).toHaveCount(2);
  await expect(page.locator('.ins-map svg')).toHaveAttribute('aria-label', /^2 agents/);
  await page.getByRole('tab', { name: /^Idle/ }).click();
  await expect(mapNodes(page)).toHaveCount(1);
  await page.getByRole('tab', { name: /^Inactive/ }).click();
  await expect(mapNodes(page)).toHaveCount(1);
  await page.getByRole('tab', { name: /^Other/ }).click();
  await expect(mapNodes(page)).toHaveCount(1);
  // The choice is shared with the tiles, and All shows everything again.
  await page.getByRole('tab', { name: 'Tiles', exact: true }).click();
  await expect(tiles(page)).toHaveCount(1);
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  await page.getByRole('tab', { name: /^All/ }).click();
  await expect(mapNodes(page)).toHaveCount(5);
  // The agent filter applies as well: Pi alone is one session.
  await page.getByLabel('Filter by agent').selectOption('pi');
  await expect(mapNodes(page)).toHaveCount(1);
  await expect(tabs).toHaveText(['All1', 'Active0', 'Idle1', 'Inactive0', 'Other0']);
});

test('a watch window shows the last output line too', async ({ page }) => {
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  await act(page, 0, 'Watch', true);
  await rail(page, 'Canvas').click();
  await expect(page.locator('.float-layer .float-win.watch .activity .last')).toContainText('Done: all tests pass', { timeout: 15000 });
});

test('the background shows behind both sidebars, each with its own opacity that is saved and kept', async ({ page, bridge }) => {
  await page.route('http://background.localhost/**', route => route.fulfill({ contentType: 'image/png', body: tinyPng }));
  await page.goto('/');
  await propsButton(page).click();
  const dialog = propsDialog(page);
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  await page.evaluate(asset => { globalThis.__fakeTerminals.background = asset; }, asset);
  await dialog.getByRole('combobox', { name: 'Mode' }).selectOption('image');
  await expect(page.locator('img.media')).toHaveCount(1);
  // The image spans the whole body, under the module list and the right sidebar as well as the work area.
  const shell = await page.locator('.shell').boundingBox();
  const media = await page.locator('img.media').boundingBox();
  expect(Math.abs(media.width - shell.width) < 2 && Math.abs(media.height - shell.height) < 2).toBe(true);
  const alpha = sel => page.locator(sel).evaluate(el => {
    // Computed colours come back as rgb(), rgba() or color(srgb r g b / a); the alpha is after a slash or the fourth number.
    const c = getComputedStyle(el).backgroundColor;
    const slash = c.match(/\/\s*([0-9.]+)/);
    if (slash) return Number(slash[1]);
    const comma = c.match(/rgba\(([^)]+)\)/);
    return comma ? Number(comma[1].split(',')[3]) : 1;
  });
  expect(await alpha('.rail')).toBe(1);
  expect(await alpha('aside')).toBe(1);
  // Two sliders, named as asked; each changes only its own sidebar, and live.
  const one = dialog.getByRole('slider', { name: 'Sidebar 1 opacity' });
  const two = dialog.getByRole('slider', { name: 'Sidebar 2 opacity' });
  await one.fill('0');
  await expect.poll(() => alpha('.rail')).toBeLessThan(0.05);
  expect(await alpha('aside')).toBe(1);
  await two.fill('50');
  await expect.poll(() => alpha('aside')).toBeGreaterThan(0.4);
  expect(await alpha('aside')).toBeLessThan(0.6);
  expect(await alpha('.rail')).toBeLessThan(0.05);
  // Saved when released, and remembered after a reload.
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.settings.appearance)).toEqual({ sidebar1_opacity: 0, sidebar2_opacity: 50 });
  await page.reload();
  await expect.poll(() => alpha('.rail')).toBeLessThan(0.05);
  await expect.poll(() => alpha('aside')).toBeLessThan(0.6);
  // The rail's buttons stay usable while it is clear.
  await rail(page, 'Inspector').click();
  await expect(page.locator('.inspector')).toBeVisible();
  // The high-contrast theme keeps the sidebars solid whatever the sliders say.
  await propsButton(page).click();
  await propsDialog(page).getByRole('tab', { name: 'Appearance' }).click();
  await propsDialog(page).getByRole('combobox', { name: 'Theme', exact: true }).selectOption('contrast');
  await expect.poll(() => alpha('.rail')).toBe(1);
  await expect.poll(() => alpha('aside')).toBe(1);
});

test('Properties > Data lists only the places that exist here, and can show one in Explorer or copy its path', async ({ page }) => {
  await openTerminals(page, 0);
  await propsButton(page).click();
  const dialog = propsDialog(page);
  await dialog.getByRole('tab', { name: 'Data' }).click();
  const list = dialog.getByRole('list', { name: 'Places on this computer' });
  await expect(list.getByRole('listitem')).toHaveCount(3);
  await expect(list).toContainText('App data (settings, layout, tags)');
  await expect(list).toContainText('3 encrypted backups, newest 2 h ago');
  await expect(list).toContainText('agent-workbench-private-backup');
  await expect(dialog).toContainText('never be shared');
  // A place that is not on this machine has no row: the list is whatever the native side found.
  await expect(list).not.toContainText('Stable copy');
  await expect(list).not.toContainText('Pi sessions');
  // Folders say Open, files say Show; opening sends only the id.
  await list.getByRole('listitem').nth(0).getByRole('button', { name: 'Open' }).click();
  await list.getByRole('listitem').nth(1).getByRole('button', { name: 'Show' }).click();
  expect(await page.evaluate(() => globalThis.__fakeTerminals.opened)).toEqual(['app_data', 'settings']);
  // With less on the machine there are fewer rows.
  await page.evaluate(() => { globalThis.__fakeTerminals.places = globalThis.__fakeTerminals.places.slice(0, 1); });
  await dialog.getByRole('tab', { name: 'Agents' }).click();
  await dialog.getByRole('tab', { name: 'Data' }).click();
  await expect(list.getByRole('listitem')).toHaveCount(1);
});

test('a typed key shows up quickly: the pane reads the echo right after sending the key, not on its slow idle timer', async ({ page }) => {
  await openTerminals(page, 1);
  await page.evaluate(() => { globalThis.__fakeTerminals.echo = true; });
  await page.locator('.terminal-list .pick').first().click();
  const pane = page.locator('.panes .pane-body').first();
  await expect(pane.locator('.xterm')).toBeVisible();
  await pane.click();
  // Let the pane settle into its idle polling first (the worst case for the first keystroke).
  await page.waitForTimeout(1500);
  await page.evaluate(() => {
    globalThis.__t0 = 0;
    document.addEventListener('keydown', () => { globalThis.__t0 = performance.now(); }, true);
    globalThis.__echoed = key => new Promise(resolve => {
      const rows = () => document.querySelector('.panes .pane-body .xterm-rows')?.textContent ?? '';
      const before = rows().split(key).length;
      const tick = () => {
        if (rows().split(key).length > before) resolve(performance.now() - globalThis.__t0);
        else if (globalThis.__t0 > 0 && performance.now() - globalThis.__t0 > 3000) resolve(3000);
        else requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });
  });
  const latencies = [];
  for (const key of 'qzxwkv') {
    const waiting = page.evaluate(k => globalThis.__echoed(k), key);
    await page.keyboard.press(key);
    latencies.push(await waiting);
    await page.waitForTimeout(500);
  }
  latencies.sort((a, b) => a - b);
  const median = latencies[Math.floor(latencies.length / 2)];
  test.info().annotations.push({ type: 'median-echo-ms', description: String(Math.round(median)) });
  expect(median, `echo latencies ${latencies.map(Math.round)}`).toBeLessThan(60);
  expect(Math.max(...latencies), `echo latencies ${latencies.map(Math.round)}`).toBeLessThan(120);
});

test('squeezing and stretching the window while output flows never breaks the emulator, and the shell is told the same clamped size', async ({ page }) => {
  await openTerminals(page, 1);
  await page.evaluate(() => { globalThis.__fakeTerminals.echo = true; });
  await page.locator('.terminal-list .pick').first().click();
  const pane = page.locator('.panes .pane-body').first();
  await expect(pane.locator('.xterm')).toBeVisible();
  await pane.click();
  for (const [w, h] of [[1440, 960], [420, 300], [1100, 700], [360, 240], [1440, 960], [500, 420]]) {
    await page.setViewportSize({ width: w, height: h });
    await page.keyboard.type('squeeze-' + w + ' '.repeat(3) + 'x'.repeat(90));
    await page.waitForTimeout(350);
  }
  await expect(pane.locator('.xterm-rows')).toContainText('squeeze-500');
  const sizes = await page.evaluate(() => globalThis.__fakeTerminals.sizes ?? []);
  expect(sizes.length).toBeGreaterThan(1);
  for (const [cols, rows] of sizes) {
    expect(cols).toBeGreaterThanOrEqual(20);
    expect(rows).toBeGreaterThanOrEqual(5);
  }
  // The emulator itself was resized to the size the shell was told (not to a sliver).
  const last = sizes.at(-1);
  const shown = await pane.locator('.xterm-rows > div').count();
  expect(shown).toBe(last[1]);
});

test('output arrives through the shared watcher: idle panes cost one watch a second, not a read per pane per tick', async ({ page }) => {
  await openTerminals(page, 8);
  await page.evaluate(() => { globalThis.__fakeTerminals.echo = true; });
  for (let i = 0; i < 8; i++) await page.locator('.terminal-list .pick').nth(i).click();
  await page.waitForTimeout(1500);
  const before = await page.evaluate(() => ({ reads: globalThis.__fakeTerminals.reads, watches: globalThis.__fakeTerminals.watches }));
  await page.waitForTimeout(3000);
  const after = await page.evaluate(() => ({ reads: globalThis.__fakeTerminals.reads, watches: globalThis.__fakeTerminals.watches }));
  expect(after.reads - before.reads, 'panes do not poll while the watcher works').toBe(0);
  expect(after.watches - before.watches, 'about one watch per second').toBeLessThanOrEqual(6);
  // And typing still shows up.
  const pane = page.locator('.panes .pane-body').first();
  await pane.click();
  await page.keyboard.type('watched-echo');
  await expect(pane.locator('.xterm-rows')).toContainText('watched-echo');
});

test('when the watcher does not work the panes poll instead and keep working', async ({ page }) => {
  await openTerminals(page, 3);
  await page.evaluate(() => { globalThis.__fakeTerminals.echo = true; globalThis.__fakeTerminals.watchOff = true; });
  for (let i = 0; i < 3; i++) await page.locator('.terminal-list .pick').nth(i).click();
  const pane = page.locator('.panes .pane-body').first();
  await pane.click();
  await page.keyboard.type('polled-echo');
  await expect(pane.locator('.xterm-rows')).toContainText('polled-echo');
  const reads = await page.evaluate(() => globalThis.__fakeTerminals.reads);
  expect(reads).toBeGreaterThan(0);
  // After the watcher failed three times it is left alone for a while instead of being retried every second.
  const watches = await page.evaluate(() => globalThis.__fakeTerminals.watches);
  await page.waitForTimeout(2500);
  expect(await page.evaluate(() => globalThis.__fakeTerminals.watches) - watches).toBeLessThanOrEqual(1);
});

/** How well the emulator's screen fills the space its parent really offers (the parent's padding is not available). */
const screenFit = page => page.evaluate(() => {
  const screen = document.querySelector('.float-win .xterm-screen');
  const parent = screen.closest('.xterm').parentElement;
  const cs = getComputedStyle(parent);
  const r = parent.getBoundingClientRect();
  const innerTop = r.top + parseFloat(cs.paddingTop) + parseFloat(cs.borderTopWidth);
  const innerBottom = r.bottom - parseFloat(cs.paddingBottom) - parseFloat(cs.borderBottomWidth);
  const s = screen.getBoundingClientRect();
  const rows = document.querySelectorAll('.float-win .xterm-rows > div').length;
  return { over: s.bottom - innerBottom, slack: innerBottom - s.bottom, cell: s.height / rows, rows, top: s.top - innerTop };
});

test('a terminal window shows exactly the rows that fit it, never one too many, at several sizes while output streams', async ({ page }) => {
  await openTerminals(page, 1);
  await page.evaluate(() => {
    const f = globalThis.__fakeTerminals;
    f.echo = true;
    f.stream[f.sessions[0].id] = Array.from({ length: 300 }, (_, i) => `streamed response line ${i} ${'x'.repeat(i % 60)}`).join('\r\n') + '\r\n';
  });
  await page.locator('.terminal-list li').first().locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(winOf(page, 0).locator('.xterm-rows')).toContainText('streamed response line 299');
  for (const [dx, dy] of [[0, 0], [120, 60], [-90, -41], [37, -33], [-20, 71], [0, 13]]) {
    const handle = await winOf(page, 0).locator('.float-resize').boundingBox();
    await page.mouse.move(handle.x + 8, handle.y + 8);
    await page.mouse.down();
    await page.mouse.move(handle.x + 8 + dx, handle.y + 8 + dy, { steps: 3 });
    await page.mouse.up();
    await page.waitForTimeout(500); // the resize is debounced
    const fit = await screenFit(page);
    expect(fit.over, `${JSON.stringify(fit)} after resizing by ${dx},${dy}`).toBeLessThanOrEqual(0.5);
    expect(fit.slack, `a spare row would still fit: ${JSON.stringify(fit)}`).toBeLessThan(fit.cell + 0.5);
  }
  // Typing several lines keeps the bottom line (where the cursor is) inside the window.
  await winOf(page, 0).locator('.xterm').click();
  for (let i = 0; i < 4; i++) await page.keyboard.type(`typed ${i}\r`);
  await page.waitForTimeout(300);
  const fit = await screenFit(page);
  expect(fit.over).toBeLessThanOrEqual(0.5);
});

test('Shift held while dragging or resizing a terminal window shows the grid and snaps it to 20 px; without Shift it moves freely', async ({ page }) => {
  await openTerminals(page, 1);
  await page.locator('.terminal-list li').first().locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(1);
  const layer = await page.locator('.float-layer').boundingBox();
  const pos = async () => { const b = await winOf(page, 0).boundingBox(); return { x: Math.round(b.x - layer.x), y: Math.round(b.y - layer.y), w: Math.round(b.width), h: Math.round(b.height) }; };
  const grid = page.locator('.float-grid');
  // Free movement: no grid, any position.
  let g = await gripBox(page, 0);
  await page.mouse.move(g.x + 20, g.y + 10);
  await page.mouse.down();
  await page.mouse.move(g.x + 20 + 33, g.y + 10 + 27, { steps: 3 });
  await expect(grid).toHaveCount(0);
  await page.mouse.up();
  const free = await pos();
  expect(free.x % 20 !== 0 || free.y % 20 !== 0).toBe(true);
  // Shift pressed BEFORE the pointer goes down: the grid shows during the drag and the window lands on the grid.
  g = await gripBox(page, 0);
  await page.keyboard.down('Shift');
  await page.mouse.move(g.x + 20, g.y + 10);
  await page.mouse.down();
  await page.mouse.move(g.x + 20 + 57, g.y + 10 + 41, { steps: 3 });
  await expect(grid).toHaveCount(1);
  await page.mouse.up();
  await expect(grid).toHaveCount(0);
  const snapped = await pos();
  expect(snapped.x % 20).toBe(0);
  expect(snapped.y % 20).toBe(0);
  // Shift pressed in the middle of a drag also shows it.
  await page.keyboard.up('Shift');
  g = await gripBox(page, 0);
  await page.mouse.move(g.x + 20, g.y + 10);
  await page.mouse.down();
  await page.mouse.move(g.x + 20 - 25, g.y + 10 + 13, { steps: 2 });
  await expect(grid).toHaveCount(0);
  await page.keyboard.down('Shift');
  await expect(grid).toHaveCount(1);
  await page.mouse.up();
  await page.keyboard.up('Shift');
  await expect(grid).toHaveCount(0);
  // Resizing with Shift lands on the grid too.
  const handle = await winOf(page, 0).locator('.float-resize').boundingBox();
  await page.keyboard.down('Shift');
  await page.mouse.move(handle.x + 8, handle.y + 8);
  await page.mouse.down();
  await page.mouse.move(handle.x + 8 + 71, handle.y + 8 + 29, { steps: 3 });
  await page.mouse.up();
  await page.keyboard.up('Shift');
  const resized = await pos();
  expect(resized.w % 20).toBe(0);
  expect(resized.h % 20).toBe(0);
});

test('Ctrl+Alt+G makes every tab see-through: no background, the interactive areas reported, the view parts usable', async ({ page }) => {
  await openTerminals(page, 1);
  await page.locator('.terminal-list li').first().locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(1);
  const canvas = page.locator('.canvas');
  const reports = () => page.evaluate(() => globalThis.__fakeTerminals.ghost.reports);
  const last = async () => { const r = await reports(); return r[r.length - 1]; };
  const inside = (rects, x, y) => rects.some(([rx, ry, rw, rh]) => x >= rx && x < rx + rw && y >= ry && y < ry + rh);
  const bg = () => canvas.evaluate(el => { const c = getComputedStyle(el).backgroundColor; return /(\/ 0\)|, 0\))$/.test(c) ? 'clear' : c; });
  expect(await bg()).not.toBe('clear');
  // On.
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toBeVisible();
  await expect(page.locator('.ghost-note')).toContainText('Ctrl+Alt+G');
  await expect.poll(bg).toBe('clear');
  await expect.poll(async () => (await last())?.effective).toBe(true);
  // The window, the sidebars, the title bar and the status bar are interactive; bare canvas is not.
  const win = await wins(page).first().boundingBox();
  const bare = await canvas.boundingBox();
  const r = (await last()).rects;
  expect(inside(r, win.x + win.width / 2, win.y + win.height / 2)).toBe(true);
  expect(inside(r, 20, 20)).toBe(true); // title bar
  const rail0 = await page.locator('nav.rail').boundingBox();
  expect(inside(r, rail0.x + 10, rail0.y + 100)).toBe(true);
  const aside = await page.locator('aside').boundingBox();
  expect(inside(r, aside.x + 10, aside.y + 100)).toBe(true);
  expect(inside(r, bare.x + bare.width / 2, bare.y + bare.height - 120)).toBe(false);
  // Moving the window moves its area (the report follows the layout).
  const g = await gripBox(page, 0);
  await page.mouse.move(g.x + 20, g.y + 10);
  await page.mouse.down();
  await page.mouse.move(g.x + 220, g.y + 150, { steps: 4 });
  await page.mouse.up();
  const moved = await wins(page).first().boundingBox();
  await expect.poll(async () => inside((await last()).rects, moved.x + moved.width / 2, moved.y + moved.height / 2)).toBe(true);
  expect(inside((await last()).rects, win.x + 10, win.y + 10)).toBe(moved.x < win.x + 10 && moved.y < win.y + 10 ? true : false);
  // Every tab: the view's backdrop is clear and the gaps between its parts let clicks through; its parts stay usable.
  await rail(page, 'Terminals').click();
  await expect(page.locator('.ghost-note')).toBeVisible();
  await expect.poll(async () => (await last())?.effective).toBe(true);
  const view = page.locator('section.terminals');
  expect(await view.evaluate(el => getComputedStyle(el).backgroundColor)).toBe('rgba(0, 0, 0, 0)');
  const head = await page.locator('.terminal-head').boundingBox();
  await expect.poll(async () => inside((await last()).rects, head.x + 20, head.y + head.height / 2)).toBe(true);
  // A gap in the view (between the hint lines and the launch row) lets clicks through.
  const hint = await page.locator('.terminal-hint').first().boundingBox();
  const gapY = hint.y - 3;
  expect(await page.evaluate(y => document.elementFromPoint(400, y)?.className ?? '', gapY)).toContain('terminals');
  const covering = async () => {
    const hits = (await last()).rects.filter(([rx, ry, rw, rh]) => 400 >= rx && 400 < rx + rw && gapY >= ry && gapY < ry + rh);
    // Name what covers the point, if anything (the reported rectangles are in physical pixels).
    const names = await page.evaluate(hits => hits.map(([x, y, w, h]) => [...document.querySelectorAll('*')].filter(el => {
      const r = el.getBoundingClientRect(), s = window.devicePixelRatio || 1;
      return Math.round(r.left * s) === x && Math.round(r.top * s) === y && Math.round(r.width * s) === w && Math.round(r.height * s) === h;
    }).map(el => `${el.tagName}.${el.className}`).join('|')), hits);
    return names;
  };
  await expect.poll(covering).toEqual([]);
  await rail(page, 'Inspector').click();
  await expect(page.locator('.ghost-note')).toBeVisible();
  expect(await page.locator('.inspector').evaluate(el => getComputedStyle(el).backgroundColor)).toBe('rgba(0, 0, 0, 0)');
  await rail(page, 'Canvas').click();
  await expect(page.locator('.ghost-note')).toBeVisible();
  await expect.poll(async () => (await last())?.effective).toBe(true);
  // A dialog takes the whole window.
  await page.getByRole('button', { name: 'Open properties' }).click();
  await expect.poll(async () => (await last())?.whole).toBe(true);
  await page.keyboard.press('Escape');
  await expect.poll(async () => (await last())?.whole).toBe(false);
  // The native window is made transparent while it is on, and the chosen background comes back afterwards.
  const kinds = () => page.evaluate(() => (globalThis.__fakeTerminals.appearances ?? []).map(a => a.background_kind));
  expect((await kinds()).at(-1)).toBe('transparent');
  // Off.
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toHaveCount(0);
  await expect.poll(async () => (await last())?.effective).toBe(false);
  expect(await bg()).not.toBe('clear');
  await expect.poll(async () => (await kinds()).at(-1)).toBe('solid');
});

test('the title bar still appears when the cursor is pushed past the top edge of a window that is not maximized', async ({ page }) => {
  await page.addInitScript(() => { if (!sessionStorage.getItem('tb-first')) { sessionStorage.setItem('tb-first', '1'); localStorage.removeItem('aw.titlebar'); } });
  await page.goto('/');
  const bar = page.locator('header.titlebar');
  const hot = page.locator('.titlebar-hot');
  // The strip is taller than a resize border, so part of it can be reached.
  expect((await hot.boundingBox()).height).toBeGreaterThanOrEqual(10);
  // Entering it, then leaving out of the page (no element entered, as when the cursor goes into the resize border) keeps the dwell going.
  await page.mouse.move(400, 300);
  await page.mouse.move(400, 6);
  await hot.dispatchEvent('pointerleave', { relatedTarget: null });
  await expect(bar).toHaveClass(/\bshown\b/, { timeout: 2000 });
  // Moving back down into the page cancels it, as before.
  await page.mouse.move(600, 400);
  await expect(bar).not.toHaveClass(/\bshown\b/, { timeout: 3000 });
  await page.mouse.move(400, 6);
  await page.mouse.move(400, 300);
  await page.waitForTimeout(600);
  await expect(bar).not.toHaveClass(/\bshown\b/);
});

test('the title bar is half height, slides away, and comes back only when the cursor rests on the top edge for a moment or something in it has focus', async ({ page }) => {
  // Not pinned: clear the fixture's setting once, so a reload would keep whatever the page stored.
  await page.addInitScript(() => { if (!sessionStorage.getItem('tb-first')) { sessionStorage.setItem('tb-first', '1'); localStorage.removeItem('aw.titlebar'); } });
  await page.goto('/');
  const bar = page.locator('header.titlebar');
  const hidden = async () => (await bar.boundingBox()).y + (await bar.boundingBox()).height <= 1;
  await expect(bar).toHaveClass(/\bautohide\b/);
  expect((await bar.boundingBox()).height).toBeLessThan(30);
  expect(await hidden()).toBe(true);
  // The content uses the full height: the shell starts at the very top.
  expect((await page.locator('.shell').boundingBox()).y).toBeLessThan(2);
  // Passing through the top edge quickly does nothing.
  await page.mouse.move(400, 300);
  await page.mouse.move(400, 1);
  await page.waitForTimeout(100);
  await page.mouse.move(400, 300);
  await page.waitForTimeout(500);
  expect(await hidden()).toBe(true);
  // Resting there for a quarter of a second reveals it.
  await page.mouse.move(400, 1);
  await page.waitForTimeout(150);
  expect(await hidden()).toBe(true);
  await page.waitForTimeout(250);
  await expect(bar).toHaveClass(/\bshown\b/);
  await expect.poll(async () => (await bar.boundingBox()).y).toBeGreaterThanOrEqual(-1);
  await expect(page.getByRole('button', { name: 'Open properties' })).toBeVisible();
  // It stays while the cursor is on it, and goes when the cursor leaves.
  await page.mouse.move(600, 12);
  await page.waitForTimeout(700);
  await expect(bar).toHaveClass(/\bshown\b/);
  await page.mouse.move(600, 400);
  await expect(bar).not.toHaveClass(/\bshown\b/, { timeout: 3000 });
  await expect.poll(hidden).toBe(true);
  // Keyboard focus inside it keeps it showing.
  await page.keyboard.press('Tab');
  await expect(bar).toHaveClass(/\bshown\b|\bautohide\b/);
  await bar.getByRole('button', { name: 'Open properties' }).focus();
  await expect.poll(async () => (await bar.boundingBox()).y).toBeGreaterThanOrEqual(-1);
});

test('a development build says so in the title bar: an amber mark and a DEV BUILD badge; the normal build shows neither', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('header.titlebar .brandmark')).toBeVisible();
  await expect(page.locator('header.titlebar .brandmark')).not.toHaveClass(/\bdev\b/);
  await expect(page.locator('header.titlebar .badge.dev')).toHaveCount(0);
  const mark = await page.locator('header.titlebar .brandmark').boundingBox();
  expect(mark.width).toBeGreaterThan(14);
  expect(mark.height).toBeGreaterThan(14);
  const ink = await page.locator('header.titlebar .brandmark path').evaluate(el => getComputedStyle(el).fill);
  expect(ink).toBe('rgb(233, 236, 239)');

  const dev = await page.context().newPage();
  await dev.addInitScript(() => { globalThis.__awRuntimeVersion = '0.1.0-dev'; try { localStorage.setItem('aw.titlebar', 'pinned'); } catch { /* storage unavailable */ } });
  await dev.goto('/');
  await expect(dev.locator('header.titlebar .badge.dev')).toHaveText('DEV BUILD');
  await expect(dev.locator('header.titlebar .brandmark')).toHaveClass(/\bdev\b/);
  expect(await dev.locator('header.titlebar .brandmark path').evaluate(el => getComputedStyle(el).fill)).toBe('rgb(232, 163, 61)');
  await dev.close();
});

test('terminals lost in a restart are listed with a Restore button and a Restore all that recreates them, agents included', async ({ page }) => {
  await page.goto('/');
  await page.evaluate(() => {
    const f = globalThis.__fakeTerminals;
    f.sessions = [
      { id: '00000a01', label: 'build', state: 'lost', persistent: true, cols: 100, rows: 30, host_pid: 1 },
      { id: '00000a02', label: 'Claude Code · proj', state: 'lost', persistent: true, cols: 110, rows: 33, host_pid: 2 },
      { id: '00000a03', label: 'still here', state: 'running', persistent: true, cols: 100, rows: 30, host_pid: 3 },
    ];
    f.agents = [{ terminal: '00000a02', agent: 'claude', cwd: 'C:\Users\me\proj', launched_ms: Date.now() }];
  });
  await page.reload();
  await page.getByRole('button', { name: /Terminals/ }).click();
  const strip = page.getByRole('group', { name: 'Terminals that did not survive a restart' });
  await expect(strip).toBeVisible();
  await expect(strip).toContainText('2 terminals did not survive the last restart');
  // Only lost terminals have a Restore button.
  await expect(page.locator('.terminal-list li button.restore')).toHaveCount(2);
  await expect(page.locator('.terminal-list li').filter({ hasText: 'still here' }).locator('button.restore')).toHaveCount(0);
  // One at a time: the plain terminal comes back, the strip counts down.
  await page.locator('.terminal-list li').filter({ hasText: 'build' }).locator('button.restore').click();
  await expect(strip).toContainText('1 terminal did not survive');
  expect(await page.evaluate(() => globalThis.__fakeTerminals.restored)).toEqual([{ terminal: '00000a01', agent: false }]);
  // Restore all: the agent's terminal goes through the agent command (which resumes it).
  await strip.getByRole('button', { name: 'Restore all' }).click();
  await expect(strip).toHaveCount(0);
  expect(await page.evaluate(() => globalThis.__fakeTerminals.restored)).toEqual([{ terminal: '00000a01', agent: false }, { terminal: '00000a02', agent: 'claude' }]);
  await expect(page.locator('.terminal-list li button.restore')).toHaveCount(0);
  await expect(page.locator('.terminal-list li').filter({ hasText: 'build' })).toContainText('running');
});

test('with nothing lost there is no Previous terminals strip', async ({ page }) => {
  await openTerminals(page, 2);
  await expect(page.getByRole('group', { name: 'Terminals that did not survive a restart' })).toHaveCount(0);
});

test('Properties can keep the title bar always showing, and the choice is kept after a reload', async ({ page }) => {
  // Never pinned by storage, on every load (this runs after the fixture's pin), so only the setting decides.
  await page.addInitScript(() => { try { localStorage.removeItem('aw.titlebar'); } catch { /* storage unavailable */ } });
  await page.goto('/');
  const bar = page.locator('header.titlebar');
  await expect(bar).toHaveClass(/\bautohide\b/);
  // The bar is slid away; focusing the gear brings it down (focus inside keeps it showing).
  await propsButton(page).focus();
  await page.keyboard.press('Enter');
  const dialog = propsDialog(page);
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  const box = dialog.getByRole('checkbox', { name: /Hide the title bar until the cursor rests on the top edge/ });
  await expect(box).toBeChecked();
  await box.uncheck();
  await expect(bar).not.toHaveClass(/\bautohide\b/);
  await expect(page.locator('.titlebar-hot')).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.settings.window.titlebar_autohide)).toBe(false);
  await page.reload();
  await expect(bar).not.toHaveClass(/\bautohide\b/);
  expect((await bar.boundingBox()).y).toBeGreaterThanOrEqual(0);
  // And back.
  await propsButton(page).click();
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  await box.check();
  await expect(bar).toHaveClass(/\bautohide\b/);
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.settings.window.titlebar_autohide)).toBe(true);
});

test('the canvas background mode, alpha and the see-through canvas are remembered for the next start', async ({ page }) => {
  await page.goto('/');
  await propsButton(page).click();
  const dialog = propsDialog(page);
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  await dialog.getByRole('combobox', { name: 'Mode' }).selectOption('transparent');
  await dialog.getByRole('slider', { name: /Canvas \/ media alpha/ }).fill('40');
  const saved = () => page.evaluate(() => globalThis.__fakeTerminals.settings.window);
  await expect.poll(saved).toMatchObject({ canvas_mode: 'transparent', canvas_alpha: 40, see_through: false });
  await page.keyboard.press('Escape');
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toBeVisible();
  await expect.poll(async () => (await saved()).see_through).toBe(true);
  // A new start (the native mode starts off): the saved values come back, see-through included, note and all.
  await page.reload();
  await expect(page.locator('.ghost-note')).toBeVisible();
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.ghost.on)).toBe(true);
  await propsButton(page).click();
  await dialog.getByRole('tab', { name: 'Appearance' }).click();
  await expect(dialog.getByRole('combobox', { name: 'Mode' })).toHaveValue('transparent');
  await expect(dialog.getByRole('slider', { name: /Canvas \/ media alpha/ })).toHaveValue('40');
  await page.keyboard.press('Escape');
  // Turning it off is remembered too.
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toHaveCount(0);
  await expect.poll(async () => (await saved()).see_through).toBe(false);
  await page.reload();
  await page.waitForTimeout(300);
  await expect(page.locator('.ghost-note')).toHaveCount(0);
});

test('Sign out shows the start screen without ending anything; it stays after a reload and Sign in returns', async ({ page }) => {
  await openTerminals(page, 2);
  await page.getByRole('button', { name: 'Sign out' }).click();
  const start = page.getByRole('dialog', { name: 'Agent Workbench start screen' });
  await expect(start).toBeVisible();
  await expect(start).toContainText('Signed out. Your terminals and agents keep running.');
  const sessions = start.getByRole('region', { name: 'Sessions' });
  await expect(sessions).toContainText('Running 2');
  await expect(sessions).toContainText('T1');
  const providers = start.getByRole('region', { name: 'Providers' });
  for (const name of ['Claude Code', 'Codex', 'Pi', 'Laya', 'Jev']) await expect(providers).toContainText(name);
  await expect(providers).toContainText('Not supported');
  await expect(start.getByRole('button', { name: 'Sign in', exact: true })).toBeFocused();
  // The window controls stay reachable above it.
  await expect(page.getByRole('button', { name: 'Minimize window' })).toBeVisible();
  await page.reload();
  await expect(start).toBeVisible();
  await start.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(start).toHaveCount(0);
  // Nothing was ended.
  expect(await page.evaluate(() => globalThis.__fakeTerminals.sessions.length)).toBe(2);
  await page.reload();
  await expect(start).toHaveCount(0);
});

test('the module list starts with Canvas, Inspector and Terminals, then the Sessions and Files panel buttons', async ({ page }) => {
  await page.goto('/');
  await expect.poll(async () => (await page.locator('nav.rail > button .nav-label').allTextContents()).slice(0, 5)).toEqual(['Canvas', 'Inspector', 'Terminals', 'Sessions', 'Files']);
});

test('Sessions and Files open in the panel beside the workspace; clicking the open one closes it, and the choice is kept', async ({ page }) => {
  await page.goto('/');
  const sessionsBtn = page.getByRole('button', { name: 'Sessions panel', exact: true });
  const filesBtn = page.getByRole('button', { name: 'Files panel', exact: true });
  await expect(sessionsBtn).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('complementary', { name: 'Session roster and inspector' })).toBeVisible();
  await filesBtn.click();
  await expect(page.getByRole('complementary', { name: 'Files panel' })).toBeVisible();
  await expect(page.getByRole('complementary', { name: 'Session roster and inspector' })).toHaveCount(0);
  await filesBtn.click();
  await expect(page.locator('aside')).toHaveCount(0);
  // The workspace takes the room.
  const shell = await page.locator('.shell').boundingBox();
  const main = await page.locator('main').boundingBox();
  expect(main.x + main.width).toBeGreaterThan(shell.x + shell.width - 2);
  await page.reload();
  await expect(page.locator('aside')).toHaveCount(0);
  await sessionsBtn.click();
  await expect(page.getByRole('complementary', { name: 'Session roster and inspector' })).toBeVisible();
});

test('Swap sides puts the module list on the right and the panel on the left, and back', async ({ page }) => {
  await page.goto('/');
  const left = async sel => (await page.locator(sel).boundingBox()).x;
  expect(await left('nav.rail')).toBeLessThan(await left('aside'));
  await page.getByRole('button', { name: 'Swap sides' }).first().click();
  await expect.poll(async () => (await left('nav.rail')) > (await left('aside'))).toBe(true);
  expect(await left('main')).toBeGreaterThan(await left('aside'));
  expect(await left('main')).toBeLessThan(await left('nav.rail'));
  await page.reload();
  await expect.poll(async () => (await left('nav.rail')) > (await left('aside'))).toBe(true);
  await page.getByRole('button', { name: 'Swap sides' }).first().click();
  await expect.poll(async () => (await left('nav.rail')) < (await left('aside'))).toBe(true);
});

test('the Files panel lists a folder lazily, hides hidden entries unless asked, and offers read-only actions', async ({ page }) => {
  await page.addInitScript(() => { try { if (!sessionStorage.getItem('files-first')) { sessionStorage.setItem('files-first', '1'); localStorage.setItem('aw.arrangement', JSON.stringify({ side: 'left', panel: 'files' })); } } catch { /* storage unavailable */ } });
  await page.goto('/');
  await page.evaluate(sessions => { globalThis.__fakeTerminals.sessions = sessions; }, fakeSessions(1));
  const panel = page.getByRole('complementary', { name: 'Files panel' });
  const tree = panel.getByRole('tree');
  await expect(panel).toContainText('C:\\Users\\me');
  await expect(tree.getByRole('treeitem')).toHaveCount(2);
  await expect(tree).toContainText('notes.txt');
  await expect(tree).not.toContainText('.secret');
  // A folder opens on click (read then), and closes again.
  await tree.getByRole('button', { name: /project/ }).click();
  await expect(tree).toContainText('README.md');
  await expect(tree).toContainText('2.0 KB');
  await tree.getByRole('button', { name: 'src', exact: true }).click();
  await expect(tree).toContainText('Empty');
  await tree.getByRole('button', { name: /project/ }).click();
  await expect(tree).not.toContainText('README.md');
  // Hidden entries on request.
  const hiddenBtn = panel.getByRole('button', { name: 'Show hidden files' });
  await expect(hiddenBtn).toHaveAttribute('aria-pressed', 'false');
  // A small icon button in the header row, the same size as the others.
  const sizes = await panel.locator('.tools button').evaluateAll(els => els.map(e => Math.round(e.getBoundingClientRect().width)));
  expect(new Set(sizes).size).toBe(1);
  await hiddenBtn.click();
  await expect(hiddenBtn).toHaveAttribute('aria-pressed', 'true');
  await expect(tree).toContainText('.secret');
  // Pin to top: the pin stays listed above the tree whatever folder is shown, and clicking a pinned folder shows it.
  await tree.getByRole('button', { name: /project/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Pin to top' }).click();
  const pinned = panel.getByRole('list', { name: 'Pinned files and folders' });
  await expect(pinned).toContainText('project');
  await tree.getByRole('button', { name: /notes\.txt/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Pin to top' }).click();
  await expect(pinned.getByRole('listitem')).toHaveCount(2);
  // A pinned folder opens in place under its pin (it must not just flicker), and closes again; the folder shown below stays.
  const pinBtn = pinned.getByRole('button', { name: /project/ });
  await pinBtn.click();
  await expect(pinBtn).toHaveAttribute('aria-expanded', 'true');
  await expect(pinned).toContainText('README.md');
  await page.waitForTimeout(300);
  await expect(pinned).toContainText('README.md');
  await expect(panel.locator('.root')).toHaveText('C:\\Users\\me');
  // Its subfolders open and close too.
  const sub = pinned.getByRole('button', { name: 'src', exact: true });
  const before = await sub.locator('xpath=..').getAttribute('aria-expanded');
  await sub.click();
  await expect(sub.locator('xpath=..')).toHaveAttribute('aria-expanded', before === 'true' ? 'false' : 'true');
  await pinBtn.click();
  await expect(pinBtn).toHaveAttribute('aria-expanded', 'false');
  await expect(pinned).not.toContainText('README.md');
  // "Show this folder" makes it the folder shown below.
  await pinBtn.click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Show this folder' }).click();
  await expect(panel.locator('.root')).toHaveText('C:\\Users\\me\\project');
  await expect(tree).toContainText('README.md');
  await page.reload();
  await expect(page.getByRole('list', { name: 'Pinned files and folders' }).getByRole('listitem')).toHaveCount(2);
  await page.getByRole('list', { name: 'Pinned files and folders' }).getByRole('button', { name: /notes\.txt/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Unpin' }).click();
  await expect(page.getByRole('list', { name: 'Pinned files and folders' }).getByRole('listitem')).toHaveCount(1);
  await page.getByRole('button', { name: 'Up one folder' }).click();
  await expect(panel.locator('.root')).toHaveText('C:\\Users\\me');
  // Reveal in File Explorer.
  await tree.getByRole('button', { name: /notes\.txt/ }).click({ button: 'right' });
  const menu = page.getByRole('menu', { name: 'Actions for notes.txt' });
  await expect(menu.getByRole('menuitem', { name: 'Open terminal here' })).toBeDisabled();
  await menu.getByRole('menuitem', { name: 'Reveal in File Explorer' }).click();
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.revealed)).toEqual(['C:\\Users\\me\\notes.txt']);
  // Insert path needs a terminal that was used; it types the quoted path and never Enter.
  await tree.getByRole('button', { name: /project/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Insert path' }).click();
  await expect(panel).toContainText('Click in a terminal first');
  // Open terminal here: the Terminals tab opens and a terminal starts in that folder.
  await tree.getByRole('button', { name: /project/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Open terminal here' }).click();
  await expect(page.locator('section.terminals')).toBeVisible();
  await expect.poll(() => page.evaluate(() => globalThis.__fakeTerminals.newFolders)).toEqual(['C:\\Users\\me\\project']);
  await expect(page.locator('section.terminals')).toContainText('New terminal started in C:\\Users\\me\\project');
  // Now a terminal has been used: Insert path types into it.
  await page.locator('.panes .pane').first().click();
  await tree.getByRole('button', { name: /project/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Insert path' }).click();
  await expect.poll(() => page.evaluate(() => (globalThis.__fakeTerminals.inputs ?? []).map(i => i.text))).toContain('"C:\\Users\\me\\project"');
  expect(await page.evaluate(() => (globalThis.__fakeTerminals.inputs ?? []).some(i => /[\r\n]/.test(i.text) && i.text.includes('project')))).toBe(false);
  // Launch agent here fills the Launch agent row's folder.
  await tree.getByRole('button', { name: /project/ }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Launch agent here' }).click();
  await expect(page.locator('section.terminals').getByPlaceholder('C:\\Users\\you\\project')).toHaveValue('C:\\Users\\me\\project');
  // The folder and what was open are remembered.
  await page.reload();
  await expect(page.getByRole('complementary', { name: 'Files panel' })).toContainText('.secret');
});

test('Inspector: the bar under the Map sets its height, the map scales with it, and the height is remembered', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 1000 });
  await openTerminals(page, 0);
  await rail(page, 'Inspector').click();
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  const svg = page.locator('.ins-map svg');
  await expect(svg).toBeVisible();
  const bar = page.getByRole('separator', { name: /Map height/ });
  const area = () => page.locator('.ins-map').boundingBox();
  const drawn = () => svg.boundingBox();
  const before = { area: await area(), drawn: await drawn() };
  // Drag the bar down: the map area grows and the drawing grows with it.
  const b = await bar.boundingBox();
  await page.mouse.move(b.x + 200, b.y + b.height / 2);
  await page.mouse.down();
  await page.mouse.move(b.x + 200, b.y + 260, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => (await area()).height).toBeGreaterThan(before.area.height + 200);
  await expect.poll(async () => (await drawn()).height).toBeGreaterThan(before.drawn.height * 1.3);
  // The keyboard moves it too, and a double-click puts it back to the default.
  await bar.focus();
  const tall = (await area()).height;
  await page.keyboard.press('ArrowUp');
  await expect.poll(async () => (await area()).height).toBeLessThan(tall);
  await page.reload();
  await rail(page, 'Inspector').click();
  await page.getByRole('tab', { name: 'Map', exact: true }).click();
  await expect.poll(async () => Math.abs((await area()).height - (tall - 24)) < 2, { message: 'the height survives a reload' }).toBe(true);
  await bar.dblclick();
  await expect.poll(async () => Math.abs((await area()).height - before.area.height) < 2).toBe(true);
});

test('the window goes gray while Workbench does not have the keyboard, and comes back when it does', async ({ page }) => {
  await page.goto('/');
  const root = page.locator('.workbench');
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect(root).not.toHaveClass(/unfocused/);
  expect(await root.evaluate(el => getComputedStyle(el).filter)).toBe('none');
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  await expect(root).toHaveClass(/unfocused/);
  await expect.poll(() => root.evaluate(el => getComputedStyle(el).filter)).toContain('grayscale(1)');
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect(root).not.toHaveClass(/unfocused/);
});

test('see-through keeps its clickable areas current even when frame callbacks are held back', async ({ page }) => {
  await openTerminals(page, 1);
  await page.locator('.terminal-list li').first().locator('button.float-out').click();
  await rail(page, 'Canvas').click();
  await expect(wins(page)).toHaveCount(1);
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toBeVisible();
  const last = async () => { const r = await page.evaluate(() => globalThis.__fakeTerminals.ghost.reports); return r[r.length - 1]; };
  const inside = (rects, x, y) => rects.some(([rx, ry, rw, rh]) => x >= rx && x < rx + rw && y >= ry && y < ry + rh);
  // A window that is hidden while it starts holds frame callbacks back; here they never run at all.
  await page.evaluate(() => { window.requestAnimationFrame = () => 1; window.cancelAnimationFrame = () => undefined; });
  const grip = wins(page).first().locator('.float-grip');
  await grip.focus();
  for (let i = 0; i < 12; i++) await grip.press('ArrowDown');
  for (let i = 0; i < 20; i++) await grip.press('ArrowRight');
  const moved = await wins(page).first().boundingBox();
  // The far corner of the moved window is only inside the reported areas if a report went out after the move.
  await expect.poll(async () => inside((await last()).rects, moved.x + moved.width - 4, moved.y + moved.height - 4), { timeout: 3000 }).toBe(true);
  await page.keyboard.press('Control+Alt+KeyG');
});
