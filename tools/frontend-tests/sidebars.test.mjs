// The sidebars' collapse and dock rules, and the grid they produce.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const s = require(`${root}artifacts/frontend-contract-src/sidebars.js`);

const full = { stage: 'full', dock: 'side' };

test('collapsing steps full, compact, flat and stops; expanding steps back and returns to the side', () => {
  const compact = s.collapse(full);
  const flat = s.collapse(compact);
  assert.deepEqual([compact.stage, flat.stage, s.collapse(flat).stage], ['compact', 'flat', 'flat']);
  const docked = s.dockTo(flat, 'top');
  assert.equal(docked.dock, 'top');
  assert.deepEqual(s.expand(docked), { stage: 'compact', dock: 'top' });
  assert.deepEqual(s.expand(s.expand(docked)), full, 'a full bar is back on its side');
});

test('only a collapsed bar can be docked', () => {
  assert.deepEqual(s.dockTo(full, 'top'), full);
  assert.deepEqual(s.normalize({ stage: 'full', dock: 'bottom' }), full);
});

test('the drop zone follows the pointer: near the top or bottom edge, otherwise the side', () => {
  const area = { top: 100, bottom: 700 };
  assert.equal(s.zoneAt(110, area), 'top');
  assert.equal(s.zoneAt(690, area), 'bottom');
  assert.equal(s.zoneAt(400, area), 'side');
  assert.equal(s.zoneAt(156, area), 'top');
  assert.equal(s.zoneAt(157, area), 'side');
});

test('the default grid puts Files on the left, the workspace next, then the splitter and the module list on the right', () => {
  const g = s.gridFor(s.defaultBars);
  assert.equal(g.columns, 'minmax(240px,var(--side-width)) 5px minmax(300px,1fr) 168px');
  assert.equal(g.rows, 'minmax(0,1fr)');
  assert.equal(g.aside.area, '1 / 1 / 2 / 2');
  assert.equal(g.splitter, '1 / 2 / 2 / 3');
  assert.equal(g.main, '1 / 3 / 2 / 4');
  assert.equal(g.rail.area, '1 / 4 / 2 / 5');
});

test('a collapsed bar on its side is a slim strip and the workspace takes the freed width', () => {
  const rail = s.collapse(full);
  const g = s.gridFor({ rail, aside: s.collapse(s.collapse(full)) });
  assert.equal(g.columns, '24px 0px minmax(300px,1fr) 44px');
  assert.equal(g.splitter, null, 'no splitter when the sessions panel is collapsed');
});

test('a docked bar becomes a row above or below the workspace and its column closes', () => {
  const top = s.gridFor({ rail: s.dockTo(s.collapse(full), 'top'), aside: full });
  assert.equal(top.columns, 'minmax(240px,var(--side-width)) 5px minmax(300px,1fr) 0px');
  assert.equal(top.rows, '44px minmax(0,1fr)');
  assert.equal(top.rail.area, '1 / 1 / 2 / 5');
  assert.equal(top.rail.horizontal, true);
  assert.equal(top.main, '2 / 3 / 3 / 4');
  const both = s.gridFor({ rail: s.dockTo(s.collapse(s.collapse(full)), 'top'), aside: s.dockTo(s.collapse(full), 'bottom') });
  assert.equal(both.rows, '24px minmax(0,1fr) 44px');
  assert.equal(both.aside.area, '3 / 1 / 4 / 5');
  const same = s.gridFor({ rail: s.dockTo(s.collapse(full), 'bottom'), aside: s.dockTo(s.collapse(full), 'bottom') });
  assert.equal(same.rows, 'minmax(0,1fr) 44px 44px', 'two bars on one edge stack');
  assert.equal(same.rail.area, '2 / 1 / 3 / 5');
  assert.equal(same.aside.area, '3 / 1 / 4 / 5');
});

test('stored bars that are not understood fall back to the default for that bar', () => {
  assert.deepEqual(s.parseBars(null), s.defaultBars);
  assert.deepEqual(s.parseBars({ rail: { stage: 'huge', dock: 'side' } }).rail, full);
  assert.deepEqual(s.parseBars({ aside: { stage: 'flat', dock: 'bottom' } }).aside, { stage: 'flat', dock: 'bottom' });
  assert.deepEqual(s.parseBars({ aside: { stage: 'full', dock: 'top' } }).aside, full, 'a full bar is put back on its side');
});

test('dropping a full bar on the top or bottom edge collapses it to compact first; dropping on its side changes nothing for a full bar', () => {
  assert.deepEqual(s.dropOn(full, 'top'), { stage: 'compact', dock: 'top' });
  assert.deepEqual(s.dropOn(full, 'bottom'), { stage: 'compact', dock: 'bottom' });
  assert.deepEqual(s.dropOn(full, 'side'), full);
  const flat = { stage: 'flat', dock: 'top' };
  assert.deepEqual(s.dropOn(flat, 'bottom'), { stage: 'flat', dock: 'bottom' });
  assert.deepEqual(s.dropOn(flat, 'side'), { stage: 'flat', dock: 'side' });
});

test('the module list can go on the left: the grid is mirrored and the panel takes the right', () => {
  const g = s.gridFor(s.defaultBars, { side: 'left', panel: 'files' });
  assert.equal(g.columns, '168px minmax(300px,1fr) 5px minmax(240px,var(--side-width))');
  assert.equal(g.rail.area, '1 / 1 / 2 / 2');
  assert.equal(g.main, '1 / 2 / 2 / 3');
  assert.equal(g.splitter, '1 / 3 / 2 / 4');
  assert.equal(g.aside.area, '1 / 4 / 2 / 5');
  // Docked bars still span the whole width.
  const docked = s.gridFor({ rail: s.dockTo(s.collapse(full), 'top'), aside: full }, { side: 'left', panel: 'sessions' });
  assert.equal(docked.rail.area, '1 / 1 / 2 / 5');
  assert.equal(docked.columns, '0px minmax(300px,1fr) 5px minmax(240px,var(--side-width))');
});

test('a closed panel takes no room, not even when it was docked to an edge', () => {
  const g = s.gridFor(s.defaultBars, { side: 'left', panel: null });
  assert.equal(g.columns, '168px minmax(300px,1fr) 0px 0px');
  assert.equal(g.splitter, null);
  const docked = s.gridFor({ rail: full, aside: s.dockTo(s.collapse(full), 'bottom') }, { side: 'left', panel: null });
  assert.equal(docked.rows, 'minmax(0,1fr)');
});

test('panel buttons open, switch and close; stored arrangements that are not understood fall back to the default', () => {
  assert.equal(s.togglePanel(null, 'files'), 'files');
  assert.equal(s.togglePanel('files', 'sessions'), 'sessions');
  assert.equal(s.togglePanel('sessions', 'sessions'), null);
  assert.deepEqual(s.parseArrangement(null), s.defaultArrangement);
  assert.deepEqual(s.parseArrangement({ side: 'right', panel: null }), { side: 'right', panel: null });
  assert.deepEqual(s.parseArrangement({ side: 'up', panel: 'music' }), s.defaultArrangement);
});
