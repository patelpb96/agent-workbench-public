// Pure tests of the agent-terminal client and its validators.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const a = require(`${root}artifacts/frontend-contract-src/terminals/agents.js`);

const info = { id: 'deadbeef', label: 'Claude Code · proj', state: 'running', persistent: true, cols: 120, rows: 30, host_pid: 7 };
const clientWith = reply => a.createAgentClient(async () => { if (reply instanceof Error) throw reply; return reply; });

test('the folder form rejects what the native side would refuse', () => {
  for (const ok of ['C:\\Users\\me\\proj', 'd:\\work\\my proj', 'C:\\Program Files (x86)\\x', 'C:\\Users\\café']) assert.equal(a.folderProblem(ok), null, ok);
  for (const bad of ['', '   ', 'relative\\dir', '/c/Users', '\\\\server\\share', "C:\\it's", 'C:\\a\u2018b', 'C:\\a\u2019b', 'C:\\a\nb', 'C:\\a"b', 'C:\\a`b', 'C:\\a|b', 'C:\\x\\..\\y', 'C:\\' + 'a'.repeat(250)]) {
    assert.notEqual(a.folderProblem(bad), null, JSON.stringify(bad));
  }
});

test('launch and list replies are validated; anything else is refused', async () => {
  const launched = await clientWith({ terminal: info, agent: 'claude' }).launch('claude', 'C:\\x');
  assert.equal(launched.agent, 'claude');
  assert.equal(launched.terminal.id, 'deadbeef');
  for (const bad of [null, {}, { terminal: info, agent: 'bash' }, { terminal: { ...info, id: 'NOPE' }, agent: 'pi' }, { terminal: info }]) {
    await assert.rejects(clientWith(bad).launch('pi', 'C:\\x'), e => e.code === 'invalid_response', JSON.stringify(bad));
  }
  const entry = { terminal: 'deadbeef', agent: 'codex', cwd: 'C:\\x', launched_ms: 1792000000000 };
  assert.deepEqual(await clientWith([entry]).list(), [entry]);
  for (const bad of [[{ ...entry, agent: 'x' }], [{ ...entry, terminal: 'zz' }], [{ ...entry, launched_ms: -1 }], [{ ...entry, extra: 1, cwd: 5 }], 'x', Array.from({ length: 65 }, () => entry)]) {
    await assert.rejects(clientWith(bad).list(), e => e.code === 'invalid_response', JSON.stringify(bad).slice(0, 60));
  }
});

test('a link is either pending or linked to an opaque 16-hex source, nothing else', async () => {
  assert.deepEqual(await clientWith({ state: 'pending', agent: 'pi' }).link('deadbeef'), { state: 'pending', agent: 'pi' });
  const linked = { state: 'linked', agent: 'claude', source: '0123456789abcdef' };
  assert.deepEqual(await clientWith(linked).link('deadbeef'), linked);
  for (const bad of [{ state: 'linked', agent: 'claude' }, { state: 'linked', agent: 'claude', source: 'C:\\secret\\t.jsonl' }, { state: 'linked', agent: 'claude', source: '0123' },
    { state: 'pending', agent: 'claude', source: '0123456789abcdef' }, { state: 'done', agent: 'claude' }, { state: 'pending', agent: 'nope' }, null]) {
    await assert.rejects(clientWith(bad).link('deadbeef'), e => e.code === 'invalid_response', JSON.stringify(bad));
  }
});

test('native errors come back sanitized with their code and message', async () => {
  const err = await clientWith(Object.assign(new Error('x'), { code: 'agent_failed', user_message: 'That folder does not exist.' })).launch('codex', 'C:\\nope').catch(e => e);
  assert.deepEqual(err, { code: 'agent_failed', message: 'That folder does not exist.' });
  const odd = await clientWith(new Error('stack trace with C:\\secret')).launch('codex', 'C:\\x').catch(e => e);
  assert.equal(odd.code, 'terminal_failed');
  assert.ok(!JSON.stringify(odd).includes('secret'));
});

test('labels and choices cover exactly the three agents', () => {
  assert.deepEqual(a.agentChoices.map(c => c.id), ['claude', 'pi', 'codex']);
  assert.equal(a.agentLabel('pi'), 'Pi');
  assert.equal(a.agentLabel('zzz'), 'Agent');
});
