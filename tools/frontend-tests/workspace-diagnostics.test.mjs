// Actual UI projector + actual Svelte SSR. Extra injected fields/caps test the
// projection boundary, not native acceptance of malformed controller state.
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
let renderDiagnostics, projectDiagnostics;
before(async () => {
  const output = join(root, 'artifacts/frontend-component-tests'); mkdirSync(output, { recursive: true });
  const component = join(root, 'frontend/src/components/WorkspaceDiagnostics.svelte'), projector = join(root, 'frontend/src/components/diagnostics-metadata.ts'), bundle = join(output, 'workspace-diagnostics.cjs');
  await build({
    stdin: { contents: `import Diagnostics from ${JSON.stringify(component)}; import { render } from 'svelte/server'; export { projectDiagnostics } from ${JSON.stringify(projector)}; export const renderDiagnostics = props => render(Diagnostics, { props }).body;`, resolveDir: join(root, 'frontend') },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'], outfile: bundle, logLevel: 'silent',
    plugins: [{ name: 'actual-workspace-diagnostics-ssr', setup(builder) {
      builder.onLoad({ filter: /WorkspaceDiagnostics\.svelte$/ }, args => ({ contents: compile(readFileSync(args.path, 'utf8'), { filename: args.path, generate: 'server', css: 'external', dev: false }).js.code, loader: 'js', resolveDir: join(root, 'frontend/src/components') }));
    } }]
  });
  ({ renderDiagnostics, projectDiagnostics } = createRequire(import.meta.url)(bundle));
});
const requestId = '11111111-1111-4111-8111-111111111111';
function state() {
  const data = upgrade(JSON.parse(readFileSync(join(root, 'contracts/fixtures/fake-success.json'), 'utf8'))), run = data.snapshot;
  const layout = { layout_version: 1, workspace_id: run.workspace_id, revision: 0, theme: 'dark', active_module: 'diagnostics', sidebar_width: 300, panels: [], containers: [] };
  return { status: 'ready', snapshot: { contract_version: 2, workspace: { workspace_id: run.workspace_id, label: 'PRIVATE_WORKSPACE_LABEL', created_at: run.created_at }, sessions: [], runs: [run], layout, layout_warning: null, execution_available: true }, selected_session_id: null, selected_run_by_session: {}, transcript: null,
    layout: { base: layout, draft: structuredClone(layout), warning: null, edit_epoch: 0, attempt: null, dirty: false, conflicted: false, problem: null }, attempts: [], last_error: null, dirty: false, in_flight: false, mutation_pending: false, visible: true, listener_ready: true, fixture: data };
}
function render(value, runtime) { return renderDiagnostics({ data: projectDiagnostics(value, runtime), refreshing: value.in_flight, canRefresh: true, onRefresh() { assert.fail('SSR must not refresh'); } }); }
test('diagnostics distinguishes missing snapshot from real loaded zero counts', () => {
  const value = state(); value.snapshot = null; value.status = 'connecting';
  const missing = projectDiagnostics(value); assert.equal(missing.sessions, null); assert.equal(missing.queued, null); assert.equal(missing.execution, 'unavailable');
  assert.match(render(value), /No native workspace snapshot loaded/);
  value.snapshot = state().snapshot; value.snapshot.runs = [];
  assert.equal(projectDiagnostics(value).runs, 0); assert.equal(projectDiagnostics(value).active, 0);
});
test('counts use stored terminal transitions and never mutate run arrays', () => {
  const value = state(), base = value.snapshot.runs[0];
  value.snapshot.runs = ['succeeded','queued','running','cancel_requested','cancelled','failed','interrupted','waiting_input'].map((status, index) => ({ ...base, state: status, enqueue_order: index + 1 }));
  const original = structuredClone(value.snapshot.runs), view = projectDiagnostics(value);
  assert.deepEqual([view.runs,view.queued,view.active,view.terminal], [8,1,3,4]); assert.deepEqual(value.snapshot.runs, original);
});
test('layout warnings/conflicts and pending receipt expose only intent metadata', () => {
  const value = state(); value.layout.dirty = true; value.layout.conflicted = true;
  value.layout.warning = { code: 'layout_invalid', user_message: 'PRIVATE_LAYOUT_ERROR', recoverable: true };
  value.layout.attempt = { request: { contract_version: 2, workspace_id: value.snapshot.workspace.workspace_id, request_id: requestId, expected_revision: 0, layout: { ...value.layout.draft, private_extra: 'PRIVATE_LAYOUT_REQUEST' } }, edit_epoch: 0 };
  const view = projectDiagnostics(value);
  assert.deepEqual(view.layout.pending_save, { request_id: requestId, expected_revision: 0 });
  assert.equal(view.layout.warning_code, 'layout_invalid'); assert.ok(view.layout.dirty && view.layout.conflicted);
  assert.doesNotMatch(JSON.stringify(view), /PRIVATE_/); assert.doesNotMatch(render(value), /PRIVATE_/);
});
test('attempt metadata is capped and omits private retry keys/DTO bodies even with duplicate request IDs', () => {
  const value = state(); value.attempts = Array.from({ length: 10 }, (_, i) => ({ command: i % 2 ? 'cancel_run' : 'start_fake_run', request_id: requestId, key: `PRIVATE_KEY_${i}`, request: { prompt: 'PRIVATE_RETRY_BODY' }, pending: false }));
  const before = structuredClone(value.attempts), view = projectDiagnostics(value);
  assert.equal(view.attempts.length, 8); assert.deepEqual(Object.keys(view.attempts[0]).sort(), ['command','pending','request_id']);
  assert.doesNotMatch(JSON.stringify(view), /PRIVATE_/); assert.doesNotMatch(render(value), /PRIVATE_/); assert.deepEqual(value.attempts, before);
});
test('history metadata counts only loaded events and never includes any payload or error body', () => {
  const value = state(), run = value.fixture.snapshot;
  value.transcript = { summary: run, summary_seq: 604, known_last_seq: 604, loaded: true, view: 'history', events: value.fixture.events.slice(2), problem: { code: 'event_history_unavailable', user_message: 'PRIVATE_HISTORY_ERROR' } };
  value.transcript.events.find(e => e.kind === 'text_delta').payload.text = 'PRIVATE_EVENT_PAYLOAD';
  const view = projectDiagnostics(value); assert.equal(view.history.first_seq, 3); assert.equal(view.history.known_last_seq, 604);
  assert.equal(view.history.last_seq, run.last_seq); assert.equal(view.history.count, value.transcript.events.length);
  assert.doesNotMatch(JSON.stringify(view), /PRIVATE_|payload|events/);
  value.transcript.loaded = false; const unloaded = projectDiagnostics(value);
  assert.equal(unloaded.history.first_seq, null); assert.equal(unloaded.history.last_seq, null); assert.equal(unloaded.history.count, 0);
});
test('unsafe error/ID/shell labels and provider discovery fields cannot become raw diagnostic strings', () => {
  const value = state(); value.last_error = { code: 'C:\\PRIVATE_ERROR_PATH', user_message: 'PRIVATE_BODY' };
  value.snapshot.workspace.workspace_id = 'PRIVATE_INVALID_ID';
  const runtime = { os: '<script>PRIVATE_OS</script>', app_version: 'C:\\PRIVATE_VERSION', providers: [{ executable: 'PRIVATE_PROVIDER_PATH' }] };
  const view = projectDiagnostics(value, runtime), html = render(value, runtime);
  assert.equal(view.workspace_id, 'unrecognized_id'); assert.equal(view.last_error_code, 'unrecognized_error'); assert.equal(view.shell.os, 'unavailable');
  assert.doesNotMatch(JSON.stringify(view), /PRIVATE_|providers/); assert.doesNotMatch(html, /PRIVATE_|<script>/);
  assert.deepEqual(projectDiagnostics(state(), { os: 'headless shell stub', app_version: 'headless-test' }).shell, { os: 'headless shell stub', app_version: 'headless-test' });
});
test('stale snapshots never promise current fake availability just because a retained flag is true', () => {
  const value = state(); value.status = 'unavailable'; value.visible = false; value.listener_ready = false;
  const view = projectDiagnostics(value); assert.equal(view.execution, 'unverified'); assert.equal(view.runs, 1);
  assert.match(render(value), /Last loaded metadata only/);
  value.status = 'ready'; value.snapshot.execution_available = false; assert.equal(projectDiagnostics(value).execution, 'unavailable');
});
test('routine read-pump dirty/in-flight flags never cycle visible diagnostics markup', () => {
  const value = state(), before = projectDiagnostics(value), quiet = render(value);
  value.in_flight = true; value.dirty = true;
  assert.deepEqual(projectDiagnostics(value), before);
  assert.equal(quiet.replace('aria-busy="false"', ''), render(value).replace('aria-busy="true"', ''));
  assert.doesNotMatch(render(value), /Summary refresh:|>idle<|>requested</);
});
