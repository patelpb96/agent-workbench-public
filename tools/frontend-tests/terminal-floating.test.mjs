// Pure tests of the free-form terminal windows and session links on the Agents canvas.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const f = require(`${root}artifacts/frontend-contract-src/terminals/floating.js`);

const T = n => (0x100 + n).toString(16).padStart(8, '0');
const S = n => `00000000-0000-4000-8000-${n.toString().padStart(12, '0')}`;
const CANVAS = { w: 1200, h: 700 };

test('floating a terminal opens a cascaded window on top; floating it again only raises it', () => {
  let s = f.defaultState();
  s = f.floatTerminal(s, T(1), CANVAS);
  s = f.floatTerminal(s, T(2), CANVAS);
  assert.equal(s.windows.length, 2);
  const [a, b] = s.windows;
  assert.ok(b.x > a.x && b.y > a.y && b.z > a.z, 'cascades and stacks');
  assert.equal(s.active, T(2));
  const again = f.floatTerminal(s, T(1), CANVAS);
  assert.equal(again.windows.length, 2, 'no duplicate window');
  assert.equal(f.stacked(again).at(-1).terminal, T(1), 'raised to the top');
  assert.equal(again.active, T(1));
  assert.equal(f.floatTerminal(s, 'not-an-id', CANVAS), s);
});

test('there is a window cap, and docking closes a window without touching others', () => {
  let s = f.defaultState();
  for (let i = 0; i < f.MAX_WINDOWS + 5; i++) s = f.floatTerminal(s, T(i), CANVAS);
  assert.equal(s.windows.length, f.MAX_WINDOWS);
  const docked = f.dock(s, T(3));
  assert.equal(docked.windows.length, f.MAX_WINDOWS - 1);
  assert.equal(f.isFloating(docked, T(3)), false);
  assert.equal(f.dock(docked, T(3)), docked, 'docking a non-floating terminal is a no-op');
});

test('windows can be placed anywhere (a star of five) and are clamped so the title bar stays reachable', () => {
  let s = f.defaultState();
  const spots = [[420, 220], [40, 40], [820, 40], [40, 420], [820, 420]]; // centre plus four corners
  spots.forEach(([x, y], i) => {
    s = f.floatTerminal(s, T(i), CANVAS);
    s = f.resizeWindow(s, T(i), 300, 200, CANVAS);
    s = f.moveWindow(s, T(i), x, y, CANVAS);
  });
  assert.deepEqual(s.windows.map(w => [w.x, w.y]), spots, 'positions are free-form, not snapped to a grid');
  assert.ok(s.windows.every(w => w.w === 300 && w.h === 200));
  // Way off the canvas: pulled back so at least KEEP_VISIBLE px of the title bar remain.
  const far = f.moveWindow(s, T(0), 99999, -500, CANVAS);
  const w = f.windowOf(far, T(0));
  assert.equal(w.x, CANVAS.w - f.KEEP_VISIBLE);
  assert.equal(w.y, 0);
  const left = f.moveWindow(s, T(0), -9999, 9999, CANVAS);
  assert.equal(f.windowOf(left, T(0)).x, f.KEEP_VISIBLE - 300);
  assert.equal(f.windowOf(left, T(0)).y, CANVAS.h - 40);
  // Sizes: never below the minimum, never larger than the canvas.
  const tiny = f.resizeWindow(s, T(1), 5, 5, CANVAS);
  assert.deepEqual([f.windowOf(tiny, T(1)).w, f.windowOf(tiny, T(1)).h], [f.MIN_W, f.MIN_H]);
  const huge = f.resizeWindow(s, T(1), 9999, 9999, CANVAS);
  assert.deepEqual([f.windowOf(huge, T(1)).w, f.windowOf(huge, T(1)).h], [CANVAS.w, CANVAS.h]);
  assert.equal(f.moveWindow(s, 'deadbeef', 1, 1, CANVAS), s, 'unknown terminal is ignored');
});

test('a tiny canvas still gets a usable window and non-finite input is repaired', () => {
  const w = f.clampWin({ terminal: T(1), x: NaN, y: Infinity, w: NaN, h: -Infinity, z: 1 }, { w: 100, h: 100 });
  assert.ok(Number.isFinite(w.x) && Number.isFinite(w.y));
  assert.equal(w.w, f.MIN_W);
  assert.equal(w.h, f.MIN_H);
});

test('raising reorders the stack and marks the window active', () => {
  let s = f.defaultState();
  for (const n of [1, 2, 3]) s = f.floatTerminal(s, T(n), CANVAS);
  assert.deepEqual(f.stacked(s).map(w => w.terminal), [T(1), T(2), T(3)]);
  s = f.raise(s, T(1));
  assert.deepEqual(f.stacked(s).map(w => w.terminal), [T(2), T(3), T(1)]);
  assert.equal(s.active, T(1));
  assert.equal(f.raise(s, T(1)), s, 'already on top');
  assert.equal(f.raise(s, T(99)), s);
});

test('a session links to one terminal and a terminal to one session; flipping shows the terminal in the panel', () => {
  let s = f.link(f.defaultState(), S(1), T(1));
  assert.deepEqual(f.linkOf(s, S(1)), { terminal: T(1), flipped: false });
  s = f.link(s, S(2), T(1));
  assert.equal(f.linkOf(s, S(1)), undefined, 'a terminal belongs to one session: the earlier link moves');
  assert.equal(f.linkOf(s, S(2)).terminal, T(1));
  s = f.link(s, S(2), T(2));
  assert.equal(f.linkOf(s, S(2)).terminal, T(2), 'relinking replaces');
  assert.equal(f.link(s, 'nope', T(3)), s);
  assert.equal(f.link(s, S(3), 'nope'), s);
  assert.equal(f.flip(s, S(9), true), s, 'no link, nothing to flip');
  s = f.flip(s, S(2), true);
  assert.equal(f.linkOf(s, S(2)).flipped, true);
  assert.deepEqual([...f.occupied(s)], [T(2)]);
  s = f.flip(s, S(2), false);
  assert.equal(f.occupied(s).size, 0);
  assert.deepEqual(f.unlink(s, S(2)).links, {});
  assert.equal(f.unlink(s, S(7)), s);
});

test('a terminal is shown in one place: floating and flipping take it from each other, and release clears both', () => {
  let s = f.link(f.defaultState(), S(1), T(1));
  s = f.flip(s, S(1), true);
  s = f.floatTerminal(s, T(1), CANVAS);
  assert.equal(f.linkOf(s, S(1)).flipped, false, 'floating takes it out of the panel');
  assert.equal(f.isFloating(s, T(1)), true);
  s = f.flip(s, S(1), true);
  assert.equal(f.isFloating(s, T(1)), false, 'flipping takes it out of the window');
  assert.equal(f.linkOf(s, S(1)).flipped, true);
  const released = f.release(s, T(1));
  assert.equal(f.occupied(released).size, 0);
  assert.equal(f.linkOf(released, S(1)).terminal, T(1), 'the link itself stays');
  assert.equal(f.release(released, T(1)), released);
  // The link cap holds.
  let many = f.defaultState();
  for (let i = 1; i <= f.MAX_LINKS + 3; i++) many = f.link(many, S(i), T(i));
  assert.equal(Object.keys(many.links).length, f.MAX_LINKS);
});

test('terminals and sessions that no longer exist are dropped', () => {
  let s = f.link(f.floatTerminal(f.floatTerminal(f.defaultState(), T(1), CANVAS), T(2), CANVAS), S(1), T(3));
  s = f.link(s, S(2), T(4));
  const r = f.reconcile(s, new Set([T(2), T(3)]));
  assert.deepEqual(r.windows.map(w => w.terminal), [T(2)]);
  assert.deepEqual(Object.keys(r.links), [S(1)]);
  assert.equal(f.reconcile(r, new Set([T(2), T(3)])), r, 'no change returns the same object');
  const withSessions = f.reconcile(s, new Set([T(1), T(2), T(3), T(4)]), new Set([S(1)]));
  assert.deepEqual(Object.keys(withSessions.links), [S(1)]);
  assert.equal(f.reconcile({ ...s, active: T(9) }, new Set([T(1), T(2), T(3), T(4)])).active, null);
});

test('stored state is validated and repaired, never trusted', () => {
  let s = f.floatTerminal(f.floatTerminal(f.defaultState(), T(1), CANVAS), T(2), CANVAS);
  s = f.flip(f.link(s, S(1), T(5)), S(1), true);
  assert.deepEqual(f.parse(JSON.parse(JSON.stringify(s))), s, 'round trip');
  assert.deepEqual(f.parse(null), f.defaultState());
  assert.deepEqual(f.parse({ version: 3, windows: [] }), f.defaultState());
  assert.deepEqual(f.parse({ version: 1, windows: [], links: {} }), f.defaultState(), 'version 1 (no watches) is still read');
  const win = (terminal, extra = {}) => ({ terminal, x: 10, y: 10, w: 300, h: 200, z: 1, ...extra });
  const messy = f.parse({
    version: 1, active: 'nope',
    windows: [win(T(1)), win(T(1)), win('bad'), win(T(2), { w: 0 }), win(T(3), { x: Infinity }), win(T(4), { z: 50 }), 7, null, win(T(5), { z: 3, x: 1e9 })],
    links: { [S(1)]: { terminal: T(4), flipped: true }, [S(2)]: { terminal: T(4), flipped: false }, nope: { terminal: T(6), flipped: false }, [S(3)]: { terminal: T(7) } }
  });
  assert.deepEqual(messy.windows.map(w => w.terminal), [T(1), T(4)], 'duplicates, bad ids and absurd or non-finite numbers dropped; ordered by z');
  assert.deepEqual(messy.windows.map(w => w.z), [1, 2], 'stacking renumbered densely');
  assert.equal(messy.active, null);
  assert.deepEqual(messy.links, { [S(1)]: { terminal: T(4), flipped: false } }, 'one session per terminal; floating wins over flipped');
  assert.equal(f.parse({ version: 1, windows: Array.from({ length: 80 }, (_, i) => win(T(i))), links: {} }).windows.length, f.MAX_WINDOWS);
});

const SRC = n => (0x1000 + n).toString(16).padStart(16, '0');

test('watch windows show an existing agent session; they cascade, stack with terminal windows and are capped', () => {
  let s = f.floatTerminal(f.defaultState(), T(1), CANVAS);
  s = f.floatWatch(s, SRC(1), 'pi', 'my-project', CANVAS);
  assert.equal(s.watches.length, 1);
  assert.deepEqual([s.watches[0].agent, s.watches[0].label], ['pi', 'my-project']);
  assert.ok(s.watches[0].z > s.windows[0].z, 'one stacking order across both kinds');
  assert.equal(f.isWatching(s, SRC(1)), true);
  const again = f.floatWatch(s, SRC(1), 'pi', 'x', CANVAS);
  assert.equal(again.watches.length, 1, 'no duplicate watch of one session');
  assert.equal(f.floatWatch(s, 'nope', 'pi', 'x', CANVAS), s, 'an id that is not an opaque source id is refused');
  assert.equal(f.floatWatch(s, SRC(2), 'bash', 'x', CANVAS), s, 'unknown agent');
  const noisy = 'a' + String.fromCharCode(7) + 'b' + String.fromCharCode(10);
  assert.equal(f.watchOf(f.floatWatch(s, SRC(3), 'codex', noisy, CANVAS), SRC(3)).label, 'ab', 'control characters are stripped from labels');
  let many = f.defaultState();
  for (let i = 0; i < f.MAX_WATCHES + 4; i++) many = f.floatWatch(many, SRC(i), 'claude', `p${i}`, CANVAS);
  assert.equal(many.watches.length, f.MAX_WATCHES);
  assert.equal(f.closeWatch(many, SRC(0)).watches.length, f.MAX_WATCHES - 1);
  assert.equal(f.closeWatch(many, SRC(900)), many);
});

test('watch windows move, resize and raise through the same entry points as terminal windows', () => {
  let s = f.floatTerminal(f.defaultState(), T(1), CANVAS);
  const terminalBefore = f.windowOf(s, T(1));
  s = f.floatWatch(s, SRC(1), 'claude', 'p', CANVAS);
  assert.equal(f.itemOf(s, T(1)).kind, 'terminal');
  assert.equal(f.itemOf(s, SRC(1)).kind, 'watch');
  assert.equal(f.itemOf(s, SRC(9)), undefined);
  s = f.moveItem(s, SRC(1), 300, 120, CANVAS);
  s = f.resizeItem(s, SRC(1), 400, 260, CANVAS);
  const w = f.watchOf(s, SRC(1));
  assert.deepEqual([w.x, w.y, w.w, w.h], [300, 120, 400, 260]);
  assert.deepEqual(f.windowOf(s, T(1)), terminalBefore, 'the terminal window is untouched');
  const far = f.moveItem(s, SRC(1), 99999, -50, CANVAS);
  assert.equal(f.watchOf(far, SRC(1)).x, CANVAS.w - f.KEEP_VISIBLE, 'a watch window also stays reachable');
  const raised = f.raiseItem(f.raiseItem(s, T(1)), SRC(1));
  assert.ok(f.watchOf(raised, SRC(1)).z > f.windowOf(raised, T(1)).z);
  assert.equal(f.raiseItem(raised, SRC(1)), raised, 'already on top');
});

test('watches are validated when stored, round-trip, and share one dense stacking order', () => {
  const s = f.floatWatch(f.floatTerminal(f.defaultState(), T(1), CANVAS), SRC(1), 'pi', 'proj', CANVAS);
  assert.deepEqual(f.parse(JSON.parse(JSON.stringify(s))), s, 'round trip');
  const good = { source: SRC(5), agent: 'codex', label: 'x', x: 1, y: 2, w: 300, h: 200, z: 7 };
  const messy = f.parse({ version: 2, windows: [{ terminal: T(1), x: 0, y: 0, w: 300, h: 200, z: 9 }], links: {}, active: null,
    watches: [good, good, { ...good, source: 'nope' }, { ...good, source: SRC(6), agent: 'bash' }, { ...good, source: SRC(7), w: 0 }, { ...good, source: SRC(8), x: Infinity }, { ...good, source: SRC(9), label: 5 }, null, 4] });
  assert.deepEqual(messy.watches.map(w => w.source), [SRC(5)]);
  assert.deepEqual([messy.watches[0].z, messy.windows[0].z], [1, 2], 'the watch (z 7) is below the terminal window (z 9), renumbered densely');
  assert.deepEqual(f.parse({ version: 1, windows: [], links: {}, watches: [good] }).watches, [], 'version 1 has no watches');
  assert.equal(f.parse({ version: 2, windows: [], links: {}, watches: Array.from({ length: 40 }, (_, i) => ({ ...good, source: SRC(i) })) }).watches.length, f.MAX_WATCHES);
});

test('a watched session that is opened as a terminal turns the watch window into the terminal window', () => {
  const src = 'a1b2c3d4e5f60718';
  let st = f.floatWatch(f.defaultState(), src, 'pi', 'proj', CANVAS);
  st = f.moveWindow ? st : st;
  const before = st.watches[0];
  const next = f.openFromWatch(st, src, T(1), CANVAS);
  assert.equal(next.watches.length, 0, 'the watch window is gone');
  assert.equal(next.windows.length, 1);
  const win = next.windows[0];
  assert.deepEqual({ x: win.x, y: win.y, w: win.w, h: win.h, z: win.z }, { x: before.x, y: before.y, w: before.w, h: before.h, z: before.z }, 'same place, size and stacking');
  assert.equal(next.active, T(1));
  // Without a watch window it opens like any floating terminal.
  const plain = f.openFromWatch(f.defaultState(), src, T(2), CANVAS);
  assert.deepEqual(plain.windows.map(w => w.terminal), [T(2)]);
  // Already floating: just raised, and the watch still goes away.
  const both = f.openFromWatch(f.floatWatch(plain, src, 'pi', 'proj', CANVAS), src, T(2), CANVAS);
  assert.equal(both.windows.length, 1);
  assert.equal(both.watches.length, 0);
  // Bad ids change nothing; the window cap refuses.
  assert.equal(f.openFromWatch(st, 'nope', T(1), CANVAS), st);
  assert.equal(f.openFromWatch(st, src, 'zz', CANVAS), st);
  let full = f.floatWatch(f.defaultState(), src, 'pi', 'proj', CANVAS);
  for (let i = 0; i < f.MAX_WINDOWS; i++) full = f.floatTerminal(full, T(10 + i), CANVAS);
  assert.equal(f.openFromWatch(full, src, T(99), CANVAS), full);
});

test('a window minimizes like a session panel: bar, block, then full again at the size it had', () => {
  let s = f.resizeWindow(f.floatTerminal(f.defaultState(), T(1), CANVAS), T(1), 700, 420, CANVAS);
  s = f.cycleWindow(s, T(1), CANVAS);
  assert.equal(s.windows[0].stage, 'compact');
  assert.deepEqual(f.shownBox(s.windows[0], CANVAS), { x: s.windows[0].x, y: s.windows[0].y, w: f.BAR_W, h: f.BAR_H });
  assert.equal(f.resizeWindow(s, T(1), 300, 200, CANVAS), s, 'a minimized window is not resized');
  s = f.cycleWindow(s, T(1), CANVAS);
  assert.equal(s.windows[0].stage, 'block');
  assert.equal(f.shownBox(s.windows[0], CANVAS).w, f.BLOCK_W);
  // A block can sit in the bottom-right corner, wholly inside the canvas.
  s = f.moveWindow(s, T(1), 5000, 5000, CANVAS);
  assert.deepEqual(f.shownBox(s.windows[0], CANVAS), { x: CANVAS.w - f.BLOCK_W, y: CANVAS.h - f.BLOCK_H, w: f.BLOCK_W, h: f.BLOCK_H });
  s = f.cycleWindow(s, T(1), CANVAS);
  assert.equal('stage' in s.windows[0], false, 'full size leaves the stage out');
  assert.equal(s.windows[0].w, 700);
  assert.equal(s.windows[0].h, 420);
});

test('floating a minimized window again restores it; the stage survives a save and bad stages are dropped', () => {
  let s = f.cycleWindow(f.floatTerminal(f.defaultState(), T(1), CANVAS), T(1), CANVAS);
  assert.equal(f.parse(JSON.parse(JSON.stringify(s))).windows[0].stage, 'compact');
  assert.equal(f.floatTerminal(s, T(1), CANVAS).windows[0].stage, undefined);
  const bad = JSON.parse(JSON.stringify(s));
  bad.windows[0].stage = 'tiny';
  assert.equal(f.parse(bad).windows.length, 0);
});
