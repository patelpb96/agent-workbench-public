// tmux-style keyboard control in a fresh headless browser against the in-page fake terminal service
// (docs/design/tmux-parity-plan.md, wave 2): real xterm emulators, real DOM and Svelte, no desktop app or real
// terminal host. Covers scrollback search, the prefix key, copy mode, typing into all panes and output logging.
import { test, expect } from './fixtures.mjs';

const hex = n => (0x1000 + n).toString(16).padStart(8, '0');
const fakeSessions = count => Array.from({ length: count }, (_, i) => ({
  id: hex(i), label: `T${i + 1}`, state: 'running', persistent: true, cols: 100, rows: 30, host_pid: 1000 + i
}));

async function openTerminals(page, count, { prefix = 'ctrl+b', output = {} } = {}) {
  await page.goto('/');
  await page.evaluate(({ sessions, prefix }) => {
    const fake = globalThis.__fakeTerminals;
    fake.sessions = sessions;
    fake.settings = { ...fake.settings, keys: { prefix } };
  }, { sessions: fakeSessions(count), prefix });
  await page.reload();
  await page.evaluate(output => { Object.assign(globalThis.__fakeTerminals.stream, output); }, output);
  await page.getByRole('button', { name: /Terminals/ }).click();
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(count);
}
async function showFirst(page, text) {
  await page.locator('.terminal-list li').nth(0).locator('button.pick').click();
  if (text) await expect(page.locator('.pane .xterm-rows').first()).toContainText(text);
  await page.locator('.pane .xterm').first().click();
}
const inputs = page => page.evaluate(() => globalThis.__fakeTerminals.inputs ?? []);
const clearInputs = page => page.evaluate(() => { globalThis.__fakeTerminals.inputs = []; });
async function prefix(page, key) {
  await page.keyboard.press('Control+b');
  await expect(page.locator('.key-hint')).toContainText('waiting for a command key');
  await page.keyboard.press(key);
}
function noPageErrors(page) {
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  return errors;
}

test('scrollback search highlights every match and counts them in a real emulator', async ({ page }) => {
  const errors = noPageErrors(page);
  await openTerminals(page, 1, { output: { [hex(0)]: 'alpha one\r\nbeta\r\nalpha two\r\nALPHA three\r\n' } });
  await showFirst(page, 'ALPHA three');
  await prefix(page, '/');
  const field = page.getByRole('textbox', { name: 'Search text' });
  await expect(field).toBeFocused();
  await field.fill('alpha');
  // Decorations are what count the matches: this fails if the emulator refuses them (allowProposedApi).
  await expect(page.locator('.search-result')).toHaveText(/^\d of 3$/);
  await expect.poll(() => page.locator('.xterm-decoration').count()).toBeGreaterThan(0);
  await page.getByRole('button', { name: 'Match case' }).click();
  await expect(page.locator('.search-result')).toHaveText(/^\d of 2$/);
  await field.press('Enter');
  await field.press('Escape');
  await expect(field).toHaveCount(0);
  await expect.poll(() => page.locator('.xterm-decoration').count()).toBe(0);
  expect(await inputs(page), 'search keys never reach the shell').toEqual([]);
  expect(errors).toEqual([]);
});

test('the prefix splits with a new shell, zooms, cycles layouts, lists its keys, and Ctrl+B twice reaches the terminal', async ({ page }) => {
  const errors = noPageErrors(page);
  await openTerminals(page, 1, { output: { [hex(0)]: 'ready\r\n' } });
  await showFirst(page, 'ready');
  await clearInputs(page);

  await prefix(page, '%');
  await expect(page.locator('.panes .pane')).toHaveCount(2);
  await expect(page.locator('.terminal-list li:not(.empty)')).toHaveCount(2, { timeout: 5000 });
  await expect(page.locator('.pane-head').nth(1)).toContainText('Terminal 2');
  const boxes = await page.locator('.panes .pane').evaluateAll(els => els.map(e => e.getBoundingClientRect().x));
  expect(boxes[0]).toBeLessThan(boxes[1]);
  // The new pane's terminal has the keyboard now.
  await expect(page.locator('.pane').nth(1).locator('.xterm-helper-textarea')).toBeFocused();

  await prefix(page, 'z');
  await expect(page.locator('.panes .pane')).toHaveCount(1);
  await expect(page.locator('.pane-count')).toHaveText('2 panes · zoomed');
  await prefix(page, 'z');
  await expect(page.locator('.panes .pane')).toHaveCount(2);

  await prefix(page, ' ');
  await expect(page.locator('.terminal-note').filter({ hasText: 'Layout: Side by side' })).toBeVisible();
  await prefix(page, ' ');
  await expect(page.locator('.terminal-note').filter({ hasText: 'Layout: Stacked' })).toBeVisible();
  const stacked = await page.locator('.panes .pane').evaluateAll(els => els.map(e => e.getBoundingClientRect().y));
  expect(stacked[0]).toBeLessThan(stacked[1]);

  await prefix(page, '?');
  const help = page.getByRole('dialog', { name: 'Terminal keyboard commands' });
  await expect(help).toBeVisible();
  await expect(help).toContainText('Split the pane to the right');
  await page.keyboard.press('Escape');
  await expect(help).toHaveCount(0);
  // Closing the list gives the keyboard back: the prefix works at once, with no click in between.
  await prefix(page, 'Escape');
  // Closed with the mouse too.
  await prefix(page, '?');
  await help.getByRole('button', { name: 'Close the list' }).click();
  await expect(help).toHaveCount(0);
  await prefix(page, 'Escape');
  // And if focus does end up nowhere in the page, the prefix still goes to the last terminal used.
  await page.evaluate(() => (document.activeElement instanceof HTMLElement) && document.activeElement.blur());
  await prefix(page, 'Escape');

  await page.locator('.pane .xterm').first().click();
  await clearInputs(page);
  await page.keyboard.press('Control+b');
  await page.keyboard.press('Control+b');
  await expect.poll(() => inputs(page)).toEqual([{ session: expect.any(String), text: '\u0002' }]);
  await prefix(page, 'q');
  await expect(page.locator('.key-hint')).toContainText('does nothing');
  expect((await inputs(page)).map(i => i.text), 'the command key is not typed').toEqual(['\u0002']);
  // Ordinary keys go straight to the terminal.
  await page.keyboard.type('ls');
  await expect.poll(async () => (await inputs(page)).map(i => i.text).join('')).toBe('\u0002ls');
  expect(errors).toEqual([]);
});

test('copy mode moves through the scrollback, selects and copies, and types nothing into the shell', async ({ page }) => {
  const errors = noPageErrors(page);
  await openTerminals(page, 1, { output: { [hex(0)]: 'first line\r\nsecond line\r\nthird line\r\n' } });
  await page.evaluate(() => {
    globalThis.__copied = [];
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: text => { globalThis.__copied.push(text); return Promise.resolve(); } } });
  });
  await showFirst(page, 'third line');
  await clearInputs(page);
  await prefix(page, '[');
  const status = page.locator('.copy-line');
  await expect(status).toContainText('COPY');
  for (const key of ['g', 'v', 'j', '$']) await page.keyboard.press(key);
  await expect(status).toContainText('selecting');
  await expect(status).toContainText('line 2 of');
  await page.keyboard.press('y');
  await expect(status).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => globalThis.__copied)).toEqual([expect.stringMatching(/^first line\r?\nsecond line/)]);
  // q leaves without copying.
  await prefix(page, '[');
  await expect(status).toContainText('COPY');
  await page.keyboard.press('k');
  await page.keyboard.press('q');
  await expect(status).toHaveCount(0);
  expect(await inputs(page), 'no copy-mode key reached the shell').toEqual([]);
  expect(errors).toEqual([]);
});

test('typing into all panes reaches every shown terminal until it is turned off', async ({ page }) => {
  await openTerminals(page, 2, { output: { [hex(0)]: 'one\r\n', [hex(1)]: 'two\r\n' } });
  await page.getByRole('button', { name: /^Tile all/ }).click();
  await expect(page.locator('.panes .pane')).toHaveCount(2);
  await page.locator('.pane .xterm').first().click();
  await prefix(page, 'S');
  await expect(page.locator('.pane.synced')).toHaveCount(2);
  await expect(page.locator('.pane-head .mode.sync')).toHaveCount(2);
  await clearInputs(page);
  await page.keyboard.type('pwd');
  await expect.poll(async () => {
    const all = await inputs(page);
    return [hex(0), hex(1)].map(id => all.filter(i => i.session === id).map(i => i.text).join(''));
  }).toEqual(['pwd', 'pwd']);
  // Zoomed, the hidden pane gets nothing: only what is on screen is typed into.
  await prefix(page, 'z');
  await expect(page.locator('.panes .pane')).toHaveCount(1);
  await expect(page.locator('.pane-head .mode.sync')).toHaveText(' · TYPING HERE ONLY WHILE ZOOMED');
  await clearInputs(page);
  await page.keyboard.type('a');
  await expect.poll(() => inputs(page)).toEqual([{ session: hex(0), text: 'a' }]);
  await prefix(page, 'z');
  await expect(page.locator('.pane.synced')).toHaveCount(2);
  await prefix(page, 'S');
  await expect(page.locator('.pane.synced')).toHaveCount(0);
  await clearInputs(page);
  await page.keyboard.type('x');
  await expect.poll(() => inputs(page)).toEqual([{ session: hex(0), text: 'x' }]);
  // The prefix sent on goes through the same path: into every pane while typing into all of them.
  await prefix(page, 'S');
  await clearInputs(page);
  await page.keyboard.press('Control+b');
  await page.keyboard.press('Control+b');
  await expect.poll(async () => (await inputs(page)).map(i => i.session).sort()).toEqual([hex(0), hex(1)]);
});

test('in copy mode even the prefix sent on stays out of the shell', async ({ page }) => {
  await openTerminals(page, 1, { output: { [hex(0)]: 'ready\r\n' } });
  await showFirst(page, 'ready');
  await prefix(page, '[');
  await expect(page.locator('.copy-line')).toContainText('COPY');
  await clearInputs(page);
  await page.keyboard.press('Control+b');
  await page.keyboard.press('Control+b');
  await page.keyboard.press('q');
  await expect(page.locator('.copy-line')).toHaveCount(0);
  await page.waitForTimeout(200);
  expect(await inputs(page)).toEqual([]);
});

test('a log that stops by itself loses its REC mark and says why', async ({ page }) => {
  await openTerminals(page, 1, { output: { [hex(0)]: 'hello\r\n' } });
  await showFirst(page, 'hello');
  await prefix(page, 'L');
  await expect(page.locator('.pane-wrap .rec')).toBeVisible();
  // The service stops it at the size cap; the Terminals view's regular check notices.
  await page.evaluate(id => { const log = globalThis.__fakeTerminals.logs[id]; Object.assign(log, { on: false, bytes: 67108864, stopped_reason: 'the 64 MiB limit was reached' }); }, hex(0));
  await expect(page.locator('.pane-wrap .rec')).toHaveCount(0, { timeout: 8000 });
  await expect(page.locator('.terminal-note')).toContainText('Logging stopped by itself: the 64 MiB limit was reached');
});

test('logging a terminal shows REC wherever it is shown, and an older service says why it cannot', async ({ page }) => {
  await openTerminals(page, 1, { output: { [hex(0)]: 'hello\r\n' } });
  await showFirst(page, 'hello');
  await prefix(page, 'L');
  await expect(page.locator('.key-hint')).toContainText('Logging this terminal');
  await expect(page.locator('.pane-wrap .rec')).toBeVisible();
  await expect(page.locator('.terminal-list .badge.rec')).toBeVisible();
  await expect(page.getByRole('button', { name: /^Stop logging T1/ })).toHaveAttribute('aria-pressed', 'true');
  await page.locator('.pane .xterm').first().click();
  await prefix(page, 'L');
  await expect(page.locator('.key-hint')).toContainText('Logging stopped');
  await expect(page.locator('.pane-wrap .rec')).toHaveCount(0);
  expect(await page.evaluate(() => globalThis.__fakeTerminals.logCalls.filter(c => c[1] !== 'status'))).toEqual([[hex(0), 'start'], [hex(0), 'stop']]);
  // A service from before logging: the button says what happened instead of failing silently.
  await page.evaluate(() => { globalThis.__fakeTerminals.logOff = true; });
  await page.getByRole('button', { name: /^Log the output of T1/ }).click();
  await expect(page.locator('.terminal-note')).toContainText('older than logging');
});

test('with the prefix turned off, Ctrl+B is just a key for the terminal', async ({ page }) => {
  await openTerminals(page, 1, { prefix: 'off', output: { [hex(0)]: 'ready\r\n' } });
  await showFirst(page, 'ready');
  await clearInputs(page);
  await page.keyboard.press('Control+b');
  await page.keyboard.press('%');
  await expect.poll(async () => (await inputs(page)).map(i => i.text).join('')).toBe('\u0002%');
  await expect(page.locator('.key-hint')).toHaveCount(0);
  await expect(page.locator('.panes .pane')).toHaveCount(1);
  await expect(page.locator('.terminal-hint').first()).not.toContainText('prefix');
});

test('on the Canvas the prefix moves between windows by where they are: arrows, o, and a word when nothing is that way', async ({ page }) => {
  await openTerminals(page, 3, { output: { [hex(0)]: 'one\r\n', [hex(1)]: 'two\r\n', [hex(2)]: 'three\r\n' } });
  for (let i = 0; i < 3; i++) await page.locator('.terminal-list li').nth(i).locator('button.float-out').click();
  await page.locator('.rail button', { hasText: 'Canvas' }).click();
  const win = n => page.locator('.float-layer .float-win').filter({ hasText: `T${n}` });
  await expect(page.locator('.float-layer .float-win')).toHaveCount(3);
  const area = await page.locator('.float-layer').boundingBox();
  // T1 top-left, T2 top-right, T3 bottom-left; the last floated is on top, so it is moved first.
  const spots = { 3: [area.x + 20, area.y + area.height - 380], 2: [area.x + area.width - 620, area.y + 20], 1: [area.x + 20, area.y + 20] };
  for (const n of [3, 2, 1]) {
    const g = await win(n).locator('.float-grip').boundingBox();
    await page.mouse.move(g.x + 20, g.y + g.height / 2);
    await page.mouse.down();
    await page.mouse.move(spots[n][0] + 20, spots[n][1] + g.height / 2, { steps: 5 });
    await page.mouse.up();
  }
  const inWindow = () => page.evaluate(() => document.activeElement?.closest('.float-win')?.querySelector('.float-grip strong')?.textContent ?? null);
  await win(1).locator('.xterm').click();
  await expect.poll(inWindow).toBe('T1');
  await prefix(page, 'ArrowRight');
  await expect.poll(inWindow).toBe('T2');
  await expect.poll(() => page.evaluate(() => document.activeElement?.className ?? '')).toContain('xterm-helper-textarea');
  await prefix(page, 'ArrowDown');
  await expect.poll(inWindow).toBe('T3');
  await prefix(page, 'ArrowDown');
  await expect(page.locator('.key-hint')).toContainText('No window below');
  await expect.poll(inWindow).toBe('T3');
  // o cycles in reading order: T1, T2 (the top row), then T3.
  await prefix(page, 'o');
  await expect.poll(inWindow).toBe('T1');
  await prefix(page, 'o');
  await expect.poll(inWindow).toBe('T2');
  // Repeat (tmux -r): right after an arrow command, another arrow works without the prefix; after half a second it is
  // an ordinary key for the terminal again.
  await win(1).locator('.xterm').click();
  await expect.poll(inWindow).toBe('T1');
  await prefix(page, 'ArrowRight');
  await page.keyboard.press('ArrowDown');
  await expect.poll(inWindow).toBe('T3');
  // The prefix during the repeat starts a new prefix (and ends the repeat).
  await page.keyboard.press('Control+b');
  await expect(page.locator('.key-hint')).toContainText('waiting for a command key');
  await page.keyboard.press('Escape');
  await page.waitForTimeout(700);
  await clearInputs(page);
  await page.keyboard.press('ArrowUp');
  await expect.poll(async () => (await inputs(page)).length).toBe(1);
  await expect.poll(inWindow).toBe('T3');
  // Alt+arrow resizes by more (tmux: 5 cells), Ctrl+arrow by a little; the corner moves, the window stays put.
  const t3 = await win(3).boundingBox();
  await prefix(page, 'Alt+ArrowRight');
  await expect.poll(async () => (await win(3).boundingBox()).width).toBeCloseTo(t3.width + 80, 0);
  await page.keyboard.press('Control+ArrowRight');
  await expect.poll(async () => (await win(3).boundingBox()).width).toBeCloseTo(t3.width + 96, 0);
  expect((await win(3).boundingBox()).x).toBeCloseTo(t3.x, 0);
  await page.waitForTimeout(700);
  // 1..9 by the same order; past the last window it says how many there are.
  await prefix(page, '3');
  await expect.poll(inWindow).toBe('T3');
  await prefix(page, '1');
  await expect.poll(inWindow).toBe('T1');
  await prefix(page, '9');
  await expect(page.locator('.key-hint')).toContainText('There are 3 windows on the Canvas');
  // Splits belong to the Terminals view.
  await prefix(page, '%');
  await expect(page.locator('.key-hint')).toContainText('"split right" is a Terminals view command');
  await expect(page.locator('.float-layer .float-win')).toHaveCount(3);
  // A text field in a Canvas window keeps its keys: the search bar here is one (it is not the terminal).
  await prefix(page, '/');
  const field = win(1).getByRole('textbox', { name: 'Search text' });
  await expect(field).toBeFocused();
  await page.keyboard.press('Control+b');
  await expect(page.locator('.key-hint')).not.toContainText('waiting for a command key');
});

test('the Canvas prefix keeps working while the window is see-through (Ctrl+Alt+G)', async ({ page }) => {
  await openTerminals(page, 2, { output: { [hex(0)]: 'one\r\n', [hex(1)]: 'two\r\n' } });
  for (let i = 0; i < 2; i++) await page.locator('.terminal-list li').nth(i).locator('button.float-out').click();
  await page.locator('.rail button', { hasText: 'Canvas' }).click();
  const win = n => page.locator('.float-layer .float-win').filter({ hasText: `T${n}` });
  await expect(page.locator('.float-layer .float-win')).toHaveCount(2);
  const area = await page.locator('.float-layer').boundingBox();
  for (const [n, x] of [[2, area.x + area.width - 620], [1, area.x + 20]]) {
    const g = await win(n).locator('.float-grip').boundingBox();
    await page.mouse.move(g.x + 20, g.y + g.height / 2);
    await page.mouse.down();
    await page.mouse.move(x + 20, area.y + 20 + g.height / 2, { steps: 5 });
    await page.mouse.up();
  }
  await page.locator('body').click({ position: { x: 5, y: 5 } }).catch(() => undefined);
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toBeVisible();
  const inWindow = () => page.evaluate(() => document.activeElement?.closest('.float-win')?.querySelector('.float-grip strong')?.textContent ?? null);
  await win(1).locator('.xterm').click();
  await expect.poll(inWindow).toBe('T1');
  await prefix(page, 'ArrowRight');
  await expect.poll(inWindow).toBe('T2');
  await expect(page.locator('.key-hint', { hasText: 'Pane commands work in the Terminals view' })).toHaveCount(0);
  // Ctrl still held from the prefix (Ctrl+B, then Ctrl+Up) is tmux's resize: the window gets shorter, focus stays.
  const before = await win(2).boundingBox();
  await page.keyboard.down('Control');
  await page.keyboard.press('b');
  await page.keyboard.press('ArrowUp');
  await page.keyboard.up('Control');
  await expect.poll(async () => (await win(2).boundingBox()).height).toBeCloseTo(before.height - 16, 0);
  await expect.poll(inWindow).toBe('T2');
  await expect(page.locator('.key-hint', { hasText: 'Terminals view' })).toHaveCount(0);
  await page.keyboard.press('Control+Alt+KeyG');
  await expect(page.locator('.ghost-note')).toHaveCount(0);
});
