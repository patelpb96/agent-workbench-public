// Real Svelte SSR + the real UI-only projector. Synthetic malformed identity/cap
// cases are not native acceptance; browser tests use authoritative native pages.
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
let renderTimeline, projectEventWindow;
before(async () => {
  const output = join(root, 'artifacts/frontend-component-tests'); mkdirSync(output, { recursive: true });
  const component = join(root, 'frontend/src/components/EventTimeline.svelte'), projector = join(root, 'frontend/src/components/event-metadata.ts'), bundle = join(output, 'event-timeline.cjs');
  await build({
    stdin: { contents: `import Timeline from ${JSON.stringify(component)}; import { render } from 'svelte/server'; export { projectEventWindow } from ${JSON.stringify(projector)}; export const renderTimeline = props => render(Timeline, { props }).body;`, resolveDir: join(root, 'frontend') },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'], outfile: bundle, logLevel: 'silent',
    plugins: [{ name: 'actual-event-timeline-ssr', setup(builder) {
      builder.onLoad({ filter: /EventTimeline\.svelte$/ }, args => ({ contents: compile(readFileSync(args.path, 'utf8'), { filename: args.path, generate: 'server', css: 'external', dev: false }).js.code, loader: 'js', resolveDir: join(root, 'frontend/src/components') }));
    } }]
  });
  ({ renderTimeline, projectEventWindow } = createRequire(import.meta.url)(bundle));
});
function sample() {
  const data = upgrade(JSON.parse(readFileSync(join(root, 'contracts/fixtures/fake-success.json'), 'utf8')));
  const transcript = { summary: data.snapshot, summary_seq: data.snapshot.last_seq, known_last_seq: data.snapshot.last_seq, applied_seq: data.snapshot.last_seq, loaded: true, events: data.events, view: 'live', generation: 0, pass: null, dirty: false, problem: null };
  const props = { sessions: [{ session_id: data.snapshot.session_id, label: 'SIMULATED event history' }], runs: [data.snapshot], selectedRunId: data.snapshot.run_id, loaded: true, available: true, canRead: true, refreshing: false, onSelect() { assert.fail('SSR must not select'); }, onEarlier() { assert.fail('SSR must not page'); }, onLive() { assert.fail('SSR must not page'); } };
  return { data, transcript, props };
}
function render(value) { return renderTimeline({ ...value.props, window: projectEventWindow(value.transcript, value.data.snapshot) }); }
test('event timeline distinguishes no workspace/no runs from an unloaded selected history', () => {
  const value = sample();
  const empty = renderTimeline({ ...value.props, runs: [], selectedRunId: undefined, window: null });
  assert.match(empty, /No runs yet/);
  const disconnected = renderTimeline({ ...value.props, runs: [], selectedRunId: undefined, window: null, loaded: false, available: false });
  assert.match(disconnected, /No native workspace data loaded/); assert.doesNotMatch(disconnected, /No runs yet/);
  value.transcript.loaded = false; value.transcript.events = []; value.transcript.known_last_seq = 604; value.transcript.summary_seq = 604;
  const unloaded = render(value);
  assert.match(unloaded, /Native high-water #604/); assert.match(unloaded, /No event range is claimed/); assert.doesNotMatch(unloaded, /data-event-id=/);
});
test('projector strips every payload and copies headers without changing authoritative events', () => {
  const value = sample(), original = structuredClone(value.transcript);
  value.transcript.events.find(event => event.kind === 'text_delta').payload.text = 'PRIVATE_PAYLOAD_SENTINEL';
  const before = structuredClone(value.transcript), projected = projectEventWindow(value.transcript, value.data.snapshot);
  assert.ok(projected.headers.length > 0);
  assert.doesNotMatch(JSON.stringify(projected), /PRIVATE_PAYLOAD_SENTINEL|payload|terminal_error/);
  assert.deepEqual(value.transcript, before);
  projected.headers[0].kind = 'usage_updated';
  assert.equal(value.transcript.events[0].kind, original.events[0].kind);
  assert.doesNotMatch(render(value), /PRIVATE_PAYLOAD_SENTINEL/);
});
test('projector rejects mismatched selected summary, foreign events and non-fake selections', () => {
  const value = sample();
  assert.equal(projectEventWindow(value.transcript, { ...value.data.snapshot, run_id: 'other' }), null);
  assert.equal(projectEventWindow(value.transcript, { ...value.data.snapshot, session_id: 'other' }), null);
  assert.equal(projectEventWindow(value.transcript, { ...value.data.snapshot, provider_id: 'codex' }), null);
  value.transcript.events[0].workspace_id = 'other';
  assert.equal(projectEventWindow(value.transcript, value.data.snapshot), null);
  assert.equal(projectEventWindow(null, value.data.snapshot), null);
});
test('metadata count cap adjusts omission and never treats high-water as a loaded cursor', () => {
  const value = sample(), base = value.data.events[0];
  value.transcript.events = Array.from({ length: 205 }, (_, index) => ({ ...base, event_id: `synthetic-${index}`, seq: index + 1 }));
  value.transcript.known_last_seq = 300; value.transcript.summary_seq = 300; value.transcript.applied_seq = 205; value.transcript.view = 'history';
  const view = projectEventWindow(value.transcript, value.data.snapshot);
  assert.equal(view.headers.length, 200); assert.equal(view.headers[0].seq, 6); assert.equal(view.headers.at(-1).seq, 205);
  assert.deepEqual(view.omitted.before, { after_seq: 0, through_seq: 5 });
  assert.deepEqual(view.omitted.after, { after_seq: 205, through_seq: 300 });
  const html = renderTimeline({ ...value.props, window: view });
  assert.match(html, /Loaded metadata #6–#205/); assert.match(html, /Native high-water #300/); assert.match(html, /Later history omitted: events 206–300/);
  assert.equal((html.match(/data-event-id=/g) ?? []).length, 200);
});
test('metadata display escapes labels/errors, marks stale data and never enables fake approvals', () => {
  const value = sample(); value.props.sessions[0].label = '<script>label</script>\u001b[31m';
  value.props.available = false; value.props.canRead = false;
  value.transcript.problem = { code: '<img>\u001b', user_message: 'PRIVATE_ERROR_SENTINEL', recoverable: true };
  const html = render(value);
  assert.match(html, /Last loaded history only/); assert.match(html, /&lt;script>label&lt;\/script>/); assert.match(html, /\\u001b/);
  assert.doesNotMatch(html, /<script>|<img>|PRIVATE_ERROR_SENTINEL|\u001b/);
  assert.match(html, /<button disabled[^>]*>Approve — unavailable/); assert.match(html, /<button disabled[^>]*>Deny — unavailable/);
});
test('loaded history navigation uses omissions/pending state, not summary sequence or terminal state', () => {
  const value = sample(); value.transcript.events = value.transcript.events.slice(2); value.transcript.view = 'history';
  const html = render(value);
  assert.match(html, /HISTORY VIEW/); assert.match(html, /Earlier history omitted: events 1–2/);
  assert.doesNotMatch(html, /<button disabled[^>]*>Load earlier event metadata/);
  value.transcript.pass = { next_after_seq: 0, through_seq: 2, replace: true };
  const pending = render(value);
  assert.match(pending, /<button disabled[^>]*>Load earlier event metadata/); assert.match(pending, /<button disabled[^>]*>Return to live event metadata/);
});
test('quiet metadata reconciliation changes only aria-busy, not visible refresh markup', () => {
  const value = sample(); const quiet = render(value); value.props.refreshing = true;
  assert.equal(quiet.replace('aria-busy="false"', ''), render(value).replace('aria-busy="true"', ''));
});
test('codex selections project their own event metadata; unknown providers do not', () => {
  const value = sample();
  const codex = { ...value.data.snapshot, provider_id: 'codex', profile_id: 'codex_text_v1', model_id: 'gpt-6.1-sol' };
  value.transcript.summary = { ...value.transcript.summary, provider_id: 'codex' };
  value.transcript.events = value.transcript.events.map(event => ({ ...event, provider_id: 'codex' }));
  const view = projectEventWindow(value.transcript, codex);
  assert.ok(view);
  assert.equal(view.headers.length, value.transcript.events.length);
  assert.equal(projectEventWindow(value.transcript, { ...codex, provider_id: 'claude' }), null);
  // A fake selection never projects a Codex transcript.
  assert.equal(projectEventWindow(value.transcript, value.data.snapshot), null);
});
