// Pure tests of the terminals pane tree (tmux-style splits).
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const p = require(`${root}artifacts/frontend-contract-src/terminals/panes.js`);

const sid = n => n.toString(16).padStart(8, '0');
const A = sid(0xa), B = sid(0xb), C = sid(0xc);
const sessionsOf = s => p.leaves(s.root).map(l => l.session);

test('a new window is one empty pane; splitting adds an empty pane and focuses it', () => {
  let s = p.defaultState();
  assert.equal(p.leafCount(s), 1);
  assert.equal(p.focusedSession(s), null);
  const first = s.focused;
  s = p.split(s, first, 'row');
  assert.equal(p.leafCount(s), 2);
  assert.notEqual(s.focused, first, 'the new pane has focus');
  assert.deepEqual(p.leaves(s.root).map(l => l.id), [first, s.focused], 'original stays first');
  const other = p.split(s, first, 'column');
  assert.equal(p.leafCount(other), 3);
  assert.equal(p.split(s, 9999, 'row'), s, 'unknown pane is ignored');
});

test('there is no small fixed cap: panes can be split up to MAX_LEAVES, then splitting stops', () => {
  let s = p.defaultState();
  for (let i = 1; i < p.MAX_LEAVES; i++) {
    // Spread the splits so the depth limit is not what stops us.
    const target = p.leaves(s.root)[i % p.leafCount(s)].id;
    s = p.split(s, target, i % 2 ? 'row' : 'column');
  }
  assert.equal(p.leafCount(s), p.MAX_LEAVES);
  assert.ok(p.MAX_LEAVES >= 32, 'room for the 16/32-terminal case');
  assert.equal(p.split(s, s.focused, 'row'), s, 'at the cap nothing changes');
  assert.ok(p.depth(s.root) <= p.MAX_DEPTH);
});

test('a long chain of splits is bounded by the pane cap, never by recursion depth', () => {
  let s = p.defaultState();
  for (let i = 0; i < 80; i++) s = p.split(s, s.focused, 'row');
  assert.equal(p.leafCount(s), p.MAX_LEAVES);
  assert.equal(p.depth(s.root), p.MAX_LEAVES - 1);
  assert.equal(p.depth(s.root), p.MAX_DEPTH);
});

test('a session is shown in at most one pane and assigning moves it', () => {
  let s = p.split(p.defaultState(), 1, 'row');
  const [l1, l2] = p.leaves(s.root).map(l => l.id);
  s = p.assign(s, l1, A);
  s = p.assign(s, l2, B);
  assert.deepEqual(sessionsOf(s), [A, B]);
  s = p.assign(s, l2, A);
  assert.deepEqual(sessionsOf(s), [null, A], 'A moved');
  assert.equal(s.focused, l2);
  assert.equal(p.leafOf(s, A), l2);
  assert.equal(p.leafOf(s, B), null);
  assert.equal(p.assign(s, 999, C), s, 'unknown pane ignored');
});

test('closing a pane gives its space to the sibling; the last pane is only emptied; terminals are never ended', () => {
  let s = p.split(p.defaultState(), 1, 'row');
  const [l1, l2] = p.leaves(s.root).map(l => l.id);
  s = p.assign(p.assign(s, l1, A), l2, B);
  const closed = p.close(s, l2);
  assert.equal(p.leafCount(closed), 1);
  assert.deepEqual(sessionsOf(closed), [A]);
  assert.equal(closed.focused, l1, 'focus moves off the closed pane');
  const nested = p.split(s, l2, 'column');
  const inner = p.leaves(nested.root).map(l => l.id);
  const back = p.close(nested, inner[1]);
  assert.equal(p.leafCount(back), 2);
  const lone = p.close(p.assign(p.defaultState(), 1, A), 1);
  assert.equal(p.leafCount(lone), 1);
  assert.deepEqual(sessionsOf(lone), [null]);
  assert.equal(p.close(s, 999), s);
});

test('sessions that no longer exist are dropped from panes, panes stay', () => {
  let s = p.split(p.defaultState(), 1, 'row');
  const [l1, l2] = p.leaves(s.root).map(l => l.id);
  s = p.assign(p.assign(s, l1, A), l2, B);
  const r = p.reconcile(s, new Set([B]));
  assert.deepEqual(sessionsOf(r), [null, B]);
  assert.equal(p.leafCount(r), 2);
  assert.equal(p.reconcile(r, new Set([B])), r, 'no change returns the same object');
});

test('a new session goes to the focused pane if empty, else the first empty pane, else the focused one', () => {
  let s = p.split(p.split(p.defaultState(), 1, 'row'), 1, 'column');
  const ids = p.leaves(s.root).map(l => l.id);
  assert.equal(p.leafForNew(s), s.focused, 'focused pane is empty');
  s = p.assign(s, s.focused, A);
  const empty = p.leaves(s.root).find(l => l.session === null).id;
  assert.equal(p.leafForNew(s), empty);
  for (const id of ids) s = p.assign(s, id, sid(0x100 + id));
  assert.equal(p.leafForNew(s), s.focused);
});

test('focus moves spatially and stops at edges', () => {
  // [ L1 | ( L2 over L3 ) ]
  let s = p.split(p.defaultState(), 1, 'row');
  const right = p.leaves(s.root)[1].id;
  s = p.split(s, right, 'column');
  const [l1, l2, l3] = p.leaves(s.root).map(l => l.id);
  assert.equal(p.neighbor(s, l1, 'right'), l2, 'upper-right is nearest to the centre line of L1 first');
  assert.equal(p.neighbor(s, l2, 'down'), l3);
  assert.equal(p.neighbor(s, l3, 'up'), l2);
  assert.equal(p.neighbor(s, l3, 'left'), l1);
  assert.equal(p.neighbor(s, l1, 'left'), l1);
  assert.equal(p.neighbor(s, l1, 'up'), l1);
  assert.equal(p.neighbor(s, l3, 'right'), l3);
  assert.equal(p.focusDirection(p.focus(s, l2), 'left').focused, l1);
  const atL1 = p.focus(s, l1);
  assert.equal(p.focusDirection(atL1, 'left'), atL1, 'edge keeps the same state');
  assert.equal(p.focusIndex(s, 2).focused, l3);
  assert.equal(p.focusIndex(s, 9), s);
});

test('divider ratios are clamped, derived from the pointer and addressed by split', () => {
  assert.equal(p.clampRatio(0.5), 0.5);
  assert.equal(p.clampRatio(0.01), p.MIN_RATIO);
  assert.equal(p.clampRatio(5), p.MAX_RATIO);
  assert.equal(p.clampRatio(NaN), 0.5);
  assert.equal(p.ratioFromPointer(600, 100, 1000), 0.5);
  assert.equal(p.ratioFromPointer(0, 100, 1000), p.MIN_RATIO);
  assert.equal(p.ratioFromPointer(5000, 100, 1000), p.MAX_RATIO);
  assert.equal(p.ratioFromPointer(10, 0, 0), 0.5);
  let s = p.split(p.defaultState(), 1, 'row');
  s = p.split(s, s.focused, 'column');
  const splits = [];
  const walk = n => { if (n.kind === 'split') { splits.push(n); walk(n.a); walk(n.b); } };
  walk(s.root);
  const inner = splits.find(n => n.dir === 'column');
  const moved = p.setRatio(s, inner.id, 0.9);
  const after = [];
  const walk2 = n => { if (n.kind === 'split') { after.push(n); walk2(n.a); walk2(n.b); } };
  walk2(moved.root);
  assert.equal(after.find(n => n.id === inner.id).ratio, p.MAX_RATIO);
  assert.equal(after.find(n => n.dir === 'row').ratio, 0.5, 'other splits untouched');
});

test('tile arranges every terminal in a balanced grid for any count up to the cap', () => {
  for (const n of [1, 2, 3, 4, 5, 7, 9, 16, 17, 32, 50, 64]) {
    const ids = Array.from({ length: n }, (_, i) => sid(i + 1));
    const s = p.tile(p.defaultState(), ids);
    assert.equal(p.leafCount(s), n, `n=${n}`);
    assert.deepEqual(sessionsOf(s), ids, `n=${n}: reading order keeps the given order`);
    assert.equal(s.focused, p.leaves(s.root)[0].id);
    const map = p.rects(s.root);
    let area = 0;
    for (const r of map.values()) {
      assert.ok(r.w > 0 && r.h > 0 && r.x >= 0 && r.y >= 0 && r.x + r.w <= 1 + 1e-9 && r.y + r.h <= 1 + 1e-9, `n=${n} inside`);
      area += r.w * r.h;
    }
    assert.ok(Math.abs(area - 1) < 1e-9, `n=${n}: panes fill the window exactly`);
    // Balanced: rows differ by at most one pane, so no pane is more than twice another (3 terminals: two on top, one wide below).
    const areas = [...map.values()].map(r => r.w * r.h);
    assert.ok(Math.max(...areas) / Math.min(...areas) <= 2.0001, `n=${n} balanced`);
    assert.ok(p.depth(s.root) <= p.MAX_DEPTH);
    const ratios = [];
    const walk = x => { if (x.kind === 'split') { ratios.push(x.ratio); walk(x.a); walk(x.b); } };
    walk(s.root);
    assert.ok(ratios.every(r => r >= 0.3 && r <= 0.7), `n=${n} ratios stay draggable`);
  }
  const s = p.tile(p.defaultState(), Array.from({ length: 70 }, (_, i) => sid(i + 1)));
  assert.equal(p.leafCount(s), p.MAX_LEAVES, 'extra terminals beyond the cap are left out');
  const same = p.defaultState();
  assert.equal(p.tile(same, []), same, 'nothing to tile');
});

test('stored state is validated and repaired, never trusted', () => {
  let s = p.split(p.defaultState(), 1, 'row');
  const [l1, l2] = p.leaves(s.root).map(l => l.id);
  s = p.assign(p.assign(s, l1, A), l2, B);
  assert.deepEqual(p.parse(JSON.parse(JSON.stringify(s))), s, 'round trip');
  assert.deepEqual(p.parse(null), p.defaultState());
  assert.deepEqual(p.parse('x'), p.defaultState());
  // Duplicate session ids, junk ids and bad ratios are repaired; a broken tree falls back to the default.
  const dup = { version: 2, focused: 99, root: { kind: 'split', id: 1, dir: 'row', ratio: 9, a: { kind: 'leaf', id: 2, session: A }, b: { kind: 'leaf', id: 3, session: A } } };
  const fixed = p.parse(dup);
  assert.deepEqual(sessionsOf(fixed), [A, null]);
  assert.equal(fixed.root.ratio, p.MAX_RATIO);
  assert.equal(fixed.focused, 2, 'unknown focus falls to the first pane');
  assert.ok(fixed.nextId > 3);
  assert.deepEqual(p.parse({ version: 2, root: { kind: 'leaf', id: 1, session: 'nope' }, focused: 1 }).root, { kind: 'leaf', id: 1, session: null });
  for (const bad of [
    { version: 2, root: { kind: 'split', id: 1, dir: 'diagonal', ratio: 0.5, a: { kind: 'leaf', id: 2, session: null }, b: { kind: 'leaf', id: 3, session: null } } },
    { version: 2, root: { kind: 'split', id: 1, dir: 'row', ratio: 0.5, a: { kind: 'leaf', id: 1, session: null }, b: { kind: 'leaf', id: 3, session: null } } },
    { version: 2, root: { kind: 'leaf', id: 0, session: null } },
    { version: 2, root: 5 }
  ]) assert.deepEqual(p.parse(bad), p.defaultState());
  // Too many panes are refused.
  const wide = n => n === 0 ? { kind: 'leaf', id: 1, session: null } : { kind: 'split', id: 2 * n, dir: 'row', ratio: 0.5, a: wide(n - 1), b: { kind: 'leaf', id: 2 * n + 1, session: null } };
  assert.deepEqual(p.parse({ version: 2, root: wide(p.MAX_LEAVES), focused: 1 }), p.defaultState(), '65 panes');
  assert.equal(p.leafCount(p.parse({ version: 2, root: wide(p.MAX_LEAVES - 1), focused: 1 })), p.MAX_LEAVES, '64 panes in a chain is accepted');
});

test('older fixed layouts (slots + two ratios) are converted, not lost', () => {
  const legacy = (layout, slots, focused, col = 0.5, row = 0.5) => ({ layout, colRatio: col, rowRatio: row, slots, focused });
  const single = p.parse(legacy('single', [A, null, null, null], 0));
  assert.deepEqual(sessionsOf(single), [A]);
  const cols = p.parse(legacy('columns', [A, B, null, null], 1, 0.3));
  assert.equal(cols.root.dir, 'row');
  assert.equal(cols.root.ratio, 0.3);
  assert.deepEqual(sessionsOf(cols), [A, B]);
  assert.equal(p.focusedSession(cols), B);
  const rows = p.parse(legacy('rows', [A, B, null, null], 0, 0.5, 0.7));
  assert.equal(rows.root.dir, 'column');
  assert.equal(rows.root.ratio, 0.7);
  const grid = p.parse(legacy('grid', [A, B, C, null], 2, 0.4, 0.6));
  assert.equal(p.leafCount(grid), 4);
  assert.deepEqual(sessionsOf(grid), [A, B, C, null]);
  assert.equal(grid.root.dir, 'column');
  assert.equal(grid.root.ratio, 0.6);
  assert.equal(grid.root.a.ratio, 0.4);
  assert.equal(p.focusedSession(grid), C);
  assert.deepEqual(p.parse(legacy('diagonal', [null, null, null, null], 0)), p.defaultState());
  assert.deepEqual(p.parse({ layout: 'single', slots: [A], focused: 0 }), p.defaultState(), 'wrong slot count');
  assert.deepEqual(sessionsOf(p.parse(legacy('grid', [A, A, 'nope', 5], 99))), [A, null, null, null]);
});

test('dividers are drawn per split over the region they divide, and a pointer maps back to a ratio', () => {
  // [ L1 | ( L2 over L3 ) ] with the left share 0.25 and the right column split 0.5.
  let s = p.split(p.defaultState(), 1, 'row');
  const right = p.leaves(s.root)[1].id;
  s = p.split(s, right, 'column');
  const outer = s.root;
  s = p.setRatio(s, outer.id, 0.25);
  const regions = p.splitRegions(s.root);
  assert.equal(regions.length, 2);
  const [o, i] = regions;
  assert.deepEqual(o.at, { x: 0, y: 0, w: 1, h: 1 });
  assert.equal(o.split.dir, 'row');
  assert.deepEqual(i.at, { x: 0.25, y: 0, w: 0.75, h: 1 });
  assert.equal(i.split.dir, 'column');
  // A pointer over the middle of the window drags the outer divider to 0.5, and the inner one only by height.
  assert.equal(p.ratioInRegion(0.5, 0.9, 'row', o.at), 0.5);
  assert.equal(p.ratioInRegion(0.625, 0.8, 'row', i.at), 0.5, 'relative to the region, not the window');
  assert.equal(p.ratioInRegion(0.1, 0.8, 'column', i.at), 0.8);
  assert.equal(p.ratioInRegion(0.0, 0.0, 'row', o.at), p.MIN_RATIO);
  assert.equal(p.ratioInRegion(1, 1, 'column', i.at), p.MAX_RATIO);
  assert.equal(p.ratioInRegion(0.5, 0.5, 'row', { x: 0, y: 0, w: 0, h: 1 }), 0.5, 'zero-size region');
  // Panes stay one entry per leaf, with stable ids across a split (so their terminals are not rebuilt).
  const before = p.leaves(s.root).map(l => l.id);
  const after = p.leaves(p.split(s, before[0], 'column').root).map(l => l.id);
  assert.ok(before.every(id => after.includes(id)));
});

test('only a well-formed session id is accepted from a drag payload', () => {
  assert.equal(p.droppedSession('deadbeef'), 'deadbeef');
  for (const bad of [null, undefined, '', 'DEADBEEF', 'deadbee', 'deadbeef1', 'javascript:alert(1)', '../../x']) assert.equal(p.droppedSession(bad), null, String(bad));
});

// ---- layout presets (tmux <prefix>+Space) ------------------------------------------------

/** Pane boxes in reading order, as [x, y, w, h]. */
const boxes = s => { const map = p.rects(s.root); return p.leaves(s.root).map(l => map.get(l.id)); };
const near = (a, b) => Math.abs(a - b) < 1e-9;
const expectRects = (s, want, label) => {
  const got = boxes(s);
  assert.equal(got.length, want.length, `${label}: pane count`);
  got.forEach((r, i) => {
    const [x, y, w, h] = Array.isArray(want[i]) ? want[i] : [want[i].x, want[i].y, want[i].w, want[i].h];
    assert.ok(near(r.x, x) && near(r.y, y) && near(r.w, w) && near(r.h, h),
      `${label}: pane ${i} is [${r.x}, ${r.y}, ${r.w}, ${r.h}], expected [${x}, ${y}, ${w}, ${h}]`);
  });
};
const many = n => Array.from({ length: n }, (_, i) => sid(i + 1));
const stateOf = (names, focusIndex = 0) => p.focusIndex(p.tile(p.defaultState(), names), focusIndex);
const everyNode = s => { const out = []; const walk = x => { out.push(x); if (x.kind === 'split') { walk(x.a); walk(x.b); } }; walk(s.root); return out; };

test('the preset list is tmux\'s cycle order and nextPreset walks it and wraps', () => {
  assert.deepEqual([...p.PRESETS], ['even-horizontal', 'even-vertical', 'main-horizontal', 'main-vertical', 'tiled']);
  let at = null;
  for (const want of p.PRESETS) { at = p.nextPreset(at); assert.equal(at, want); }
  assert.equal(p.nextPreset(at), p.PRESETS[0], 'after the last comes the first');
  assert.equal(p.nextPreset(p.PRESETS[2]), p.PRESETS[3]);
  assert.equal(p.nextPreset('nonsense'), p.PRESETS[0], 'an unknown preset starts the cycle');
});

test('even-horizontal puts every pane side by side at equal widths; even-vertical stacks them', () => {
  const horizontal = [
    [[0, 0, 1, 1]],
    [[0, 0, 1 / 2, 1], [1 / 2, 0, 1 / 2, 1]],
    [[0, 0, 1 / 3, 1], [1 / 3, 0, 1 / 3, 1], [2 / 3, 0, 1 / 3, 1]],
    [[0, 0, 1 / 4, 1], [1 / 4, 0, 1 / 4, 1], [1 / 2, 0, 1 / 4, 1], [3 / 4, 0, 1 / 4, 1]],
    [[0, 0, 1 / 5, 1], [1 / 5, 0, 1 / 5, 1], [2 / 5, 0, 1 / 5, 1], [3 / 5, 0, 1 / 5, 1], [4 / 5, 0, 1 / 5, 1]]
  ];
  for (let n = 1; n <= 5; n++) expectRects(p.applyPreset(stateOf(many(n)), 'even-horizontal'), horizontal[n - 1], `even-horizontal/${n}`);
  const vertical = horizontal.map(cells => cells.map(([x, y, w, h]) => [y, x, h, w]));
  for (let n = 1; n <= 5; n++) expectRects(p.applyPreset(stateOf(many(n)), 'even-vertical'), vertical[n - 1], `even-vertical/${n}`);
});

test('main-vertical keeps the focused pane on the left at 0.6 and stacks the rest evenly on the right', () => {
  expectRects(p.applyPreset(stateOf(many(1)), 'main-vertical'), [[0, 0, 1, 1]], 'main-vertical/1');
  expectRects(p.applyPreset(stateOf(many(2)), 'main-vertical'), [[0, 0, 0.6, 1], [0.6, 0, 0.4, 1]], 'main-vertical/2');
  expectRects(p.applyPreset(stateOf(many(3)), 'main-vertical'),
    [[0, 0, 0.6, 1], [0.6, 0, 0.4, 0.5], [0.6, 0.5, 0.4, 0.5]], 'main-vertical/3');
  expectRects(p.applyPreset(stateOf(many(4)), 'main-vertical'),
    [[0, 0, 0.6, 1], [0.6, 0, 0.4, 1 / 3], [0.6, 1 / 3, 0.4, 1 / 3], [0.6, 2 / 3, 0.4, 1 / 3]], 'main-vertical/4');
  expectRects(p.applyPreset(stateOf(many(5)), 'main-vertical'),
    [[0, 0, 0.6, 1], ...[0, 1, 2, 3].map(k => [0.6, k / 4, 0.4, 1 / 4])], 'main-vertical/5');
  // The focused pane is the main one wherever it sat, and the others keep their order.
  const moved = p.applyPreset(stateOf(many(4), 2), 'main-vertical');
  assert.deepEqual(sessionsOf(moved), [sid(3), sid(1), sid(2), sid(4)], 'main first, the rest in reading order');
  assert.equal(p.focusedSession(moved), sid(3));
  expectRects(moved, [[0, 0, 0.6, 1], [0.6, 0, 0.4, 1 / 3], [0.6, 1 / 3, 0.4, 1 / 3], [0.6, 2 / 3, 0.4, 1 / 3]], 'main-vertical/focused');
});

test('main-horizontal keeps the focused pane on top at 0.6 and lays the rest side by side below', () => {
  expectRects(p.applyPreset(stateOf(many(1)), 'main-horizontal'), [[0, 0, 1, 1]], 'main-horizontal/1');
  expectRects(p.applyPreset(stateOf(many(2)), 'main-horizontal'), [[0, 0, 1, 0.6], [0, 0.6, 1, 0.4]], 'main-horizontal/2');
  expectRects(p.applyPreset(stateOf(many(3)), 'main-horizontal'),
    [[0, 0, 1, 0.6], [0, 0.6, 0.5, 0.4], [0.5, 0.6, 0.5, 0.4]], 'main-horizontal/3');
  expectRects(p.applyPreset(stateOf(many(4)), 'main-horizontal'),
    [[0, 0, 1, 0.6], [0, 0.6, 1 / 3, 0.4], [1 / 3, 0.6, 1 / 3, 0.4], [2 / 3, 0.6, 1 / 3, 0.4]], 'main-horizontal/4');
  expectRects(p.applyPreset(stateOf(many(5)), 'main-horizontal'),
    [[0, 0, 1, 0.6], ...[0, 1, 2, 3].map(k => [k / 4, 0.6, 1 / 4, 0.4])], 'main-horizontal/5');
  const moved = p.applyPreset(stateOf(many(4), 1), 'main-horizontal');
  assert.deepEqual(sessionsOf(moved), [sid(2), sid(1), sid(3), sid(4)]);
  assert.equal(p.focusedSession(moved), sid(2));
  expectRects(moved, [[0, 0, 1, 0.6], [0, 0.6, 1 / 3, 0.4], [1 / 3, 0.6, 1 / 3, 0.4], [2 / 3, 0.6, 1 / 3, 0.4]], 'main-horizontal/focused');
});

test('every preset keeps each session once, keeps focus, and survives a save/load round trip', () => {
  for (const preset of p.PRESETS) {
    for (let n = 1; n <= 5; n++) {
      const names = many(n);
      for (let focusIndex = 0; focusIndex < n; focusIndex++) {
        const label = `${preset}/${n}@${focusIndex}`;
        const before = stateOf(names, focusIndex);
        const out = p.applyPreset(before, preset);
        assert.equal(p.leafCount(out), n, `${label}: pane count`);
        assert.deepEqual([...sessionsOf(out)].sort(), [...names].sort(), `${label}: each session once`);
        assert.equal(p.focusedSession(out), p.focusedSession(before), `${label}: focus follows the session`);
        assert.ok(p.leafById(out, out.focused), `${label}: the focused leaf exists`);
        const nodes = everyNode(out);
        const ids = nodes.map(x => x.id);
        assert.equal(new Set(ids).size, ids.length, `${label}: ids are unique`);
        assert.equal(ids.length, 2 * n - 1, `${label}: one split per gap`);
        assert.ok(out.nextId > Math.max(...ids), `${label}: nextId is past every id`);
        assert.deepEqual(p.parse(JSON.parse(JSON.stringify(out))), out, `${label}: round trip`);
        const map = p.rects(out.root);
        assert.equal(map.size, n, `${label}: one rect per pane`);
        let area = 0;
        for (const r of map.values()) { assert.ok(r.w > 0 && r.h > 0, `${label}: every pane visible`); area += r.w * r.h; }
        assert.ok(Math.abs(area - 1) < 1e-9, `${label}: the panes fill the window`);
        assert.ok(p.depth(out.root) <= p.MAX_DEPTH, `${label}: depth`);
        assert.ok(nodes.filter(x => x.kind === 'split').every(x => x.ratio >= p.MIN_RATIO && x.ratio <= p.MAX_RATIO), `${label}: ratios stay draggable`);
      }
    }
  }
});

test('presets drop empty panes, keep the reading order, and leave a window with no session alone', () => {
  const empty = p.defaultState();
  for (const preset of p.PRESETS) assert.equal(p.applyPreset(empty, preset), empty, `${preset}: nothing to arrange`);
  const three = stateOf(many(3));
  const holed = p.clear(three, p.leaves(three.root)[1].id);
  for (const preset of p.PRESETS) {
    const out = p.applyPreset(holed, preset);
    assert.equal(p.leafCount(out), 2, `${preset}: the empty pane is dropped`);
    assert.deepEqual(sessionsOf(out), [sid(1), sid(3)], `${preset}: order kept`);
    assert.equal(p.focusedSession(out), sid(1));
  }
  for (const preset of ['even-horizontal', 'even-vertical', 'tiled']) {
    assert.deepEqual(sessionsOf(p.applyPreset(stateOf(many(5)), preset)), many(5), `${preset}: order kept`);
  }
  // A focused pane with no session hands focus to the first pane that has one.
  const two = stateOf(many(2), 1);
  const cleared = p.clear(two, two.focused);
  for (const preset of p.PRESETS) {
    const out = p.applyPreset(cleared, preset);
    assert.deepEqual(sessionsOf(out), [sid(1)], `${preset}: only the assigned session`);
    assert.equal(p.focusedSession(out), sid(1), `${preset}: a session gets the focus`);
  }
  // Junk never reaches the tree.
  for (const bad of ['diagonal', '', null, undefined, 7]) assert.equal(p.applyPreset(three, bad), three, String(bad));
});

test('tiled is exactly what tile() already does, with the focus still on the same session', () => {
  for (const n of [1, 2, 3, 4, 5, 7, 9, 16, 33, p.MAX_LEAVES]) {
    const names = many(n);
    const s = stateOf(names);
    assert.deepEqual(p.applyPreset(s, 'tiled'), p.tile(s, names), `n=${n}`);
    expectRects(p.applyPreset(s, 'tiled'), boxes(p.tile(s, names)), `n=${n}`);
  }
  const five = stateOf(many(5), 3);
  const out = p.applyPreset(five, 'tiled');
  assert.deepEqual(boxes(out), boxes(p.tile(five, many(5))), 'same grid');
  assert.equal(p.focusedSession(out), sid(4), 'tile() focuses the first pane, a preset keeps the session');
});

test('presets respect MAX_LEAVES and never invent panes', () => {
  const full = stateOf(many(p.MAX_LEAVES));
  for (const preset of p.PRESETS) {
    const out = p.applyPreset(full, preset);
    assert.equal(p.leafCount(out), p.MAX_LEAVES, `${preset}: every pane kept`);
    assert.equal(new Set(sessionsOf(out)).size, p.MAX_LEAVES, `${preset}: each session once`);
    assert.ok(p.depth(out.root) <= p.MAX_DEPTH, `${preset}: depth`);
    assert.ok(out.nextId <= 2 * p.MAX_LEAVES, `${preset}: ids stay in range`);
    const area = [...p.rects(out.root).values()].reduce((sum, r) => sum + r.w * r.h, 0);
    assert.ok(Math.abs(area - 1) < 1e-9, `${preset}: fills the window`);
    assert.deepEqual(p.parse(JSON.parse(JSON.stringify(out))), out, `${preset}: round trip at the cap`);
  }
  const over = p.tile(p.defaultState(), many(p.MAX_LEAVES + 6));
  for (const preset of p.PRESETS) assert.equal(p.leafCount(p.applyPreset(over, preset)), p.MAX_LEAVES, `${preset}: stays at the cap`);
});
