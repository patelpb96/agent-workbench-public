// Pure tests of tmux-style keyboard control: the prefix state machine, copy mode and the pane commands it drives.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const k = require(`${root}artifacts/frontend-contract-src/terminals/keymap.js`);
const c = require(`${root}artifacts/frontend-contract-src/terminals/copy-mode.js`);
const p = require(`${root}artifacts/frontend-contract-src/terminals/panes.js`);

const key = (key, mods = {}) => ({ key, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...mods });
const ctrl = (letter) => key(letter, { ctrlKey: true });

test('only exactly Ctrl+B (or the chosen prefix) is the prefix, and "off" takes nothing', () => {
  assert.equal(k.isPrefix(ctrl('b'), 'ctrl+b'), true);
  assert.equal(k.isPrefix(ctrl('B'), 'ctrl+b'), true, 'Caps Lock does not matter');
  for (const other of [key('b'), key('b', { ctrlKey: true, shiftKey: true }), key('b', { ctrlKey: true, altKey: true }), key('b', { ctrlKey: true, metaKey: true }), ctrl('a'), ctrl('c')]) {
    assert.equal(k.isPrefix(other, 'ctrl+b'), false, JSON.stringify(other));
  }
  assert.equal(k.isPrefix(ctrl('a'), 'ctrl+a'), true);
  assert.equal(k.isPrefix(ctrl('b'), 'ctrl+a'), false);
  assert.equal(k.isPrefix(ctrl('b'), 'off'), false);
  assert.deepEqual(k.interpret(false, ctrl('b'), 'off'), { waiting: false, swallow: false, action: null, unknown: false });
  assert.deepEqual(k.interpret(true, key('%'), 'off'), { waiting: false, swallow: false, action: null, unknown: false }, 'off never acts');
  assert.equal(k.isPrefixSetting('ctrl+b') && k.isPrefixSetting('ctrl+a') && k.isPrefixSetting('off'), true);
  for (const bad of ['ctrl+c', '', null, 1, 'Ctrl+B']) assert.equal(k.isPrefixSetting(bad), false, String(bad));
});

test('keys that are not the prefix reach the terminal untouched', () => {
  for (const other of [key('a'), ctrl('c'), key('%'), key('Enter'), key('ArrowUp')]) {
    assert.deepEqual(k.interpret(false, other, 'ctrl+b'), { waiting: false, swallow: false, action: null, unknown: false });
  }
});

test('prefix then a command key: the tmux bindings', () => {
  const after = (input) => {
    const first = k.interpret(false, ctrl('b'), 'ctrl+b');
    assert.deepEqual(first, { waiting: true, swallow: true, action: null, unknown: false });
    return k.interpret(true, input, 'ctrl+b');
  };
  const cases = [
    [key('%', { shiftKey: true }), { kind: 'split', dir: 'row' }],
    [key('"', { shiftKey: true }), { kind: 'split', dir: 'column' }],
    [key('ArrowLeft'), { kind: 'focus', dir: 'left' }],
    [key('ArrowDown'), { kind: 'focus', dir: 'down' }],
    [key('ArrowRight', { ctrlKey: true }), { kind: 'resize', dir: 'right', big: false }],
    [key('ArrowUp', { ctrlKey: true }), { kind: 'resize', dir: 'up', big: false }],
    [key('ArrowLeft', { altKey: true }), { kind: 'resize', dir: 'left', big: true }],
    [key('1'), { kind: 'select', index: 0 }],
    [key('9'), { kind: 'select', index: 8 }],
    [key('0'), { kind: 'select', index: 9 }],
    [key('o'), { kind: 'next-pane' }],
    [key('x'), { kind: 'close-pane' }],
    [key('z'), { kind: 'zoom' }],
    [key(' '), { kind: 'next-layout' }],
    [key('c'), { kind: 'new-terminal' }],
    [key('['), { kind: 'copy-mode' }],
    [key('/'), { kind: 'search' }],
    [key('L', { shiftKey: true }), { kind: 'toggle-log' }],
    [key('S', { shiftKey: true }), { kind: 'sync' }],
    [key('?', { shiftKey: true }), { kind: 'help' }]
  ];
  for (const [input, action] of cases) {
    assert.deepEqual(after(input), { waiting: false, swallow: true, action, unknown: false }, JSON.stringify(input));
  }
});

test('the prefix twice sends it through; Escape cancels; an unbound key is reported; Shift alone keeps waiting', () => {
  assert.deepEqual(k.interpret(true, ctrl('b'), 'ctrl+b'), { waiting: false, swallow: true, action: { kind: 'send-prefix' }, unknown: false });
  assert.equal(k.PREFIX_BYTES['ctrl+b'], '\u0002');
  assert.equal(k.PREFIX_BYTES['ctrl+a'], '\u0001');
  assert.deepEqual(k.interpret(true, key('Escape'), 'ctrl+b'), { waiting: false, swallow: true, action: null, unknown: false });
  assert.deepEqual(k.interpret(true, key('q'), 'ctrl+b'), { waiting: false, swallow: true, action: null, unknown: true });
  assert.deepEqual(k.interpret(true, ctrl('x'), 'ctrl+b'), { waiting: false, swallow: true, action: null, unknown: true }, 'Ctrl+letter is not a command');
  for (const mod of ['Shift', 'Control', 'Alt', 'Meta']) {
    assert.deepEqual(k.interpret(true, key(mod, { shiftKey: mod === 'Shift' }), 'ctrl+b'), { waiting: true, swallow: false, action: null, unknown: false }, mod);
  }
  // Every documented binding exists in the help list, and the list names the keys a person presses.
  assert.ok(k.BINDINGS.length >= 15 && k.BINDINGS.every(b => b.keys && b.what));
});

const extent = { lines: 100, cols: 80, rows: 24 };

test('copy mode moves a cursor inside the buffer and never past its edges', () => {
  let s = c.startCopy({ row: 50, col: 10 }, extent);
  assert.deepEqual(s, { cursor: { row: 50, col: 10 }, anchor: null });
  const press = (input) => { const step = c.copyKey(s, input, extent); s = step.state; return step.effect; };
  assert.equal(press(key('k')), 'none'); assert.deepEqual(s.cursor, { row: 49, col: 10 });
  press(key('ArrowDown')); press(key('j')); assert.deepEqual(s.cursor, { row: 51, col: 10 });
  press(key('h')); press(key('ArrowLeft')); assert.equal(s.cursor.col, 8);
  press(key('l')); assert.equal(s.cursor.col, 9);
  press(key('PageUp')); assert.equal(s.cursor.row, 51 - 23);
  press(key('PageDown')); assert.equal(s.cursor.row, 51);
  press(key('u', { ctrlKey: true })); assert.equal(s.cursor.row, 39);
  press(key('d', { ctrlKey: true })); assert.equal(s.cursor.row, 51);
  press(key('0')); assert.equal(s.cursor.col, 0);
  press(key('$')); assert.equal(s.cursor.col, 79);
  press(key('g')); assert.deepEqual(s.cursor, { row: 0, col: 0 });
  press(key('k')); press(key('h')); assert.deepEqual(s.cursor, { row: 0, col: 0 }, 'top-left edge holds');
  press(key('G')); assert.deepEqual(s.cursor, { row: 99, col: 0 });
  press(key('j')); assert.equal(s.cursor.row, 99, 'last line holds');
  assert.deepEqual(c.startCopy({ row: 500, col: -3 }, extent).cursor, { row: 99, col: 0 }, 'a start outside the buffer is pulled in');
});

test('copy mode selects from the anchor to the cursor, copies only a selection, and leaves on q or Escape', () => {
  let s = c.startCopy({ row: 10, col: 5 }, extent);
  assert.deepEqual(c.highlight(s, 80), { col: 5, row: 10, length: 1 }, 'no selection: just the cursor cell');
  assert.equal(c.copyKey(s, key('Enter'), extent).effect, 'none', 'nothing to copy yet');
  s = c.copyKey(s, key('v'), extent).state;
  assert.deepEqual(s.anchor, { row: 10, col: 5 });
  for (const input of [key('j'), key('j'), key('l')]) s = c.copyKey(s, input, extent).state;
  assert.deepEqual(c.highlight(s, 80), { col: 5, row: 10, length: 2 * 80 + 1 + 1 });
  // Backwards from the anchor reads the same way (reading order).
  let back = c.startCopy({ row: 10, col: 5 }, extent);
  back = c.copyKey(back, key(' '), extent).state;
  back = c.copyKey(back, key('k'), extent).state;
  assert.deepEqual(c.highlight(back, 80), { col: 5, row: 9, length: 81 });
  assert.equal(c.copyKey(s, key('y'), extent).effect, 'copy');
  assert.equal(c.copyKey(s, key('Enter'), extent).effect, 'copy');
  assert.equal(c.copyKey(s, key('v'), extent).state.anchor, null, 'v again cancels the selection');
  assert.equal(c.copyKey(s, key('q'), extent).effect, 'exit');
  assert.equal(c.copyKey(s, key('Escape'), extent).effect, 'exit');
  assert.equal(c.copyKey(s, key('/'), extent).effect, 'search');
  assert.equal(c.copyKey(s, key('n'), extent).effect, 'next-match');
  assert.equal(c.copyKey(s, key('N', { shiftKey: true }), extent).effect, 'previous-match');
  assert.deepEqual(c.copyKey(s, key('a'), extent), { state: s, effect: 'none' }, 'other keys do nothing (and type nothing)');
  assert.match(c.copyStatus(s, extent), /selecting/);
  assert.match(c.copyStatus(c.startCopy({ row: 0, col: 0 }, extent), extent), /line 1 of 100/);
});

test('copy mode keeps its cursor on screen by scrolling as little as possible', () => {
  assert.equal(c.scrollFor(50, 40, 24), 40, 'already visible');
  assert.equal(c.scrollFor(30, 40, 24), 30, 'above: scroll up to it');
  assert.equal(c.scrollFor(70, 40, 24), 47, 'below: it becomes the last visible line');
});

const sid = n => n.toString(16).padStart(8, '0');

test('keyboard resize moves the focused pane\'s nearest divider in the arrow\'s direction', () => {
  // [A | B] side by side, B focused.
  let s = p.assign(p.defaultState(), 1, sid(1));
  s = p.split(s, 1, 'row');
  s = p.assign(s, s.focused, sid(2));
  const splitId = s.root.id;
  assert.equal(s.root.ratio, 0.5);
  // B has no divider on its right: Right moves its left divider right (B shrinks), Left moves it left (B grows).
  assert.equal(p.resizeFocused(s, 'right').root.ratio, 0.55);
  assert.equal(p.resizeFocused(s, 'left').root.ratio, 0.45);
  // A focused: its divider is on its right.
  const a = p.focus(s, p.leaves(s.root)[0].id);
  assert.equal(p.resizeFocused(a, 'right').root.ratio, 0.55);
  assert.equal(p.resizeFocused(a, 'left', 0.1).root.ratio, 0.4);
  // No stacked split: Up and Down change nothing.
  assert.equal(p.resizeFocused(a, 'up'), a);
  assert.equal(p.resizeFocused(a, 'down'), a);
  // The ratio stays within the usual limits.
  let far = a;
  for (let i = 0; i < 30; i++) far = p.resizeFocused(far, 'right');
  assert.equal(far.root.ratio, p.MAX_RATIO);
  assert.equal(far.root.id, splitId, 'the same split, only its ratio changed');
  // Nested: [A | (B over C)], C focused: Up moves the B/C divider (the nearest stacked one).
  let n = p.split(s, s.focused, 'column');
  n = p.assign(n, n.focused, sid(3));
  const inner = n.root.b;
  assert.equal(p.resizeFocused(n, 'up').root.b.ratio, inner.ratio - 0.05);
  assert.equal(p.resizeFocused(n, 'left').root.ratio, 0.45, 'Left reaches the outer side-by-side divider');
});

test('zoom shows one pane over the whole view without changing the arrangement', () => {
  let s = p.assign(p.defaultState(), 1, sid(1));
  s = p.split(s, 1, 'row');
  const all = p.rects(s.root);
  const id = s.focused;
  assert.deepEqual([...p.zoomedRects(all, id)], [[id, { x: 0, y: 0, w: 1, h: 1 }]]);
  assert.deepEqual(p.zoomedRects(all, null), all);
  assert.deepEqual(p.zoomedRects(all, 999), all, 'a pane that no longer exists is not zoomed');
});

const sp = require(`${root}artifacts/frontend-contract-src/terminals/spatial.js`);
const box = (x, y, w = 100, h = 80) => ({ x, y, w, h });

test('on the Canvas, an arrow goes to the nearest window on that side, preferring one straight ahead', () => {
  const me = box(400, 300);
  const others = [box(700, 300), box(450, 40), box(420, 600), box(60, 320), box(520, 520)];
  assert.equal(sp.nearestInDirection(me, others, 'right'), 0);
  assert.equal(sp.nearestInDirection(me, others, 'up'), 1);
  assert.equal(sp.nearestInDirection(me, others, 'left'), 3);
  // Down: (420,600) is straight below; (520,520) is nearer but off to the side.
  assert.equal(sp.nearestInDirection(me, others, 'down'), 2);
  assert.equal(sp.nearestInDirection(box(900, 300), others, 'right'), -1, 'nothing further that way');
  // Overlapping windows: a centre only a hair to one side does not count as "on that side".
  assert.equal(sp.nearestInDirection(me, [box(400.2, 300)], 'right'), -1);
  assert.equal(sp.nearestInDirection(me, [], 'up'), -1);
});

test('prefix+o on the Canvas cycles windows in reading order', () => {
  assert.deepEqual(sp.readingOrder([box(500, 100), box(100, 100), box(300, 20), box(100, 400)]), [2, 1, 0, 3]);
  assert.deepEqual(sp.readingOrder([]), []);
  // A hand-placed row is a row: 2 px higher on the right does not put it first.
  assert.deepEqual(sp.readingOrder([box(50, 102), box(600, 100)]), [0, 1]);
  // Further apart than the band: separate rows again.
  assert.deepEqual(sp.readingOrder([box(50, 100 + sp.ROW_BAND + 1), box(600, 100)]), [1, 0]);
});

test('as in tmux: arrow moves, Ctrl+arrow resizes a little, Alt+arrow more; arrow commands repeat without the prefix', () => {
  assert.deepEqual(k.command(key('ArrowUp')), { kind: 'focus', dir: 'up' });
  assert.deepEqual(k.command(key('ArrowUp', { ctrlKey: true })), { kind: 'resize', dir: 'up', big: false });
  assert.deepEqual(k.command(key('ArrowUp', { altKey: true })), { kind: 'resize', dir: 'up', big: true });
  assert.equal(k.command(key('ArrowUp', { ctrlKey: true, altKey: true })), null, 'no tmux binding');
  assert.equal(k.command(key('ArrowUp', { metaKey: true })), null);
  // tmux binds the arrow commands with -r (repeat-time 500 ms); the others are one press each.
  assert.equal(k.REPEAT_MS, 500);
  for (const a of [{ kind: 'focus', dir: 'up' }, { kind: 'resize', dir: 'up', big: false }]) assert.equal(k.repeatable(a), true);
  for (const a of [{ kind: 'zoom' }, { kind: 'split', dir: 'row' }, { kind: 'next-pane' }, { kind: 'select', index: 0 }]) assert.equal(k.repeatable(a), false, a.kind);
  for (const m of ['Control', 'Shift', 'Alt']) assert.equal(k.isModifierKey(m), true);
  assert.equal(k.isModifierKey('ArrowUp'), false);
});
