// Pure tests of the inspector's state groups, sorting, sections and colours.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const g = require(`${root}artifacts/frontend-contract-src/inspector/groups.js`);

const NOW = 1_800_000_000_000;
const MIN = 60_000;
let n = 0;
const src = (over = {}) => ({ id: (++n).toString(16).padStart(16, '0'), agent: 'claude', label: 'C--Users-USERPROFILE-proj', modified_ms: NOW - 10 * MIN, bytes: 5000, active: false, running: null, model: null, last_line: null, folder: null, kind: 'session', parent: null, ...over });

test('a session is active, idle, inactive or other by how recently it moved, whether Claude says it runs, and whether it has content', () => {
  assert.equal(g.classify(src({ modified_ms: NOW - 30_000 }), NOW), 'active');
  assert.equal(g.classify(src({ modified_ms: NOW - 2 * MIN }), NOW), 'active', 'the boundary is inclusive');
  assert.equal(g.classify(src({ modified_ms: NOW - 2 * MIN - 1 }), NOW), 'idle');
  assert.equal(g.classify(src({ modified_ms: NOW - 2 * 60 * MIN }), NOW), 'idle');
  assert.equal(g.classify(src({ modified_ms: NOW - 2 * 60 * MIN - 1 }), NOW), 'inactive');
  assert.equal(g.classify(src({ modified_ms: NOW - 99 * 60 * MIN }), NOW), 'inactive');
  assert.equal(g.classify(src({ bytes: 216 }), NOW), 'other', 'a stub has nothing to show');
  assert.equal(g.classify(src({ modified_ms: NOW - 99 * 60 * MIN, running: 'background' }), NOW), 'active', 'running beats file age');
  assert.equal(g.classify(src({ bytes: 100, running: 'interactive' }), NOW), 'active');
  assert.equal(g.classify(src({ modified_ms: NOW + 5000 }), NOW), 'active', 'a clock a little ahead is not an error');
});

test('tab counts add up and tabs filter', () => {
  const list = [src({ modified_ms: NOW - 1000 }), src({ running: 'background', modified_ms: NOW - 9e8 }), src(), src({ modified_ms: NOW - 9e8 }), src({ bytes: 10 })];
  const c = g.counts(list, NOW);
  assert.deepEqual(c, { all: 5, active: 2, idle: 1, inactive: 1, other: 1 });
  assert.equal(c.active + c.idle + c.inactive + c.other, c.all);
  assert.equal(g.inTab(list, 'all', NOW).length, 5);
  assert.equal(g.inTab(list, 'idle', NOW).length, 1);
  assert.deepEqual(g.TABS.map(t => t.id), ['all', 'active', 'idle', 'inactive', 'other'], 'other comes last');
});

test('sorting is stable and sections group by agent, model or folder', () => {
  const a = src({ agent: 'pi', model: 'gemini-x', folder: 'beta', modified_ms: NOW - 5 * MIN });
  const b = src({ agent: 'claude', model: 'claude-opus-5', folder: 'Alpha', modified_ms: NOW - 20 * MIN });
  const c = src({ agent: 'claude', model: 'claude-opus-5', folder: 'alpha', modified_ms: NOW - 1 * MIN });
  const d = src({ agent: 'codex', model: null, folder: null, label: 'x', modified_ms: NOW - 3 * MIN });
  const list = [a, b, c, d];
  assert.deepEqual(g.sortSources(list, 'newest').map(s => s.id), [c, d, a, b].map(s => s.id));
  assert.deepEqual(g.sortSources(list, 'agent').map(s => s.id), [c, b, a, d].map(s => s.id), 'claude, pi, codex; newest first within');
  assert.deepEqual(g.sortSources(list, 'model').map(s => s.id), [c, b, a, d].map(s => s.id), 'unknown model last');
  const byFolder = g.sections(list, 'folder');
  assert.deepEqual(byFolder.map(s => s.items.length), [2, 1, 1], 'folder names group without regard to case');
  assert.deepEqual(g.sections(list, 'agent').map(s => s.label), ['Claude Code', 'Pi', 'Codex']);
  assert.deepEqual(g.sections(list, 'model').map(s => s.label), ['claude-opus-5', 'gemini-x', 'Model not named']);
  const flat = g.sections(list, 'newest');
  assert.equal(flat.length, 1);
  assert.equal(flat[0].label, null);
  assert.deepEqual(g.sections([], 'agent'), []);
  assert.deepEqual(g.sections([], 'newest'), []);
  assert.equal(list.length, 4, 'inputs are never reordered in place');
});

test('names come from the folder, else a tidied label', () => {
  assert.equal(g.displayName(src({ folder: 'agent-workbench' })), 'agent-workbench');
  assert.equal(g.tidyLabel('C--Users-USERPROFILE-Projects-agent-workbench'), 'Projects-agent-workbench');
  assert.equal(g.tidyLabel('--C--Users-USERPROFILE--'), 'home folder');
  assert.equal(g.tidyLabel('C--Users-USERPROFILE'), 'home folder');
  assert.equal(g.tidyLabel('2026-10-22'), '2026-10-22');
  assert.equal(g.tidyLabel(''), 'home folder');
});

test('a subagent is named after its parent, else its folder, else its label, plus a short tag; a session is unchanged', () => {
  const sub = src({ kind: 'subagent', id: 'abcd000000000001', folder: 'workbench' });
  assert.equal(g.displayName(sub, 'agent-workbench'), 'agent-workbench \u00b7 sub abcd');
  assert.equal(g.displayName(sub), 'workbench \u00b7 sub abcd');
  assert.equal(g.displayName(sub, null), 'workbench \u00b7 sub abcd');
  assert.equal(g.displayName({ ...sub, folder: null }), `${g.tidyLabel('C--Users-USERPROFILE-proj')} \u00b7 sub abcd`);
  const session = src({ folder: 'agent-workbench' });
  assert.equal(g.displayName(session), 'agent-workbench');
  assert.equal(g.displayName(session, 'ignored parent'), 'agent-workbench', 'a session never takes a parent name');
});

test('colours are steady per model, palettes are fixed, and model names are shortened', () => {
  assert.equal(g.modelColor('claude-opus-5'), g.modelColor('claude-opus-5'));
  assert.notEqual(g.modelColor('claude-opus-5'), g.modelColor('gpt-6'));
  assert.equal(g.modelColor(null), '#8696ab');
  assert.match(g.modelColor('x'), /^hsl\(\d+ 55% 68%\)$/);
  assert.deepEqual(g.TAG_NAMES, ['red', 'orange', 'yellow', 'green', 'blue', 'purple']);
  for (const name of g.TAG_NAMES) assert.match(g.TAG_COLOR[name], /^#[0-9a-f]{6}$/);
  assert.equal(g.isTagName('red'), true);
  assert.equal(g.isTagName('pink'), false);
  assert.equal(g.shortModel('claude-opus-5-20261001'), 'opus-5');
  assert.equal(g.shortModel('models/gemini-3-pro'), 'gemini-3-pro');
  assert.equal(g.shortModel('x'.repeat(40)).length, 22);
});

test('ages are compact for tiles', () => {
  assert.equal(g.agoShort(NOW - 1000, NOW), 'now');
  assert.equal(g.agoShort(NOW - 40_000, NOW), '40s');
  assert.equal(g.agoShort(NOW - 12 * MIN, NOW), '12m');
  assert.equal(g.agoShort(NOW - 5 * 60 * MIN, NOW), '5h');
  assert.equal(g.agoShort(NOW - 72 * 60 * MIN, NOW), '3d');
  assert.equal(g.agoShort(NOW + 9999, NOW), 'now');
});
