// Actual Svelte SSR: display/escaping/grouping, not browser interaction or native proof.
import test, { before } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
const root = fileURLToPath(new URL('../../', import.meta.url));
const { upgrade } = createRequire(import.meta.url)(join(root, 'frontend/src/execution/v2-fixtures.cjs'));
const requireFrontend = createRequire(join(root, 'frontend/package.json'));
const { compile } = requireFrontend('svelte/compiler');
const { build } = requireFrontend('esbuild');
const output = join(root, 'artifacts/frontend-component-tests');
const component = join(root, 'frontend/src/components/RunBoard.svelte');
let renderBoard;
before(async () => {
  mkdirSync(output, { recursive: true });
  const bundle = join(output, 'run-board.cjs');
  await build({
    stdin: { contents: `import Board from ${JSON.stringify(component)}; import { render } from 'svelte/server'; export const renderBoard = props => render(Board, { props }).body;`, resolveDir: join(root, 'frontend') },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'], outfile: bundle, logLevel: 'silent',
    plugins: [{ name: 'actual-run-board-ssr', setup(builder) {
      builder.onLoad({ filter: /RunBoard\.svelte$/ }, args => ({ contents: compile(readFileSync(args.path, 'utf8'), { filename: args.path, generate: 'server', css: 'external', dev: false }).js.code, loader: 'js', resolveDir: join(root, 'frontend/src/components') }));
    } }]
  });
  ({ renderBoard } = createRequire(import.meta.url)(bundle));
});
function props() {
  return { sessions: [], runs: [], loaded: true, available: true, canInspect: true, refreshing: false, onInspect: () => assert.fail('SSR must not inspect') };
}
function sample() {
  const { snapshot } = upgrade(JSON.parse(readFileSync(join(root, 'contracts/fixtures/fake-success.json'), 'utf8')));
  return { ...props(), sessions: [{ session_id: snapshot.session_id, label: '<script>label</script>\u001b[31m' }], runs: [
    { ...snapshot, run_id: 'finished', enqueue_order: 1 },
    { ...snapshot, run_id: 'queue-later', enqueue_order: 4, state: 'queued' },
    { ...snapshot, run_id: 'active', enqueue_order: 2, state: 'cancel_requested' },
    { ...snapshot, run_id: 'queue-first', enqueue_order: 3, state: 'queued' },
  ] };
}
test('run board distinguishes unloaded authority from a genuinely empty loaded workspace', () => {
  const unloaded = renderBoard({ ...props(), loaded: false, available: false, canInspect: false });
  assert.match(unloaded, /No native run data loaded/);
  assert.doesNotMatch(unloaded, /data-group=/);
  const empty = renderBoard(props());
  assert.match(empty, /No runs yet/);
  assert.equal((empty.match(/data-group=/g) ?? []).length, 3);
  assert.match(empty, /Automatic handoff — unavailable/);
});
test('run board groups active/queued/terminal summaries without changing native input order', () => {
  const value = sample(), original = structuredClone(value.runs), html = renderBoard(value);
  assert.deepEqual(value.runs, original);
  const queued = html.slice(html.indexOf('data-group="queued"'), html.indexOf('data-group="history"'));
  assert.ok(queued.indexOf('data-run-id="queue-first"') < queued.indexOf('data-run-id="queue-later"'));
  assert.ok(html.indexOf('data-run-id="active"') < html.indexOf('data-group="queued"'));
  assert.ok(html.indexOf('data-run-id="finished"') > html.indexOf('data-group="history"'));
  assert.match(html, /4 shown \/ 4 retained \/ 200 maximum/);
});
test('run board escapes labels, keeps unknown usage honest and marks stale data', () => {
  const html = renderBoard({ ...sample(), available: false, canInspect: false });
  assert.match(html, /Last loaded summaries only/);
  assert.match(html, /input tokens unknown · output tokens unknown/);
  assert.match(html, /&lt;script>label&lt;\/script>/);
  assert.match(html, /\\u001b\[31m/);
  assert.doesNotMatch(html, /<script>|\u001b/);
  assert.equal((html.match(/<button disabled[^>]*aria-label="Inspect/g) ?? []).length, 4);
});
test('quiet reconciliation changes aria-busy only, without inserting a flashing refresh row', () => {
  const value = sample(), quiet = renderBoard(value), busy = renderBoard({ ...value, refreshing: true });
  assert.equal(quiet.replace('aria-busy="false"', ''), busy.replace('aria-busy="true"', ''));
  assert.doesNotMatch(busy, /Refreshing durable/);
});
