// Pure tests of the terminal client: strict reply validation and error sanitizing.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const { createTerminalClient, decodeBase64, isTerminalInfo, sanitizeError, filterPhrase, validAgentName, fmtRemaining, phraseOk } = require(`${root}artifacts/frontend-contract-src/terminals/client.js`);

const info = { id: 'deadbeef', label: 'Terminal 1', state: 'running', persistent: true, cols: 100, rows: 30, host_pid: 4242 };
const client = reply => createTerminalClient(async () => reply);

test('a log reply is validated strictly and no path is ever sent', async () => {
  const seen = [];
  const reply = { op: 'logging', on: true, path: 'C:\\Users\\me\\AppData\\wb\\logs\\deadbeef-20261025T101112Z.log', bytes: 12, stopped_reason: null };
  const c = createTerminalClient(async (command, args) => { seen.push([command, args]); return reply; });
  assert.deepEqual(await c.log('deadbeef', 'start'), { on: true, path: reply.path, bytes: 12, stopped_reason: null });
  await c.log('deadbeef', 'stop');
  await c.log('deadbeef', 'status');
  assert.deepEqual(seen, [['terminal_log', { session: 'deadbeef', action: 'start' }], ['terminal_log', { session: 'deadbeef', action: 'stop' }], ['terminal_log', { session: 'deadbeef', action: 'status' }]]);
  assert.deepEqual(await client({ op: 'logging', on: false, path: null, bytes: 0, stopped_reason: null }).log('deadbeef', 'status'), { on: false, path: null, bytes: 0, stopped_reason: null });
  assert.deepEqual(await client({ ...reply, on: false, stopped_reason: 'the 64 MiB limit was reached' }).log('deadbeef', 'status'), { on: false, path: reply.path, bytes: 12, stopped_reason: 'the 64 MiB limit was reached' });
  for (const bad of [{ ...reply, op: 'ok' }, { ...reply, on: 'yes' }, { ...reply, bytes: -1 }, { ...reply, path: '' }, { ...reply, path: 'x'.repeat(1025) },
    { ...reply, stopped_reason: 5 }, { ...reply, extra: 1 }, { op: 'logging', on: true, path: null, bytes: 0 }, null]) {
    await assert.rejects(client(bad).log('deadbeef', 'status'), e => e.code === 'invalid_response', JSON.stringify(bad));
  }
});

test('terminal info is validated strictly', () => {
  assert.equal(isTerminalInfo(info), true);
  for (const state of ['exited', 'lost', 'unresponsive']) assert.equal(isTerminalInfo({ ...info, state }), true, state);
  for (const bad of [
    { ...info, id: 'DEADBEEF' }, { ...info, id: 'dead' }, { ...info, state: 'zombie' }, { ...info, persistent: 'yes' },
    { ...info, cols: -1 }, { ...info, host_pid: 1.5 }, { ...info, label: 'x'.repeat(257) }, null, [], 'x',
  ]) assert.equal(isTerminalInfo(bad), false, JSON.stringify(bad));
});

test('list, create and read accept only their exact reply shapes', async () => {
  assert.deepEqual(await client({ op: 'sessions', items: [info] }).list(), [info]);
  await assert.rejects(client({ op: 'sessions', items: [{ ...info, state: 'bad' }] }).list(), { code: 'invalid_response' });
  await assert.rejects(client({ op: 'created', session: { id: 'x' } }).create(100, 30, 'a'), { code: 'invalid_response' });
  await assert.rejects(client({ op: 'ok' }).list(), { code: 'invalid_response' });
  const out = await client({ op: 'output', data: 'aGVsbG8=', next: 5, skipped: 0 }).read('deadbeef', 0);
  assert.deepEqual([...out.bytes], [104, 101, 108, 108, 111]);
  assert.equal(out.next, 5);
  for (const bad of [
    { op: 'output', data: '!!!!', next: 1, skipped: 0 }, { op: 'output', data: 'aGVsbG8', next: 1, skipped: 0 },
    { op: 'output', data: 'aGVsbG8=', next: -1, skipped: 0 }, { op: 'output', data: 5, next: 1, skipped: 0 },
  ]) await assert.rejects(client(bad).read('deadbeef', 0), { code: 'invalid_response' }, JSON.stringify(bad));
});

test('base64 decoding rejects malformed input and keeps arbitrary bytes', () => {
  assert.deepEqual([...decodeBase64('AP8=')], [0, 255]);
  assert.equal(decodeBase64('A'), null);
  assert.equal(decodeBase64('AP8=AP8='), null);
});

test('errors from the native side are sanitized and bounded', async () => {
  assert.deepEqual(sanitizeError({ code: 'terminal_failed', user_message: 'm', extra: 'secret' }), { code: 'terminal_failed', message: 'm' });
  assert.equal(sanitizeError(new Error('C:\\Users\\secret\\path boom')).message, 'The terminal service did not respond.');
  assert.equal(sanitizeError({ code: 'x'.repeat(100), user_message: 'y'.repeat(900) }).message.length, 300);
  const failing = createTerminalClient(async () => { throw { code: 'terminal_unavailable', user_message: 'not installed' }; });
  await assert.rejects(failing.list(), { code: 'terminal_unavailable', message: 'not installed' });
});

test('sign-in setting accepts only booleans', async () => {
  assert.equal(await createTerminalClient(async () => true).autostartGet(), true);
  assert.equal(await createTerminalClient(async () => false).autostartSet(false), false);
  await assert.rejects(createTerminalClient(async () => 'yes').autostartGet(), { code: 'invalid_response' });
  const seen = [];
  await createTerminalClient(async (c, a) => { seen.push([c, a]); return true; }).autostartSet(true);
  assert.deepEqual(seen, [['terminal_autostart_set', { enabled: true }]]);
});

test('creating a terminal sends the history choice and accepts the reported size', async () => {
  const seen = [];
  const c = createTerminalClient(async (command, args) => { seen.push([command, args]); return { op: 'created', session: { ...info, history_mib: 32 } }; });
  const made = await c.create(100, 30, 'T', true);
  assert.equal(made.history_mib, 32);
  assert.deepEqual(seen, [['terminal_new', { cols: 100, rows: 30, label: 'T', extendedHistory: true }]]);
  assert.equal(isTerminalInfo({ ...info, history_mib: 99999 }), false, 'absurd history size is rejected');
  assert.equal(isTerminalInfo({ ...info, history_mib: 'big' }), false);
  assert.equal(isTerminalInfo(info), true, 'older services omit the field');
});

test('agent access replies are validated and requests carry exact arguments', async () => {
  const draft = { agent: 'claude', text: 'ls -la', multiline: false };
  assert.deepEqual(await createTerminalClient(async () => ({ op: 'draft', draft })).draft('deadbeef'), { draft });
  assert.deepEqual(await createTerminalClient(async () => ({ op: 'draft', draft: null })).draft('deadbeef'), { draft: null });
  for (const bad of [{ op: 'draft', draft: { agent: 'a', text: 5, multiline: false } }, { op: 'draft', draft: { agent: 'a', text: 'x' } }, { op: 'grants', items: [] }, { draft }])
    await assert.rejects(createTerminalClient(async () => bad).draft('deadbeef'), { code: 'invalid_response' });
  const items = [{ agent: 'claude', remaining_ms: 5000, keep_after_detach: false }];
  assert.deepEqual(await createTerminalClient(async () => ({ op: 'grants', items })).grants('deadbeef'), items);
  await assert.rejects(createTerminalClient(async () => ({ op: 'grants', items: [{ agent: 'a', remaining_ms: -1, keep_after_detach: false }] })).grants('deadbeef'), { code: 'invalid_response' });
  const seen = [];
  const c = createTerminalClient(async (command, args) => { seen.push([command, args]); return undefined; });
  await c.grantCreate('claude', 'deadbeef', 'one_hour', true, 'Ab1!');
  await c.grantRevoke('claude', 'deadbeef');
  await c.gateSet('old', 'new');
  await c.draftAccept('deadbeef');
  await c.draftDiscard('deadbeef');
  assert.deepEqual(seen, [
    ['terminal_grant_create', { agent: 'claude', session: 'deadbeef', duration: 'one_hour', keepAfterDetach: true, gate: 'Ab1!' }],
    ['terminal_grant_revoke', { agent: 'claude', session: 'deadbeef' }],
    ['terminal_gate_set', { current: 'old', new: 'new' }],
    ['terminal_draft_accept', { session: 'deadbeef' }],
    ['terminal_draft_discard', { session: 'deadbeef' }],
  ]);
});

test('phrase, agent-name and countdown helpers', () => {
  assert.equal(filterPhrase('a b-c_D9!@#$%^&*()é\n'), 'abcD9!@#$%^&*()');
  assert.equal(filterPhrase('x'.repeat(300)).length, 256);
  assert.equal(phraseOk('Ab1!'), true);
  assert.equal(phraseOk('a b'), false);
  assert.equal(phraseOk(''), true);
  assert.equal(validAgentName('claude-3.5_x'), true);
  for (const bad of ['', 'has space', 'x'.repeat(65), 'é']) assert.equal(validAgentName(bad), false, bad);
  assert.equal(fmtRemaining(45_000), '45 s');
  assert.equal(fmtRemaining(5 * 60_000), '5 min');
  assert.equal(fmtRemaining(3 * 3_600_000), '3 h');
  assert.equal(fmtRemaining(2 * 86_400_000), '2 d');
});

test('commands go to the expected native names with exact arguments', async () => {
  const seen = [];
  const c = createTerminalClient(async (command, args) => { seen.push([command, args]); return undefined; });
  await c.input('deadbeef', 'ls\r');
  await c.resize('deadbeef', 90, 25);
  await c.end('deadbeef');
  assert.deepEqual(seen, [
    ['terminal_input', { session: 'deadbeef', text: 'ls\r' }],
    ['terminal_resize', { session: 'deadbeef', cols: 90, rows: 25 }],
    ['terminal_end', { session: 'deadbeef' }],
  ]);
});
