// Pure tests for the Files follow/navigation policy.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const f = require(`${root}artifacts/frontend-contract-src/files/follow.js`);

const A = 'C:\\Work\\Alpha';
const B = 'C:\\Work\\Beta';
const focus = (folder, terminalId = 't1') => ({ terminalId, recordedFolder: folder });
const out = (root, following, changed, reason) => ({ root, following, changed, reason });
const initial = (root, following) => f.createFollowPolicy({ root, following });

test('sameFolder accepts only two valid full paths, case-insensitively', () => {
  assert.equal(f.sameFolder(A, 'c:\\work\\alpha\\'), true);
  assert.equal(f.sameFolder('C:\\Work\\Alpha\\src', 'c:\\WORK\\alpha\\SRC'), true);
  assert.equal(f.sameFolder('C:\\', 'c:\\'), true);
  assert.equal(f.sameFolder('C:\\', 'C:\\\\'), true);
  assert.equal(f.sameFolder(B, A), false, 'the whole path, not just the basename, is compared');
  assert.equal(f.sameFolder('D:\\Work\\Alpha', A), false, 'the drive letter counts');
  assert.equal(f.sameFolder('C:\\Work\\Alpha2', A), false);
  assert.equal(f.sameFolder('C:\\Work\\Alpha\\src', A), false);
  assert.equal(f.sameFolder('C:\\', 'C:\\Work'), false);
  for (const bad of [null, undefined, '', 'relative', 'C:foo', 'C:\\a\\..\\b', 'C:\\a\\.\\b', 'C:\\bad|name']) {
    assert.equal(f.sameFolder(A, bad), false, `right operand ${String(bad)}`);
    assert.equal(f.sameFolder(bad, A), false, `left operand ${String(bad)}`);
    assert.equal(f.sameFolder(bad, bad), false, `both operands ${String(bad)}`);
  }
});

test('construction accepts a valid root and follows only literal true', () => {
  const on = initial(A, true);
  assert.deepEqual(on.state(), { root: A, following: true, closed: false });
  const content = on.captureContentToken();
  assert.equal(on.isContentCurrent(content), true, 'construction does not load or advance');
  for (const value of ['true', 1, {}, [], null, undefined, false]) {
    const p = initial(A, value);
    assert.deepEqual(p.state(), { root: A, following: false, closed: false }, `following=${String(value)}`);
    assert.equal(p.focusChanged(focus(B)).reason, 'following-off');
  }
  assert.deepEqual(initial('C:\\a\\..\\b', true).state(), { root: null, following: true, closed: false });
  assert.deepEqual(initial(null, false).state(), { root: null, following: false, closed: false });
  assert.deepEqual(initial(undefined, 'true').state(), { root: null, following: false, closed: false });
  assert.deepEqual(f.createFollowPolicy({ root: A }).state(), { root: A, following: false, closed: false });
  assert.deepEqual(f.createFollowPolicy({ following: true }).state(), { root: null, following: true, closed: false });
});

test('setFollowing changes state only when it toggles, advancing the right token', () => {
  const p = initial(A, false);
  let content = p.captureContentToken();
  let intent = p.captureIntentToken('picker');
  assert.deepEqual(p.setFollowing(false, focus(B)), out(A, false, false, 'unchanged'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('picker', intent), true);

  assert.deepEqual(p.setFollowing(true, { terminalId: null, recordedFolder: B }), out(A, true, false, 'follow-enabled'));
  assert.equal(p.isContentCurrent(content), true, 'ineligible focus does not load');
  assert.equal(p.isIntentCurrent('picker', intent), false, 'an effective toggle advances intent');

  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.setFollowing(true, focus(B)), out(A, true, false, 'unchanged'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('startup', intent), true);

  assert.deepEqual(p.setFollowing(false, focus(B)), out(A, false, false, 'follow-disabled'));
  assert.equal(p.isContentCurrent(content), true, 'OFF keeps the listing');
  assert.equal(p.isIntentCurrent('startup', intent), false);

  content = p.captureContentToken();
  assert.deepEqual(p.setFollowing(true, focus('c:\\work\\alpha\\')), out(A, true, false, 'follow-enabled'));
  assert.equal(p.isContentCurrent(content), true, 'same folder does not reload');

  assert.deepEqual(p.setFollowing(false, focus(A)), out(A, false, false, 'follow-disabled'));
  assert.deepEqual(p.setFollowing(true, focus(B)), out(B, true, true, 'follow-enabled-navigation'));
  assert.equal(p.isContentCurrent(content), false, 'navigation advances content');
  assert.deepEqual(initial(null, false).setFollowing(true, focus(A)), out(A, true, true, 'follow-enabled-navigation'));
});

test('focusChanged navigates only for a different valid folder while following', () => {
  const p = initial(A, true);
  let content = p.captureContentToken();
  let intent = p.captureIntentToken('startup');
  assert.deepEqual(p.focusChanged(focus('c:\\work\\alpha\\')), out(A, true, false, 'same-focus'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('startup', intent), true);
  for (const value of [null, undefined, { terminalId: null, recordedFolder: B }, { terminalId: 't1', recordedFolder: null }, { terminalId: 't1', recordedFolder: undefined }, focus('relative')]) {
    assert.deepEqual(p.focusChanged(value), out(A, true, false, 'ineligible-focus'), String(value));
    assert.equal(p.isContentCurrent(content), true);
    assert.equal(p.isIntentCurrent('startup', intent), true);
  }
  assert.deepEqual(p.focusChanged(focus(B)), out(B, true, true, 'focused-folder'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);
  content = p.captureContentToken();
  intent = p.captureIntentToken('picker');
  assert.deepEqual(p.focusChanged(focus('c:\\work\\beta\\')), out(B, true, false, 'same-focus'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('picker', intent), true);
  p.pause();
  intent = p.captureIntentToken('picker');
  assert.deepEqual(p.focusChanged(focus(A)), out(B, false, false, 'following-off'));
  assert.equal(p.isIntentCurrent('picker', intent), true);
  assert.equal(p.isContentCurrent(content), true);
});

test('setInitialRoot refreshes a valid path, even a case-equivalent one', () => {
  const p = initial(A, true);
  let content = p.captureContentToken();
  let intent = p.captureIntentToken('startup');
  for (const bad of [null, undefined, 'relative', 'C:\\a\\..\\b']) {
    assert.deepEqual(p.setInitialRoot(bad), out(A, true, false, 'invalid-initial'), String(bad));
    assert.equal(p.isContentCurrent(content), true);
    assert.equal(p.isIntentCurrent('startup', intent), true);
  }
  assert.deepEqual(p.setInitialRoot('c:\\work\\alpha\\'), out('c:\\work\\alpha\\', true, true, 'initialized'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);
  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.setInitialRoot(B), out(B, true, true, 'initialized'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);
});

test('manualUp always pauses and reloads only when a parent exists', () => {
  const p = initial(A, true);
  let content = p.captureContentToken();
  let intent = p.captureIntentToken('picker');
  assert.deepEqual(p.manualUp(), out('C:\\Work', false, true, 'manual-up'));
  assert.equal(p.state().following, false);
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('picker', intent), false);

  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.manualUp(), out('C:\\', false, true, 'manual-up'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);

  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.manualUp(), out('C:\\', false, false, 'manual-up-no-parent'));
  assert.equal(p.isContentCurrent(content), true, 'no parent keeps the listing');
  assert.equal(p.isIntentCurrent('startup', intent), false, 'manualUp always advances intent');

  const q = initial('C:\\', true);
  const qContent = q.captureContentToken();
  const qIntent = q.captureIntentToken('picker');
  assert.deepEqual(q.manualUp(), out('C:\\', false, false, 'manual-up-no-parent'));
  assert.equal(q.isContentCurrent(qContent), true);
  assert.equal(q.isIntentCurrent('picker', qIntent), false);

  const n = initial(null, true);
  assert.deepEqual(n.manualUp(), out(null, false, false, 'manual-up-no-parent'));
});

test('manualChoose pauses and commits only a different valid path', () => {
  const p = initial(A, true);
  let content = p.captureContentToken();
  let intent = p.captureIntentToken('picker');
  assert.deepEqual(p.manualChoose('c:\\work\\alpha\\'), out(A, false, false, 'manual-choice-same'));
  assert.equal(p.state().following, false);
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('picker', intent), false, 'choosing always advances intent');

  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.manualChoose('relative'), out(A, false, false, 'manual-choice-invalid'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('startup', intent), false);

  content = p.captureContentToken();
  intent = p.captureIntentToken('startup');
  assert.deepEqual(p.manualChoose(B), out(B, false, true, 'manual-choice'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);

  const q = initial(null, true);
  assert.deepEqual(q.manualChoose(null), out(null, false, false, 'manual-choice-invalid'));
  assert.deepEqual(q.manualChoose(A), out(A, false, true, 'manual-choice'));
});

test('pause and intent-only actions do not strand a same-root listing', () => {
  const p = initial(A, true);
  const content = p.captureContentToken();
  const beforePause = p.captureIntentToken('picker');
  assert.deepEqual(p.pause(), out(A, false, false, 'paused'));
  assert.equal(p.isContentCurrent(content), true);
  assert.equal(p.isIntentCurrent('picker', beforePause), false, 'pause advances intent');

  const picker = p.captureIntentToken('picker');
  assert.deepEqual(p.focusChanged(focus(B)), out(A, false, false, 'following-off'));
  assert.equal(p.isIntentCurrent('picker', picker), true, 'OFF focus changes leave a picker current');
  assert.equal(p.isContentCurrent(content), true);

  assert.deepEqual(p.setFollowing(false, focus(B)), out(A, false, false, 'unchanged'));
  assert.equal(p.isIntentCurrent('picker', picker), true);
  assert.deepEqual(p.manualChoose('relative'), out(A, false, false, 'manual-choice-invalid'));
  assert.equal(p.isIntentCurrent('picker', picker), false);
  assert.equal(p.isContentCurrent(content), true);

  const same = p.captureIntentToken('picker');
  assert.deepEqual(p.manualChoose('c:\\work\\alpha\\'), out(A, false, false, 'manual-choice-same'));
  assert.equal(p.isIntentCurrent('picker', same), false);
  assert.equal(p.isContentCurrent(content), true);

  assert.deepEqual(p.setFollowing(true, { terminalId: null, recordedFolder: B }), out(A, true, false, 'follow-enabled'));
  assert.equal(p.isContentCurrent(content), true, 'non-navigating ON keeps the listing');
  assert.deepEqual(p.pause(), out(A, false, false, 'paused'));
  assert.equal(p.isContentCurrent(content), true, 'a second pause is intent-only');
});

test('refresh preserves state but advances both tokens; tokens belong to one policy', () => {
  const p = initial(A, true);
  const content = p.captureContentToken();
  const intent = p.captureIntentToken('startup');
  const other = initial(A, true);
  assert.equal(other.isContentCurrent(content), false, 'another policy cannot use the content token');
  assert.equal(other.isIntentCurrent('startup', intent), false, 'another policy cannot use the intent token');
  assert.deepEqual(p.refresh(), out(A, true, true, 'refreshed'));
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', intent), false);
  assert.equal(p.state().root, A);
  assert.equal(p.state().following, true);
});

test('dispose invalidates every token, clears state, and later actions are closed', () => {
  const p = initial(A, true);
  const content = p.captureContentToken();
  const startup = p.captureIntentToken('startup');
  const picker = p.captureIntentToken('picker');
  p.dispose();
  assert.deepEqual(p.state(), { root: null, following: false, closed: true });
  assert.equal(p.isContentCurrent(content), false);
  assert.equal(p.isIntentCurrent('startup', startup), false);
  assert.equal(p.isIntentCurrent('picker', picker), false);
  assert.equal(p.isContentCurrent(p.captureContentToken()), false);
  assert.equal(p.isIntentCurrent('startup', p.captureIntentToken('startup')), false);

  const stillContent = p.captureContentToken();
  const stillIntent = p.captureIntentToken('startup');
  const actions = [
    () => p.setFollowing(true, focus(B)),
    () => p.pause(),
    () => p.focusChanged(focus(B)),
    () => p.setInitialRoot(B),
    () => p.manualUp(),
    () => p.manualChoose(B),
    () => p.refresh(),
  ];
  for (const action of actions) assert.deepEqual(action(), out(null, false, false, 'closed'));
  assert.equal(p.captureContentToken(), stillContent, 'closed actions change no content token');
  assert.equal(p.captureIntentToken('startup'), stillIntent, 'closed actions change no intent token');
  p.dispose();
  assert.equal(p.captureContentToken(), stillContent);
  assert.equal(p.captureIntentToken('startup'), stillIntent);
  assert.deepEqual(p.state(), { root: null, following: false, closed: true });
});

test('while there is no root, only actions that set one end a pending startup', () => {
  const keeps = [['toggle on', p => p.setFollowing(true, null)], ['toggle off', p => { p.setFollowing(true, null); p.setFollowing(false, null); }],
    ['pause', p => p.pause()], ['refresh', p => p.refresh()], ['up', p => p.manualUp()], ['bad choice', p => p.manualChoose('relative')],
    ['ineligible focus', p => p.focusChanged(null)]];
  for (const [name, action] of keeps) {
    const p = initial(null, false);
    const startup = p.captureIntentToken('startup');
    action(p);
    assert.equal(p.state().root, null, name);
    assert.equal(p.isIntentCurrent('startup', startup), true, `${name}: startup still supplies the first root`);
  }
  const ends = [['choice', p => p.manualChoose(B)], ['follow on', p => p.setFollowing(true, focus(B))],
    ['focus', p => { p.setFollowing(true, null); p.focusChanged(focus(B)); }], ['initial', p => p.setInitialRoot(B)], ['dispose', p => p.dispose()]];
  for (const [name, action] of ends) {
    const p = initial(null, false);
    const startup = p.captureIntentToken('startup');
    action(p);
    assert.equal(p.isIntentCurrent('startup', startup), false, name);
  }
  const p = initial(null, false), picker = p.captureIntentToken('picker');
  p.pause();
  assert.equal(p.isIntentCurrent('picker', picker), false, 'a pause still ends an older picker');
  const open = p.captureIntentToken('picker');
  p.setInitialRoot(B);
  assert.equal(p.isIntentCurrent('picker', open), true, 'the startup default arriving leaves an open picker current');
});

test('every intent advance invalidates both token kinds', () => {
  const cases = [['toggle', p => p.setFollowing(false, null)], ['pause', p => p.pause()],
    ['focus', p => p.focusChanged(focus(B))], ['up', p => p.manualUp()], ['choose', p => p.manualChoose(B)],
    ['refresh', p => p.refresh()], ['dispose', p => p.dispose()]];
  for (const [name, action] of cases) {
    const p = initial(A, true);
    const startup = p.captureIntentToken('startup');
    const picker = p.captureIntentToken('picker');
    action(p);
    assert.equal(p.isIntentCurrent('startup', startup), false, `${name}: startup`);
    assert.equal(p.isIntentCurrent('picker', picker), false, `${name}: picker`);
  }
});
