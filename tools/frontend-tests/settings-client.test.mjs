// Pure tests of the settings client and its validators (the Properties window).
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const s = require(`${root}artifacts/frontend-contract-src/settings/settings.js`);

const good = { version: 1, terminals: { start_folder: 'C:\\Users\\me', extended_history: true }, agents: { launch_folder: 'C:\\work' }, appearance: { sidebar1_opacity: 40, sidebar2_opacity: 100 },
  window: { titlebar_autohide: false, canvas_mode: 'transparent', canvas_color: '#A0b1c2', canvas_alpha: 35, see_through: true }, keys: { prefix: 'ctrl+a' } };
const clientWith = reply => s.createSettingsClient(async (command, args) => { if (reply instanceof Error) throw reply; return typeof reply === 'function' ? reply(command, args) : reply; });

test('the terminal start folder may be empty or an absolute drive path, nothing else', () => {
  for (const ok of ['', '   ', 'C:\\Users\\me', "C:\\Users\\O'Brien", 'd:\\work\\my project', 'C:\\Program Files (x86)\\x']) assert.equal(s.startFolderProblem(ok), null, JSON.stringify(ok));
  for (const bad of ['relative', '/c/Users', '\\\\server\\share', 'C:', 'C:\\x\\..\\y', 'C:\\x\\.\\y', 'C:\\a"b', 'C:\\a<b', 'C:\\a|b', 'C:\\a*b', 'C:\\a?b', 'C:\\a\nb', 'C:\\' + 'a'.repeat(250)]) {
    assert.notEqual(s.startFolderProblem(bad), null, JSON.stringify(bad));
  }
});

test('the agent launch folder follows the stricter rule (it is typed into PowerShell inside quotes)', () => {
  assert.equal(s.launchFolderProblem(''), null);
  assert.equal(s.launchFolderProblem('C:\\work'), null);
  for (const bad of ["C:\\it's", 'C:\\a\u2019b', 'relative']) assert.notEqual(s.launchFolderProblem(bad), null, JSON.stringify(bad));
});

test('only a settings object of exactly the expected shape is accepted', () => {
  assert.equal(s.isSettings(good), true);
  assert.equal(s.isSettings(s.defaultSettings), true);
  assert.equal(s.defaultSettings.keys.prefix, 'ctrl+b', 'tmux\'s prefix by default');
  for (const prefix of ['ctrl+b', 'ctrl+a', 'off']) assert.equal(s.isSettings({ ...good, keys: { prefix } }), true, prefix);
  for (const bad of [null, 5, [], {}, { ...good, version: 2 }, { ...good, extra: 1 },
    { ...good, terminals: { ...good.terminals, extra: 1 } }, { ...good, terminals: { start_folder: 'x' } },
    { ...good, terminals: { start_folder: 5, extended_history: false } }, { ...good, terminals: { start_folder: '', extended_history: 'yes' } },
    { ...good, agents: {} }, { ...good, agents: { launch_folder: 'x'.repeat(241) } },
    { ...good, appearance: undefined }, { ...good, appearance: { sidebar1_opacity: 40 } },
    { ...good, appearance: { sidebar1_opacity: 101, sidebar2_opacity: 100 } }, { ...good, appearance: { sidebar1_opacity: -1, sidebar2_opacity: 100 } },
    { ...good, appearance: { sidebar1_opacity: 50.5, sidebar2_opacity: 100 } }, { ...good, appearance: { sidebar1_opacity: '50', sidebar2_opacity: 100 } },
    { ...good, appearance: { sidebar1_opacity: 50, sidebar2_opacity: 100, extra: 1 } },
    { ...good, window: undefined }, { ...good, window: { ...good.window, extra: 1 } },
    { ...good, window: { ...good.window, canvas_mode: 'image' } }, { ...good, window: { ...good.window, canvas_color: 'red' } },
    { ...good, window: { ...good.window, canvas_color: '#12345' } }, { ...good, window: { ...good.window, canvas_alpha: 101 } },
    { ...good, window: { ...good.window, see_through: 'yes' } }, { ...good, window: { ...good.window, titlebar_autohide: 1 } },
    { ...good, keys: undefined }, { ...good, keys: {} }, { ...good, keys: { prefix: 'ctrl+c' } }, { ...good, keys: { prefix: 'off', extra: 1 } }]) {
    assert.equal(s.isSettings(bad), false, JSON.stringify(bad));
  }
});

test('loading validates the reply; saving checks the values first and returns what was stored', async () => {
  assert.deepEqual(await clientWith(good).load(), good);
  await assert.rejects(clientWith({ nope: 1 }).load(), e => e.code === 'invalid_response');
  const calls = [];
  const client = clientWith((command, args) => { calls.push([command, args]); return args.settings; });
  assert.deepEqual(await client.save(good), good);
  assert.deepEqual(calls, [['settings_save', { settings: good }]]);
  // A bad value never reaches native.
  const bad = { ...good, terminals: { ...good.terminals, start_folder: 'relative' } };
  await assert.rejects(client.save(bad), e => e.code === 'settings_invalid' && /absolute/.test(e.message));
  assert.equal(calls.length, 1);
  // A reply that is not valid settings is refused even after a "successful" save.
  await assert.rejects(clientWith({ ...good, extra: true }).save(good), e => e.code === 'invalid_response');
  // Native refusals keep their code and message; unknown errors are generic.
  const refused = await clientWith(Object.assign(new Error('x'), { code: 'settings_invalid', user_message: 'The terminal start folder does not exist.' })).save(good).catch(e => e);
  assert.deepEqual(refused, { code: 'settings_invalid', message: 'The terminal start folder does not exist.' });
  const odd = await clientWith(new Error('stack with C:\\secret')).load().catch(e => e);
  assert.ok(!JSON.stringify(odd).includes('secret'));
});
