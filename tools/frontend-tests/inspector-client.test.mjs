// Pure tests of the inspector client: strict reply validation, formatting and sorting.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const { createInspectorClient, fmtAgo, fmtDuration, isMapPayload, isSnapshot, isSource, isTags, isPeek, sanitizeError, share, sortTools, toMapFacts } =
  require(`${root}artifacts/frontend-contract-src/inspector/client.js`);

const id = '0123456789abcdef';
const source = { id, agent: 'claude', label: 'proj', modified_ms: 5, bytes: 10, active: true, running: null, model: 'claude-opus-5', last_line: 'Done.', folder: 'proj', kind: 'session', parent: null };
const subagent = { ...source, id: 'abcdef0123456789', kind: 'subagent', parent: id, running: null };
const tool = (name, over = {}) => ({ name, calls: 2, completed: 2, errors: 0, unknown_outcome: 0, total_ms: 3000, max_ms: 2000, median_ms: 1000, last_used_ms: 9000, ...over });
const snap = (over = {}) => ({
  state: 'working', first_event_ms: 1, last_event_ms: 2, turns: 1, open_calls: 0,
  usage: { input: 1, output: 2, cache_read: null, cache_write: null, reasoning: null, records: 1 },
  breakdown: { tool_ms: 1, model_ms: 2, human_ms: 3 }, tools: [tool('Bash')], tools_omitted: 0,
  recent: [{ tool: 'Bash', started_ms: 1, duration_ms: 2, ok: null }], timeline: { start_ms: 1, bucket_ms: 60000, buckets: [{ calls: 1, errors: 0, tool_ms: 5 }] }, orphan_results: 0, ...over,
});
const reply = (over = {}) => ({ source: { id, agent: 'pi', label: 'p', modified_ms: 1, bytes: 2 }, now_ms: 100, snapshot: snap(), peek: { last_line: 'Done.', model: null }, ...over });
const client = r => createInspectorClient(async () => r);

test('a full list of up to 128 sources is accepted, more is not (the native list can hold 90)', async () => {
  const many = Array.from({ length: 90 }, (_, i) => ({ ...source, id: i.toString(16).padStart(16, '0') }));
  assert.equal((await client(many).sources()).length, 90);
  assert.equal((await client(Array.from({ length: 128 }, () => source)).sources()).length, 128);
  await assert.rejects(client(Array.from({ length: 129 }, () => source)).sources(), { code: 'invalid_response' });
});

test('sources are validated strictly', async () => {
  assert.equal(isSource(source), true);
  for (const bad of [{ ...source, id: 'short' }, { ...source, agent: 'gpt' }, { ...source, bytes: -1 }, { ...source, active: 'yes' }, null,
    { ...source, running: 'maybe' }, { ...source, model: 5 }, { ...source, last_line: 'x'.repeat(221) }, { ...source, folder: 'x'.repeat(101) }, { ...source, running: undefined },
    { ...source, kind: 'agent' }, { ...source, kind: undefined }, { ...source, parent: undefined }, { ...source, parent: 'short' }, { ...source, parent: 5 }])
    assert.equal(isSource(bad), false);
  assert.equal(isSource(subagent), true, 'a subagent with its parent id is a valid source');
  assert.equal(isSource({ ...subagent, parent: null }), true, 'a subagent whose parent is not on disk has a null parent');
  const { kind: _kind, parent: _parent, ...old } = source;
  assert.equal(isSource(old), false, 'a payload without kind and parent is rejected, not defaulted');
  assert.deepEqual(await client([source, subagent]).sources(), [source, subagent]);
  assert.deepEqual(await client([source]).sources(), [source]);
  await assert.rejects(client([{ ...source, id: 'zz' }]).sources(), { code: 'invalid_response' });
  await assert.rejects(client({ items: [] }).sources(), { code: 'invalid_response' });
});

test('snapshots reject malformed, oversized or mismatched replies', async () => {
  assert.equal(isSnapshot(snap()), true);
  assert.equal(isSnapshot(snap({ timeline: null })), true, 'no timeline is allowed');
  for (const bad of [
    snap({ state: 'sleeping' }), snap({ turns: -1 }), snap({ tools: [tool('x', { calls: 'many' })] }),
    snap({ tools: Array.from({ length: 101 }, (_, i) => tool('t' + i)) }), snap({ recent: Array.from({ length: 51 }, () => ({ tool: 'x', started_ms: 1, duration_ms: 1, ok: true })) }),
    snap({ usage: { input: 1 } }), snap({ breakdown: { tool_ms: 1 } }),
    snap({ timeline: { start_ms: 1, bucket_ms: 0, buckets: [{ calls: 1, errors: 0, tool_ms: 1 }] } }),
    snap({ timeline: { start_ms: 1, bucket_ms: 60000, buckets: [] } }),
    snap({ timeline: { start_ms: 1, bucket_ms: 60000, buckets: Array.from({ length: 49 }, () => ({ calls: 1, errors: 0, tool_ms: 1 })) } }),
    snap({ timeline: { start_ms: 1, bucket_ms: 60000, buckets: [{ calls: 1, errors: 'x', tool_ms: 1 }] } }),
  ]) assert.equal(isSnapshot(bad), false, JSON.stringify(bad).slice(0, 80));
  assert.equal((await client(reply()).snapshot(id)).now_ms, 100);
  await assert.rejects(client(reply({ source: { id: 'fedcba9876543210', agent: 'pi', label: 'p', modified_ms: 1, bytes: 2 } })).snapshot(id), { code: 'invalid_response' }, 'reply for another source');
  await assert.rejects(client(reply({ snapshot: snap({ state: 'x' }) })).snapshot(id), { code: 'invalid_response' });
  await assert.rejects(client(reply()).snapshot('not-an-id'), { code: 'inspector_failed' }, 'bad id never reaches native');
});

test('native errors are sanitized and the right commands are called', async () => {
  assert.deepEqual(sanitizeError({ code: 'inspector_failed', user_message: 'm', extra: 'secret' }), { code: 'inspector_failed', message: 'm' });
  assert.equal(sanitizeError(new Error('C:\\Users\\secret boom')).message, 'The inspector did not respond.');
  const seen = [];
  const c = createInspectorClient(async (command, args) => { seen.push([command, args]); return command === 'inspector_sources' ? [] : reply(); });
  await c.sources();
  await c.snapshot(id);
  assert.deepEqual(seen, [['inspector_sources', undefined], ['inspector_snapshot', { id }]]);
});

test('durations, ago text and shares read naturally and never invent values', () => {
  assert.equal(fmtDuration(null), 'unknown');
  assert.equal(fmtDuration(450), '450 ms');
  assert.equal(fmtDuration(2500), '2.5 s');
  assert.equal(fmtDuration(42_000), '42 s');
  assert.equal(fmtDuration(125_000), '2 min 5 s');
  assert.equal(fmtDuration(3_900_000), '1 h 5 min');
  assert.equal(fmtAgo(null, 10), 'never');
  assert.equal(fmtAgo(9_000, 10_000), 'just now');
  assert.equal(fmtAgo(0, 125_000), '2 min 5 s ago');
  assert.equal(share(1, 4), '25%');
  assert.equal(share(1, 0), '0%');
});

test('sorting is stable, handles unknown values and does not mutate its input', () => {
  const rows = [tool('b', { total_ms: 10, median_ms: null }), tool('a', { total_ms: 10 }), tool('c', { total_ms: 99, median_ms: 5 })];
  const copy = JSON.stringify(rows);
  assert.deepEqual(sortTools(rows, 'total_ms', true).map(r => r.name), ['c', 'a', 'b'], 'ties fall back to the name');
  assert.deepEqual(sortTools(rows, 'name', false).map(r => r.name), ['a', 'b', 'c']);
  assert.deepEqual(sortTools(rows, 'median_ms', false).map(r => r.name), ['b', 'c', 'a'], 'unknown sorts lowest');
  assert.equal(JSON.stringify(rows), copy);
});

test('colour tags and the peek are validated, and saving only sends valid tags', async () => {
  assert.equal(isTags({ version: 1, tags: {} }), true);
  assert.equal(isTags({ version: 1, tags: { [id]: 'green' } }), true);
  for (const bad of [null, {}, { version: 2, tags: {} }, { version: 1 }, { version: 1, tags: { [id]: 'pink' } }, { version: 1, tags: { nope: 'red' } }, { version: 1, tags: {}, extra: 1 }, { version: 1, tags: [] }])
    assert.equal(isTags(bad), false, JSON.stringify(bad));
  assert.equal(isTags({ version: 1, tags: Object.fromEntries(Array.from({ length: 257 }, (_, i) => [i.toString(16).padStart(16, '0'), 'red'])) }), false);
  assert.equal(isPeek({ last_line: null, model: null }), true);
  assert.equal(isPeek({ last_line: 'x'.repeat(221), model: null }), false);
  const seen = [];
  const c = createInspectorClient(async (command, args) => { seen.push([command, args]); return command === 'inspector_tags_load' ? { version: 1, tags: {} } : args.tags; });
  assert.deepEqual(await c.tags(), { version: 1, tags: {} });
  const tags = { version: 1, tags: { [id]: 'blue' } };
  assert.deepEqual(await c.saveTags(tags), tags);
  await assert.rejects(c.saveTags({ version: 1, tags: { [id]: 'pink' } }), { code: 'inspector_failed' });
  assert.deepEqual(seen.map(x => x[0]), ['inspector_tags_load', 'inspector_tags_save'], 'an invalid tag never reaches native');
  await assert.rejects(client(reply({ peek: { last_line: 5, model: null } })).snapshot(id), { code: 'invalid_response' });
});

const mapPayload = (over = {}) => ({
  generated_ms: 5,
  sessions: [{ id, open_calls: 1, open_tools: ['Bash'], tools: [{ name: 'Bash', calls: 3, errors: 1 }] }, { id: 'fedcba9876543210', open_calls: 0, open_tools: [], tools: [] }],
  reports: [{ manager: id, report: 'fedcba9876543210' }],
  ...over
});

test('the map payload is validated strictly and turned into per-source facts', async () => {
  assert.equal(isMapPayload(mapPayload()), true);
  assert.equal(isMapPayload(mapPayload({ sessions: [], reports: [] })), true);
  const tool = { name: 'Bash', calls: 1, errors: 0 };
  for (const bad of [
    null, [], mapPayload({ generated_ms: -1 }), mapPayload({ extra: 1 }), mapPayload({ sessions: 'x' }),
    mapPayload({ sessions: [{ id: 'short', open_calls: 0, open_tools: [], tools: [] }] }),
    mapPayload({ sessions: [{ id, open_calls: -1, open_tools: [], tools: [] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: [], tools: [{ ...tool, calls: 1.5 }] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: [], tools: [{ ...tool, name: 'x'.repeat(201) }] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: [], tools: [{ ...tool, path: 'C:/secret' }] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: [], tools: Array.from({ length: 101 }, () => tool) }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, tools: [] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: 'Bash', tools: [] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: Array.from({ length: 9 }, () => 'Bash'), tools: [] }] }),
    mapPayload({ sessions: [{ id, open_calls: 0, open_tools: [5], tools: [] }] }),
    mapPayload({ sessions: Array.from({ length: 129 }, () => ({ id, open_calls: 0, open_tools: [], tools: [] })) }),
    mapPayload({ reports: [{ manager: id }] }),
    mapPayload({ reports: [{ manager: id, report: 'nope' }] }),
    mapPayload({ reports: Array.from({ length: 257 }, () => ({ manager: id, report: id })) })
  ]) assert.equal(isMapPayload(bad), false, JSON.stringify(bad).slice(0, 80));
  assert.deepEqual(toMapFacts(mapPayload()), { [id]: { tools: [{ name: 'Bash', calls: 3, errors: 1 }], open_calls: 1, open_tools: ['Bash'] }, fedcba9876543210: { tools: [], open_calls: 0, open_tools: [] } });
  assert.deepEqual(await client(mapPayload()).map(), mapPayload());
  await assert.rejects(client({ nope: true }).map(), { code: 'invalid_response' });
});
