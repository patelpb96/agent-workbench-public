// Pure tests of the "where is everything" client.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const { createLocationsClient, isLocation } = require(`${root}artifacts/frontend-contract-src/settings/locations.js`);

const place = (over = {}) => ({ id: 'app_data', label: 'App data', path: 'C:\\Users\\me\\data', detail: '2 KB', file: false, ...over });

test('a place has exactly the expected shape and sizes', () => {
  assert.equal(isLocation(place()), true);
  assert.equal(isLocation(place({ detail: '' })), true);
  for (const bad of [null, [], {}, { ...place(), extra: 1 }, place({ id: 'App' }), place({ id: '../x' }), place({ id: '' }), place({ label: '' }), place({ path: '' }),
    place({ path: 'x'.repeat(1025) }), place({ detail: 'x'.repeat(121) }), place({ file: 'yes' }), place({ id: 5 })]) {
    assert.equal(isLocation(bad), false, JSON.stringify(bad));
  }
});

test('listing validates the whole reply; opening sends only a valid id', async () => {
  const seen = [];
  const client = reply => createLocationsClient(async (command, args) => { seen.push([command, args]); if (reply instanceof Error) throw reply; return reply; });
  assert.deepEqual(await client([place(), place({ id: 'journal' })]).list(), [place(), place({ id: 'journal' })]);
  await assert.rejects(client([place(), place()]).list(), { code: 'invalid_response' }, 'duplicate ids');
  await assert.rejects(client([place(), { nope: 1 }]).list(), { code: 'invalid_response' }, 'one bad row spoils the list');
  await assert.rejects(client({ items: [] }).list(), { code: 'invalid_response' });
  await assert.rejects(client(Array.from({ length: 33 }, (_, i) => place({ id: 'p' + 'a'.repeat(i % 5) + (i > 4 ? 'b'.repeat(i) : '') }))).list(), { code: 'invalid_response' });
  seen.length = 0;
  const c = client(null);
  await c.open('journal');
  assert.deepEqual(seen, [['locations_open', { id: 'journal' }]]);
  seen.length = 0;
  await assert.rejects(c.open('C:\\Windows'), { code: 'locations_failed' });
  await assert.rejects(c.open('../x'), { code: 'locations_failed' });
  assert.equal(seen.length, 0, 'a path-like id never reaches native');
  const failing = client(Object.assign(new Error('x'), { code: 'locations_failed', user_message: 'That place is not available on this machine.' }));
  await assert.rejects(failing.open('journal'), { message: 'That place is not available on this machine.' });
});
