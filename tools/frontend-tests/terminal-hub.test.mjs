// The shared output watcher: subscribers get output matched by cursor, fall back when watching fails or a session
// is gone, and many subscriptions in one turn share one watch.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const { createOutputHub } = require(`${root}artifacts/frontend-contract-src/terminals/hub.js`);

const A = '0000000a', B = '0000000b';
const bytes = text => new TextEncoder().encode(text);
const turn = () => new Promise(resolve => setTimeout(resolve, 5));
const until = async (done, what) => {
  for (let i = 0; i < 400; i++) { if (done()) return; await turn(); }
  assert.fail(`timed out: ${what}`);
};

// Every hub made by a test is stopped afterwards so no watch is left waiting.
const open = [];
test.afterEach(() => { for (const hub of open.splice(0)) hub.stopAll(); });

/** A fake service: each session has an output string; `watch` answers when a cursor is behind, or after the wait. */
function setup() {
  const state = { out: { [A]: '', [B]: '' }, calls: [], clock: 0, hidden: false, fail: null, sessions: new Set([A, B]) };
  const deps = {
    watch: async (cursors, waitMs) => {
      state.calls.push(cursors.map(c => ({ ...c })));
      if (state.fail) throw state.fail;
      for (let i = 0; i < waitMs / 5; i++) {
        const items = [];
        const gone = [];
        for (const c of cursors) {
          if (!state.sessions.has(c.session)) { gone.push(c.session); continue; }
          const all = bytes(state.out[c.session]);
          if (all.length > c.from) items.push({ session: c.session, from: c.from, bytes: all.slice(c.from), next: all.length, skipped: 0 });
        }
        if (items.length || gone.length) return { items, gone };
        await turn();
      }
      return { items: [], gone: [] };
    },
    hidden: () => state.hidden,
    sleep: ms => new Promise(resolve => setTimeout(resolve, Math.min(ms, 5))),
    now: () => state.clock,
  };
  const hub = createOutputHub(deps);
  open.push(hub);
  return { state, hub };
}
const collector = () => {
  const got = { text: '', next: 0, falls: [] };
  return { got, sub: { onOutput: (b, next) => { got.text += new TextDecoder().decode(b); got.next = next; }, onFallback: why => got.falls.push(why) } };
};

test('output reaches the subscriber from its cursor, in order, and only once', async () => {
  const { state, hub } = setup();
  state.out[A] = 'hello ';
  const c = collector();
  const stop = hub.subscribe(A, 0, c.sub);
  assert.ok(stop);
  await until(() => c.got.text === 'hello ', 'first output');
  state.out[A] += 'world';
  await until(() => c.got.text === 'hello world', 'second output');
  assert.equal(c.got.next, 11);
  stop();
});

test('a subscriber that starts partway gets only what follows its cursor', async () => {
  const { state, hub } = setup();
  state.out[A] = 'abcdef';
  const c = collector();
  hub.subscribe(A, 3, c.sub);
  await until(() => c.got.text === 'def', 'tail');
});

test('subscriptions made in the same turn share one watch, which covers all of them', async () => {
  const { state, hub } = setup();
  const a = collector();
  const b = collector();
  hub.subscribe(A, 0, a.sub);
  hub.subscribe(B, 0, b.sub);
  state.out[B] = 'bee';
  await until(() => b.got.text === 'bee', 'B output');
  assert.deepEqual(state.calls[0].map(c => c.session).sort(), [A, B]);
  assert.ok(state.calls.every(c => c.length === 2), 'every watch covered both, none was started for one alone');
});

test('a later subscription is covered by a new watch without waiting for the old one to time out', async () => {
  const { state, hub } = setup();
  const a = collector();
  hub.subscribe(A, 0, a.sub);
  await until(() => state.calls.length === 1, 'first watch');
  state.out[B] = 'late';
  const b = collector();
  hub.subscribe(B, 0, b.sub);
  await until(() => b.got.text === 'late', 'late subscriber served');
});

test('two subscribers on one session each get the output, at their own cursors', async () => {
  const { state, hub } = setup();
  state.out[A] = '12345';
  const x = collector();
  const y = collector();
  hub.subscribe(A, 0, x.sub);
  hub.subscribe(A, 2, y.sub);
  await until(() => x.got.text === '12345' && y.got.text === '345', 'both served');
});

test('after unsubscribing nothing more is delivered', async () => {
  const { state, hub } = setup();
  const c = collector();
  const stop = hub.subscribe(A, 0, c.sub);
  state.out[A] = 'one';
  await until(() => c.got.text === 'one', 'delivered');
  stop();
  state.out[A] += 'two';
  await new Promise(resolve => setTimeout(resolve, 60));
  assert.equal(c.got.text, 'one');
});

test('a session that is gone tells its subscriber to fall back, once', async () => {
  const { state, hub } = setup();
  const c = collector();
  hub.subscribe(A, 0, c.sub);
  state.sessions.delete(A);
  await until(() => c.got.falls.length === 1, 'fallback');
  assert.deepEqual(c.got.falls, ['gone']);
  await new Promise(resolve => setTimeout(resolve, 30));
  assert.equal(c.got.falls.length, 1);
});

test('three failures in a row switch watching off: subscribers fall back and new ones are refused for a while, then it comes back', async () => {
  const { state, hub } = setup();
  state.fail = { code: 'terminal_unavailable', message: 'no' };
  const c = collector();
  assert.ok(hub.subscribe(A, 0, c.sub));
  await until(() => c.got.falls.length === 1, 'fallback after failures');
  assert.deepEqual(c.got.falls, ['unavailable']);
  assert.equal(hub.subscribe(A, 0, collector().sub), null, 'refused while switched off');
  state.fail = null;
  state.clock += 31_000;
  const d = collector();
  state.out[A] = 'back';
  assert.ok(hub.subscribe(A, 0, d.sub), 'allowed again after the pause');
  await until(() => d.got.text === 'back', 'works again');
});

test('a busy answer is retried without counting as a failure', async () => {
  const { state, hub } = setup();
  state.fail = { code: 'watch_busy', message: 'busy' };
  const c = collector();
  hub.subscribe(A, 0, c.sub);
  await new Promise(resolve => setTimeout(resolve, 80));
  assert.deepEqual(c.got.falls, [], 'still subscribed');
  state.fail = null;
  state.out[A] = 'ok';
  await until(() => c.got.text === 'ok', 'delivered after busy');
});

test('nothing is asked of the service while the page is hidden', async () => {
  const { state, hub } = setup();
  state.hidden = true;
  const c = collector();
  hub.subscribe(A, 0, c.sub);
  state.out[A] = 'x';
  await new Promise(resolve => setTimeout(resolve, 60));
  assert.equal(state.calls.length, 0);
  state.hidden = false;
  await until(() => c.got.text === 'x', 'delivered when visible');
});

test('more than 64 subscribers are not taken: the 65th polls instead', () => {
  const { hub } = setup();
  for (let i = 0; i < 64; i++) assert.ok(hub.subscribe(A, i, collector().sub));
  assert.equal(hub.subscribe(A, 99, collector().sub), null);
});
