// Executes the actual FilesPanel instance script with controlled transports and SSR-renders the component.
// This is NOT real DOM or Svelte reactivity certification. No native/app/provider/browser/terminal commands are run.
import test, { before } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import vm from 'node:vm';

const root = fileURLToPath(new URL('../../', import.meta.url));
const componentPath = join(root, 'frontend/src/files/FilesPanel.svelte');
const requireFrontend = createRequire(join(root, 'frontend/package.json'));
const requireHere = createRequire(import.meta.url);
const { compile, parse } = requireFrontend('svelte/compiler');
const { build } = requireFrontend('esbuild');
const ts = requireFrontend('typescript');
const output = join(root, 'artifacts/frontend-component-tests');
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const listing = hidden => ({ entries: [], more: 0, hidden });
const saved = (rootPath, following = false, extra = {}) => JSON.stringify({ root: rootPath, expanded: [], hidden: false, following, ...extra });
let filesApi, followApi, renderFiles;

before(async () => {
  mkdirSync(output, { recursive: true });
  const filesBundle = join(output, 'files-pure.cjs'), followBundle = join(output, 'follow-pure.cjs');
  await build({ entryPoints: [join(root, 'frontend/src/files/files.js')], bundle: true, platform: 'node', format: 'cjs', outfile: filesBundle, logLevel: 'silent' });
  await build({ entryPoints: [join(root, 'frontend/src/files/follow.ts')], bundle: true, platform: 'node', format: 'cjs', outfile: followBundle, logLevel: 'silent' });
  filesApi = requireHere(filesBundle); followApi = requireHere(followBundle);
  const ssrBundle = join(output, 'files-panel-ssr.cjs');
  await build({
    stdin: {
      contents: `import FilesPanel from ${JSON.stringify(componentPath)};
import { render } from 'svelte/server';
export function renderFiles(saved, pins) {
  const values = new Map([['aw.files', saved ?? 'null'], ['aw.files.pins', JSON.stringify(pins ?? [])]]);
  globalThis.localStorage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) };
  return render(FilesPanel, { props: { onShowTerminals() {} } }).body;
}`,
      resolveDir: join(root, 'frontend'), sourcefile: 'files-panel-ssr-test.mjs'
    },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'], outfile: ssrBundle, logLevel: 'silent',
    plugins: [{
      name: 'actual-files-panel-ssr-with-safe-imports',
      setup(builder) {
        builder.onResolve({ filter: /.*/ }, args => {
          if (args.path === '@tauri-apps/api/core') return { path: 'tauri', namespace: 'files-stubs' };
          if (args.path.endsWith('ContextMenu.svelte')) return { path: 'menu', namespace: 'files-stubs' };
          if (args.path.includes('settings-store.svelte.js')) return { path: 'settings', namespace: 'files-stubs' };
          if (args.path.includes('last-focused.svelte.js')) return { path: 'last-focused', namespace: 'files-stubs' };
          if (args.path.includes('agent-store.svelte.js')) return { path: 'agents', namespace: 'files-stubs' };
          if (args.path.endsWith('/terminals/tauri.js')) return { path: 'terminal-client', namespace: 'files-stubs' };
          if (args.path.includes('requests.svelte.js')) return { path: 'terminal-requests', namespace: 'files-stubs' };
          return undefined;
        });
        builder.onLoad({ filter: /.*/, namespace: 'files-stubs' }, args => {
          const modules = {
            tauri: `export const invoke = () => { throw new Error('unexpected invoke'); }; export const isTauri = () => false;`,
            menu: `export default function ContextMenu() { return null; }`,
            settings: `export const settingsStore = { value: { terminals: { start_folder: '' } }, pickFolder: async () => null };`,
            'last-focused': `export const lastFocused = { id: null, label: '' };`,
            agents: `export const agentStore = { entryOf: () => undefined };`,
            'terminal-client': `export const terminalClient = { input: async () => { throw new Error('unexpected terminal input'); } };`,
            'terminal-requests': `export const terminalRequests = {};`
          };
          return { contents: modules[args.path], loader: 'js' };
        });
        builder.onLoad({ filter: /FilesPanel\.svelte$/ }, args => ({
          contents: compile(readFileSync(args.path, 'utf8'), { filename: args.path, generate: 'server', css: 'external', dev: false }).js.code,
          loader: 'js', resolveDir: join(root, 'frontend/src/files')
        }));
      }
    }]
  });
  ({ renderFiles } = requireHere(ssrBundle));
});

function checkboxExpression(ast) {
  const seen = new Set();
  let expression;
  const visit = node => {
    if (!node || typeof node !== 'object' || seen.has(node) || expression) return;
    seen.add(node);
    if ((node.type === 'RegularElement' || node.type === 'Element') && node.name === 'input') {
      const attr = node.attributes?.find(item => item.name === 'onchange'), value = attr?.value;
      expression = Array.isArray(value) ? value.find(item => item?.expression)?.expression : value?.expression ?? attr?.expression;
      if (expression) return;
    }
    for (const value of Object.values(node)) {
      if (Array.isArray(value)) value.forEach(visit);
      else if (value && typeof value === 'object') visit(value);
    }
  };
  visit(ast.html ?? ast.fragment);
  assert.ok(expression, 'Svelte AST exposes the native checkbox onchange expression');
  return expression;
}

function harness(options = {}) {
  const effects = [], mounts = [], destroys = [], calls = [], forbidden = [];
  const values = new Map(Object.entries(options.storage ?? {}));
  const storage = { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, String(value)) };
  const registry = new Map(Object.entries(options.agents ?? {}));
  const lastFocused = { id: options.focusId ?? null, label: 'fixture terminal' }, picks = [];
  let homeCalls = 0, createFilesClientCalls = 0;
  const client = {
    list(path, hidden) {
      const index = calls.length; calls.push({ path, hidden });
      return options.list ? options.list(path, hidden, index) : Promise.resolve(listing(0));
    },
    home() { homeCalls++; return typeof options.home === 'function' ? options.home() : Promise.resolve(options.home ?? 'C:\\Home'); },
    reveal() { forbidden.push('reveal'); throw new Error('unexpected reveal'); }
  };
  const settingsStore = {
    value: { terminals: { start_folder: options.startFolder ?? '' } },
    pickFolder: () => picks.length ? picks.shift()() : Promise.resolve(null)
  };
  const context = {
    exports: {}, module: { exports: {} }, localStorage: storage,
    onMount: fn => mounts.push(fn), onDestroy: fn => destroys.push(fn), untrack: fn => fn(),
    $props: () => ({ onShowTerminals() {} }),
    $state: Object.assign(value => value, { raw: value => value }),
    $derived: { by: fn => fn() }, $effect: fn => effects.push(fn),
    ...filesApi, createFilesClient: () => { createFilesClientCalls++; return client; },
    invoke: (...args) => { forbidden.push(`invoke:${args[0]}`); throw new Error('unexpected invoke'); },
    isTauri: () => false, ContextMenu: function ContextMenu() {},
    settingsStore, lastFocused, agentStore: { entryOf: id => registry.get(id) },
    terminalClient: { input: () => { forbidden.push('input'); throw new Error('unexpected input'); } },
    terminalRequests: {}, createFollowPolicy: followApi.createFollowPolicy, sameFolder: followApi.sameFolder
  };
  context.globalThis = context;
  const source = readFileSync(componentPath, 'utf8'), parsed = parse(source, { filename: componentPath });
  assert.ok(parsed.instance?.content, 'Svelte parser found the real component instance script');
  const script = source.slice(parsed.instance.content.start, parsed.instance.content.end);
  const handler = checkboxExpression(parsed), handlerSource = source.slice(handler.start, handler.end);
  const expose = `
globalThis.__filesTest = {
  get root() { return root; }, get following() { return following; }, get showHidden() { return showHidden; },
  get notice() { return notice; }, get folders() { return folders; }, get expanded() { return expanded; }, get pinsOpen() { return pinsOpen; },
  get policy() { return followPolicy; }, load, changeFollowing, checkboxChange: event => (${handlerSource})(event),
  toggleHidden, showFolder, up, refresh, pickRoot, togglePinOpen, itemsFor, loadedFor, walkInto,
  get homeCalls() { return homeCalls; }
};`;
  const ast = ts.createSourceFile('FilesPanel-instance.ts', script, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const imports = ast.statements
    .filter(ts.isImportDeclaration)
    .map(statement => ({ start: statement.getStart(ast), end: statement.end }))
    .sort((a, b) => b.start - a.start);
  let scriptWithoutImports = script;
  for (const range of imports) {
    scriptWithoutImports = scriptWithoutImports.slice(0, range.start) + scriptWithoutImports.slice(range.end);
  }
  const javascript = ts.transpileModule(`${scriptWithoutImports}\n${expose}`, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS }
  }).outputText;
  vm.runInNewContext(javascript, context, { filename: 'actual-FilesPanel-instance-script.js' });
  return {
    api: context.__filesTest, calls, forbidden, storage, registry, lastFocused, picks,
    queuePick: promise => picks.push(() => promise),
    get createFilesClientCalls() { return createFilesClientCalls; },
    mount() { for (const fn of mounts) fn(); for (const fn of effects) fn(); },
    flushEffects() { for (const fn of effects) fn(); },
    focus(id, cwd, registered = true) {
      lastFocused.id = id;
      if (id !== null && registered) registry.set(id, { cwd });
      else if (id !== null) registry.delete(id);
      this.flushEffects();
    },
    destroy() { for (const fn of destroys) fn(); },
    get homeCalls() { return homeCalls; }
  };
}
async function drain() { for (let i = 0; i < 12; i++) await Promise.resolve(); }

test('actual component script reaches the injected mocked files client', () => {
  const h = harness();
  assert.equal(h.createFilesClientCalls, 1);
  assert.deepEqual(h.forbidden, []);
});

test('saved following is literal true only; missing and malformed flags remain OFF and are normalized', async () => {
  const malformed = harness({ storage: { 'aw.files': saved('C:\\Work', 'true') } });
  assert.equal(malformed.api.following, false); malformed.mount(); await drain();
  assert.equal(malformed.api.following, false);
  assert.equal(JSON.parse(malformed.storage.getItem('aw.files')).following, false);
  const missing = harness({ storage: { 'aw.files': JSON.stringify({ root: 'C:\\Work' }) } });
  assert.equal(missing.api.following, false); missing.mount(); await drain();
  assert.equal(missing.api.following, false);
  assert.equal(JSON.parse(missing.storage.getItem('aw.files')).following, false);
});

test('remembered opt-in uses a valid focused agent launch folder ahead of the saved root and persists it', async () => {
  const h = harness({ storage: { 'aw.files': saved('C:\\Saved', true) }, focusId: 'agent-1', agents: { 'agent-1': { cwd: 'D:\\Agent' } } });
  h.mount(); await drain();
  assert.equal(h.api.root, 'D:\\Agent'); assert.equal(h.api.following, true);
  assert.deepEqual(h.calls.map(call => call.path), ['D:\\Agent']);
  assert.match(h.api.notice, /recorded launch folder: D:\\Agent/);
  const reopened = harness({ storage: { 'aw.files': h.storage.getItem('aw.files') } });
  reopened.mount(); await drain();
  assert.equal(reopened.api.root, 'D:\\Agent', 'the initial focused-folder navigation was remembered');
  assert.equal(reopened.api.following, true);
  assert.deepEqual(reopened.calls.map(call => call.path), ['D:\\Agent']);
});

test('focus changes navigate only to registered recorded folders; invalid and case-only changes do not reload', async () => {
  const h = harness({ storage: { 'aw.files': saved('C:\\Work\\Alpha', true) }, focusId: 'agent-1', agents: { 'agent-1': { cwd: 'c:\\work\\alpha\\' } } });
  h.mount(); await drain(); assert.equal(h.calls.length, 1);
  h.focus('agent-2', 'C:\\Work\\Beta'); await drain();
  assert.equal(h.api.root, 'C:\\Work\\Beta'); assert.equal(h.calls.length, 2);
  assert.match(h.api.notice, /recorded launch folder: C:\\Work\\Beta/);
  h.focus('shell', undefined, false); assert.match(h.api.notice, /no valid recorded launch folder/);
  const ineligibleNotice = h.api.notice; h.flushEffects();
  assert.equal(h.api.notice, ineligibleNotice, 'the same ineligible focus does not churn the announcement');
  h.focus(null, undefined); h.focus('invalid', 'C:\\..\\escape'); await drain();
  assert.equal(h.api.root, 'C:\\Work\\Beta'); assert.equal(h.calls.length, 2);
  h.focus('equivalent', 'c:\\work\\beta\\');
  assert.doesNotMatch(h.api.notice ?? '', /no valid recorded launch folder/);
});

test('enabling follow with an ineligible focus keeps the root; full-path case duplicates do not reload', async () => {
  const h = harness({ storage: { 'aw.files': saved('C:\\Work\\Alpha') } });
  h.mount(); await drain(); h.api.checkboxChange({ currentTarget: { checked: true } });
  assert.match(h.api.notice, /Follow is on\. Click in an agent terminal/, 'no terminal focused yet');
  h.focus('unregistered', undefined, false);
  assert.match(h.api.notice, /no valid recorded launch folder/, 'a plain shell is announced as soon as it is focused');
  h.focus('bad', 'C:\\..\\bad'); await drain();
  assert.equal(h.api.following, true); assert.equal(h.api.root, 'C:\\Work\\Alpha');
  assert.equal(h.calls.length, 1); assert.match(h.api.notice, /no valid recorded launch folder/);
  h.focus('same', 'c:\\WORK\\ALPHA\\'); await drain(); assert.equal(h.calls.length, 1);
});

test('Up and Show this folder pause follow; cancelling the picker stays paused', async () => {
  const picker = deferred(), h = harness({ storage: { 'aw.files': saved('C:\\Work\\Child', true) } });
  h.queuePick(picker.promise); h.mount(); await drain();
  const picking = h.api.pickRoot();
  assert.equal(h.api.following, false, 'pause happens before awaiting the picker');
  assert.match(h.api.notice, /Follow paused while choosing/);
  picker.resolve(null); await picking;
  assert.equal(h.api.root, 'C:\\Work\\Child'); assert.equal(h.api.following, false);
  assert.match(h.api.notice, /Follow paused\. Keeping this folder/, 'the notice no longer says the picker is open');
  h.api.checkboxChange({ currentTarget: { checked: true } }); h.api.up();
  assert.equal(h.api.root, 'C:\\Work'); assert.equal(h.api.following, false);
  assert.match(h.api.notice, /Follow paused\. Showing parent folder/);
  const action = h.api.itemsFor('D:\\Chosen', true).find(item => item.label === 'Show this folder');
  assert.ok(action); action.onselect();
  assert.equal(h.api.root, 'D:\\Chosen'); assert.equal(h.api.following, false);
  assert.match(h.api.notice, /Follow paused\. Showing folder/);
});

test('startup home is stale after later focus or manual navigation', async () => {
  const home = deferred(), focused = harness({ storage: { 'aw.files': saved(null, true) }, home: () => home.promise });
  focused.mount(); await drain(); focused.focus('agent-1', 'D:\\Focused');
  home.resolve('C:\\LateHome'); await drain(); assert.equal(focused.api.root, 'D:\\Focused');
  const laterHome = deferred(), manual = harness({ storage: { 'aw.files': saved(null) }, home: () => laterHome.promise });
  manual.mount(); await drain(); manual.api.showFolder('E:\\Manual');
  laterHome.resolve('C:\\LateHome'); await drain(); assert.equal(manual.api.root, 'E:\\Manual');
});

test('actions that set no folder while startup waits for home do not leave the panel empty', async () => {
  const actions = [
    ['follow on', h => h.api.checkboxChange({ currentTarget: { checked: true } })],
    ['follow off', h => h.api.checkboxChange({ currentTarget: { checked: false } })],
    ['hidden files', h => h.api.toggleHidden()],
    ['cancelled picker', async h => { h.queuePick(Promise.resolve(null)); await h.api.pickRoot(); }]
  ];
  for (const [name, action] of actions) {
    const home = deferred(), h = harness({ storage: { 'aw.files': saved(null, name === 'follow off') }, home: () => home.promise });
    h.mount(); await drain();
    await action(h);
    home.resolve('C:\\Home'); await drain();
    assert.equal(h.api.root, 'C:\\Home', name);
    assert.ok(h.calls.some(call => call.path === 'C:\\Home'), `${name}: home is listed`);
  }
  const home = deferred(), pick = deferred(), h = harness({ storage: { 'aw.files': saved(null) }, home: () => home.promise });
  h.queuePick(pick.promise); h.mount(); await drain();
  const picking = h.api.pickRoot();
  home.resolve('C:\\Home'); await drain();
  assert.equal(h.api.root, 'C:\\Home');
  pick.resolve('E:\\Picked'); await picking;
  assert.equal(h.api.root, 'E:\\Picked', 'home arriving while the picker is open does not cancel the pick');
  assert.match(h.api.notice, /Showing folder: E:\\Picked/);
});

test('picker survives OFF-mode focus changes but a newer refresh invalidates an older picker', async () => {
  const pick = deferred(), h = harness({ storage: { 'aw.files': saved('C:\\Work') } });
  h.queuePick(pick.promise); h.mount(); await drain();
  const pending = h.api.pickRoot(); h.focus('agent', 'D:\\Recorded');
  pick.resolve('E:\\Picked'); await pending;
  assert.equal(h.api.root, 'E:\\Picked'); assert.equal(h.api.following, false);
  const stalePick = deferred(); h.queuePick(stalePick.promise);
  const stale = h.api.pickRoot(); h.api.refresh(); stalePick.resolve('F:\\Stale'); await stale;
  assert.equal(h.api.root, 'E:\\Picked');
  const invalid = harness({ storage: { 'aw.files': saved('C:\\Work') } });
  invalid.queuePick(Promise.resolve('C:\\..\\escape')); invalid.mount(); await drain();
  await invalid.api.pickRoot();
  assert.equal(invalid.api.root, 'C:\\Work');
  assert.match(invalid.api.notice, /That folder could not be shown/);
  assert.doesNotMatch(invalid.api.notice, /Showing folder/);
});

test('same-path request identity rejects late overlaps; pause and non-navigating toggle do not strand a listing', async () => {
  const pending = [], h = harness({ storage: { 'aw.files': saved('C:\\Work') }, list: () => { const d = deferred(); pending.push(d); return d.promise; } });
  h.mount(); const token = h.api.policy.captureContentToken();
  void h.api.load('c:\\work\\', token, true); assert.equal(pending.length, 2, 'an equivalent spelling of the root');
  pending[1].resolve(listing(2)); await drain(); pending[0].resolve(listing(1)); await drain();
  assert.equal(h.api.loadedFor('C:\\Work').listing.hidden, 2);
  assert.equal(h.api.folders.size, 1, 'equivalent spellings share one entry');
  const active = deferred(), pausePicker = deferred();
  const paused = harness({ storage: { 'aw.files': saved('C:\\Work') }, list: () => active.promise });
  paused.queuePick(pausePicker.promise); paused.mount();
  const picking = paused.api.pickRoot(); assert.equal(paused.api.following, false);
  active.resolve(listing(7)); await drain();
  assert.equal(paused.api.folders.get('C:\\Work').listing.hidden, 7);
  pausePicker.resolve(null); await picking;
});

test('equivalent pinned-path retry replaces a failure for both lookup and tree walking', async () => {
  const h = harness({
    storage: {
      'aw.files': saved('C:\\Work'),
      'aw.files.pins': JSON.stringify([{ path: 'c:\\work\\', folder: true }])
    },
    list: (_path, _hidden, index) => index === 0
      ? Promise.reject(new Error('old listing failure'))
      : Promise.resolve({ entries: [{ name: 'recovered.txt', kind: 'file', size: 4 }], more: 0, hidden: 0 })
  });
  h.mount(); await drain();
  assert.equal(h.api.loadedFor('C:\\Work')?.state, 'failed');
  assert.equal(h.api.loadedFor('c:\\work\\')?.state, 'failed');

  const rootBefore = h.api.root, followingBefore = h.api.following;
  h.api.togglePinOpen('c:\\work\\');
  await drain();
  assert.equal(h.calls.length, 2);
  assert.equal(h.calls[1].path, 'c:\\work\\');
  assert.equal(h.api.root, rootBefore);
  assert.equal(h.api.following, followingBefore);
  assert.equal(h.api.loadedFor('C:\\Work')?.state, 'ready');
  assert.equal(h.api.loadedFor('c:\\work\\')?.state, 'ready');

  for (const path of ['C:\\Work', 'c:\\work\\']) {
    const rows = [];
    h.api.walkInto(rows, path, 0);
    assert.equal(rows.some(row => row.kind === 'note' && row.failed), false);
    assert.ok(rows.some(row => row.kind === 'entry' && row.entry.name === 'recovered.txt'));
  }
});

test('effective checkbox toggles and same or invalid manual choices preserve pending same-root listings', async () => {
  const actions = [
    h => { h.api.checkboxChange({ currentTarget: { checked: true } }); h.api.checkboxChange({ currentTarget: { checked: false } }); },
    h => h.api.showFolder('c:\\work\\'),
    h => h.api.showFolder('C:\\..\\escape')
  ];
  for (const action of actions) {
    const result = deferred(), h = harness({ storage: { 'aw.files': saved('C:\\Work') }, list: () => result.promise });
    h.mount(); const token = h.api.policy.captureContentToken();
    action(h);
    assert.equal(h.api.policy.captureContentToken(), token);
    result.resolve(listing(8)); await drain();
    assert.equal(h.api.folders.get('C:\\Work').listing.hidden, 8);
  }
});

test('show-hidden refresh advances content and reloads root, expanded folders, and open pins', async () => {
  const h = harness({ storage: {
    'aw.files': JSON.stringify({ root: 'C:\\Work', expanded: ['C:\\Work\\Sub'], hidden: false, following: false }),
    'aw.files.pins': JSON.stringify([{ path: 'D:\\Pinned', folder: true }])
  } });
  h.mount(); await drain(); h.api.togglePinOpen('D:\\Pinned'); await drain();
  const before = h.calls.length; h.api.toggleHidden(); await drain();
  const refreshed = h.calls.slice(before);
  assert.deepEqual(refreshed.map(call => call.path).sort(), ['C:\\Work', 'C:\\Work\\Sub', 'D:\\Pinned'].sort());
  assert.ok(refreshed.every(call => call.hidden === true));
});

test('pinned-folder expansion stays in place and does not pause follow', async () => {
  const h = harness({
    storage: {
      'aw.files': saved('C:\\Root', true), 'aw.files.pins': JSON.stringify([{ path: 'D:\\Pinned', folder: true }])
    }
  });
  h.mount(); await drain(); const token = h.api.policy.captureContentToken();
  h.api.togglePinOpen('D:\\Pinned'); await drain();
  assert.equal(h.api.root, 'C:\\Root'); assert.equal(h.api.following, true);
  assert.equal(h.api.policy.captureContentToken(), token);
  assert.ok(h.calls.some(call => call.path === 'D:\\Pinned'));
});

test('unmount disposes policy and ignores late listing results', async () => {
  const result = deferred(), h = harness({ storage: { 'aw.files': saved('C:\\Work') }, list: () => result.promise });
  h.mount(); h.destroy(); result.resolve(listing(9)); await drain();
  assert.equal(h.api.policy.state().closed, true); assert.equal(h.api.policy.state().root, null);
  assert.equal(h.api.folders.get('C:\\Work').state, 'loading');
  assert.deepEqual(h.forbidden, []);
});

test('actual SSR markup retains controls and pins and renders saved checkbox ON and OFF', () => {
  const html = renderFiles(saved('C:\\Work'), [{ path: 'D:\\Pinned', folder: true }]);
  assert.match(html, /Follow focused agent’s recorded launch folder/);
  assert.match(html, /recorded launch folder, not live shell CWD/);
  const checkbox = html.match(/<input\b[^>]*>/)?.[0] ?? '';
  assert.match(checkbox, /type="checkbox"/); assert.doesNotMatch(checkbox, /\bchecked(?:=|>)/);
  const checkedHtml = renderFiles(saved('C:\\Work', true));
  const checkedInput = checkedHtml.match(/<input\b[^>]*>/)?.[0] ?? '';
  assert.match(checkedInput, /\bchecked(?:=|>)/);
  assert.match(html, /aria-label="Pinned files and folders"/); assert.match(html, /Pinned/);
  assert.match(html, /aria-label="Up one folder"/);
  assert.match(html, /aria-label="Choose the folder to show"/);
  assert.match(html, /aria-label="Show hidden files"/);
});
