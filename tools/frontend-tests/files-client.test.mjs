// Pure tests of the Files panel client and its helpers.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const f = require(`${root}artifacts/frontend-contract-src/files/files.js`);

const listing = { path: 'C:\\Users\\me', entries: [{ name: 'src', kind: 'folder', size: 0, modified_ms: 0 }, { name: 'a.txt', kind: 'file', size: 5, modified_ms: 1 }], more: 0, hidden: 1 };
const clientWith = reply => f.createFilesClient(async (command, args) => { if (reply instanceof Error) throw reply; return typeof reply === 'function' ? reply(command, args) : reply; });

test('only a listing of exactly the expected shape is accepted', () => {
  assert.equal(f.isListing(listing), true);
  for (const bad of [null, [], { ...listing, extra: 1 }, { ...listing, path: 'relative' }, { ...listing, more: -1 },
    { ...listing, entries: [{ name: '..', kind: 'folder', size: 0, modified_ms: 0 }] },
    { ...listing, entries: [{ name: 'a\\b', kind: 'file', size: 0, modified_ms: 0 }] },
    { ...listing, entries: [{ name: 'a', kind: 'device', size: 0, modified_ms: 0 }] },
    { ...listing, entries: [{ name: 'a', kind: 'file', size: 1.5, modified_ms: 0 }] },
    { ...listing, entries: Array.from({ length: f.maxEntries + 1 }, (_, i) => ({ name: `f${i}`, kind: 'file', size: 0, modified_ms: 0 })) }]) {
    assert.equal(f.isListing(bad), false, JSON.stringify(bad).slice(0, 80));
  }
});

test('paths join, go up and are quoted for a terminal', () => {
  assert.equal(f.joinPath('C:\\a', 'b'), 'C:\\a\\b');
  assert.equal(f.joinPath('C:\\', 'b'), 'C:\\b');
  assert.equal(f.parentPath('C:\\a\\b'), 'C:\\a');
  assert.equal(f.parentPath('C:\\a'), 'C:\\');
  assert.equal(f.parentPath('C:\\'), null);
  assert.equal(f.quotedPath('C:\\my project\\x'), '"C:\\my project\\x"');
  assert.equal(f.quotedPath('C:\\a\nb'), null, 'never a newline: inserting must not run anything');
  assert.equal(f.sizeText(512), '512 B');
  assert.equal(f.sizeText(2048), '2.0 KB');
  assert.equal(f.sizeText(50 * 1024 * 1024), '50 MB');
});

test('the client checks the path before asking and the reply after', async () => {
  const calls = [];
  const client = clientWith((command, args) => { calls.push([command, args]); return command === 'files_home' ? 'C:\\Users\\me' : listing; });
  assert.deepEqual(await client.list('C:\\Users\\me', false), listing);
  assert.deepEqual(calls, [['files_list', { path: 'C:\\Users\\me', showHidden: false }]]);
  await assert.rejects(client.list('relative', false), e => e.code === 'files_invalid');
  await assert.rejects(client.list('C:\\Other', false), e => e.code === 'invalid_response', 'a listing of another folder is refused');
  assert.equal(await client.home(), 'C:\\Users\\me');
  await assert.rejects(clientWith('nope').home(), e => e.code === 'invalid_response');
  await assert.rejects(client.reveal('..\\x'), e => e.code === 'files_invalid');
  const odd = await clientWith(new Error('stack with C:\\secret')).list('C:\\x', false).catch(e => e);
  assert.ok(!JSON.stringify(odd).includes('secret'));
});

test('pins toggle, ignore case, stay under the cap and survive only in a valid shape', () => {
  const a = { path: 'C:\\work\\a', folder: true };
  let pins = f.togglePin([], a);
  assert.deepEqual(pins, [a]);
  assert.equal(f.isPinned(pins, 'c:\\WORK\\A'), true);
  pins = f.togglePin(pins, { path: 'C:\\WORK\\A', folder: true });
  assert.deepEqual(pins, [], 'the same path in another case unpins');
  const full = Array.from({ length: f.maxPins }, (_, i) => ({ path: `C:\\p${i}`, folder: false }));
  assert.equal(f.togglePin(full, { path: 'C:\\more', folder: false }).length, f.maxPins);
  assert.deepEqual(f.parsePins([a, a, { path: 'relative', folder: true }, { path: 'C:\\x', folder: 'yes' }, null, { path: 'C:\\b', folder: false }]), [a, { path: 'C:\\b', folder: false }]);
  assert.deepEqual(f.parsePins('nope'), []);
  assert.equal(f.baseName('C:\\work\\a'), 'a');
  assert.equal(f.baseName('C:\\'), 'C:');
});
