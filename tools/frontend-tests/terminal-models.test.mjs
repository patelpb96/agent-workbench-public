// The model picker's pure rules: what it lists, how favourites are kept, and the one line it types.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const { modelCommand, validModelName, visibleModels, parseFavourites, toggleFavourite, shortModelName } = require(`${root}artifacts/frontend-contract-src/terminals/models.js`);

test('the command is /model plus a single safe token, never a newline', () => {
  assert.equal(modelCommand('opus'), '/model opus');
  assert.equal(modelCommand('claude-opus-5-5'), '/model claude-opus-5-5');
  assert.equal(modelCommand(''), '/model');
  for (const bad of ['a b', 'x\n', 'x\r', 'x;y', '-flag', '$(x)', 'a'.repeat(81)]) assert.equal(modelCommand(bad), null, JSON.stringify(bad));
  assert.equal(validModelName('gpt-6'), true);
});

test('search matches names and notes in any case, favourites come first, and a valid typed name is offered', () => {
  const names = list => list.map(m => m.name);
  assert.ok(names(visibleModels('claude', 'SON', [])).includes('sonnet'));
  assert.ok(names(visibleModels('claude', 'cheapest', [])).includes('haiku'));
  assert.deepEqual(names(visibleModels('claude', '', ['haiku'])).slice(0, 1), ['haiku']);
  assert.deepEqual(names(visibleModels('claude', 'my-model_1', [])), ['my-model_1']);
  assert.deepEqual(names(visibleModels('claude', 'bad name', [])), []);
  assert.deepEqual(names(visibleModels('pi', '', [])), ['']);
});

test('favourites are per agent, toggle on and off, and stored text that is wrong is dropped', () => {
  let f = toggleFavourite({}, 'claude', 'opus');
  f = toggleFavourite(f, 'codex', 'gpt-6');
  assert.deepEqual(f, { claude: ['opus'], codex: ['gpt-6'] });
  assert.deepEqual(toggleFavourite(f, 'claude', 'opus').claude, []);
  assert.deepEqual(parseFavourites(JSON.stringify(f)), f);
  assert.deepEqual(parseFavourites('not json'), {});
  assert.deepEqual(parseFavourites('[1]'), {});
  assert.deepEqual(parseFavourites(null), {});
  assert.deepEqual(parseFavourites(JSON.stringify({ claude: ['ok', 5, 'bad name', 'ok'], other: ['x'] })), { claude: ['ok'] });
});

test('the bar shortens a model name without hiding which family it is', () => {
  assert.equal(shortModelName(null), '—');
  assert.equal(shortModelName('claude-sonnet-5-5'), 'sonnet-5-5');
  assert.equal(shortModelName('claude-opus-5-5-20260101'), 'opus-5-5');
  assert.equal(shortModelName('models/gemini-4-pro'), 'gemini-4-pro');
  assert.equal(shortModelName('gpt-6.1-sol'), 'gpt-6.1-sol');
});
