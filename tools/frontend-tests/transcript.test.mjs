// Actual Svelte SSR component regressions. These detect markup churn, not pixels
// or WebView/GPU flicker. No native app/provider/production data access.
import test, { before } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const root = fileURLToPath(new URL('../../', import.meta.url));
const { upgrade } = createRequire(import.meta.url)(join(root, 'frontend/src/execution/v2-fixtures.cjs'));
const requireFrontend = createRequire(join(root, 'frontend/package.json'));
// Both tools already belong to the locked frontend installation; no new deps.
const { compile } = requireFrontend('svelte/compiler');
const { build } = requireFrontend('esbuild');
const output = join(root, 'artifacts/frontend-component-tests');
const component = join(root, 'frontend/src/components/SessionTranscript.svelte');
const fixture = name => upgrade(JSON.parse(readFileSync(join(root, 'contracts/fixtures', name + '.json'), 'utf8')));
let renderTranscript;

before(async () => {
  mkdirSync(output, { recursive: true });
  const bundle = join(output, 'session-transcript.cjs');
  await build({
    stdin: {
      contents: `import Transcript from ${JSON.stringify(component)};\nimport { render } from 'svelte/server';\nexport const renderTranscript = props => render(Transcript, { props }).body;`,
      resolveDir: join(root, 'frontend'), sourcefile: 'transcript-component-test.mjs'
    },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'],
    outfile: bundle, logLevel: 'silent',
    plugins: [{
      name: 'actual-transcript-ssr',
      setup(builder) {
        builder.onLoad({ filter: /SessionTranscript\.svelte$/ }, args => ({
          contents: compile(readFileSync(args.path, 'utf8'), {
            filename: args.path, generate: 'server', css: 'external', dev: false
          }).js.code,
          loader: 'js', resolveDir: join(root, 'frontend/src/components')
        }));
      }
    }]
  });
  ({ renderTranscript } = createRequire(import.meta.url)(bundle));
});

function props(name = 'fake-success') {
  const data = fixture(name);
  return {
    run: data.snapshot, events: data.events,
    omission: { before: null, after: null, message: null }, stage: 'full'
  };
}
function withoutBusy(markup) {
  return markup.replace(/\saria-busy="(?:true|false)"/g, '');
}

for (const name of ['fake-success', 'fake-failed', 'fake-cancelled', 'fake-recovered']) {
  test(`background refresh preserves visible markup/flow for ${name}`, () => {
    const value = props(name);
    const idle = renderTranscript({ ...value, refreshing: false });
    const refreshing = renderTranscript({ ...value, refreshing: true });
    assert.equal(withoutBusy(refreshing), withoutBusy(idle),
      'a routine poll must not insert/remove a visible status row above transcript text');
    assert.equal((refreshing.match(/<li\b/g) ?? []).length, value.events.length);
    assert.ok(refreshing.includes('SIMULATED'));
  });
}

test('refresh remains accessible without a repeatedly flashing status announcement', () => {
  const value = props();
  assert.match(renderTranscript({ ...value, refreshing: true }), /aria-busy="true"/);
  assert.match(renderTranscript({ ...value, refreshing: false }), /aria-busy="false"/);
  assert.doesNotMatch(renderTranscript({ ...value, refreshing: true }), /Refreshing durable history/);
});

test('refresh does not disturb earlier-history controls or escaping of display text', () => {
  const value = props();
  value.events.find(event => event.kind === 'text_delta').payload.text = '<script>not executable</script>\u001b[31m';
  value.events = value.events.slice(2);
  value.omission = { before: { after_seq: 0, through_seq: 2 }, after: null, message: 'Earlier history omitted.' };
  value.onEarlier = () => {};
  const idle = renderTranscript({ ...value, refreshing: false });
  const refreshing = renderTranscript({ ...value, refreshing: true });
  assert.equal(withoutBusy(refreshing), withoutBusy(idle));
  assert.ok(refreshing.includes('Load earlier history'));
  assert.ok(refreshing.includes('&lt;script>not executable&lt;/script>'));
  assert.ok(refreshing.includes('\\u001b[31m'));
  assert.ok(!refreshing.includes('<script>'));
  assert.ok(!refreshing.includes('\u001b'));
});

test('codex runs render their provider reply, reported usage and errors (escaped), not the fake-only placeholder', () => {
  const data = fixture('fake-success');
  const codexRun = { ...data.snapshot, provider_id: 'codex', profile_id: 'codex_text_v1', model_id: 'gpt-6.1-sol',
    usage: { ...data.snapshot.usage, input_tokens: 21, output_tokens: 4, cached_input_tokens: 0, reasoning_output_tokens: 2 } };
  const events = data.events.map(event => {
    const codex = { ...event, provider_id: 'codex' };
    if (event.kind === 'run_created') codex.source = 'system';
    if (event.kind === 'text_delta') { codex.source = 'provider'; codex.payload = { ...event.payload, text: 'READY <b>bold</b>' }; }
    if (event.kind === 'usage_updated') { codex.source = 'provider'; codex.payload = { ...codexRun.usage }; }
    return codex;
  });
  const html = renderTranscript({ run: codexRun, events, omission: { before: null, after: null, message: null }, stage: 'full' });
  assert.match(html, /CODEX · codex_text_v1 · gpt-6\.1-sol/);
  assert.match(html, /aria-label="Codex run transcript"/);
  assert.match(html, /READY &lt;b>bold&lt;\/b>/);
  assert.doesNotMatch(html, /<b>bold<\/b>/);
  assert.match(html, /Reported usage: input 21, output 4, cached 0, reasoning 2/);
  assert.match(html, /not a bill/);
  assert.doesNotMatch(html, /Provider execution is disabled/);
  // A foreign-provider event cannot appear in a Codex transcript.
  const foreign = renderTranscript({ run: codexRun, events: [...events, { ...events[3], event_id: '00000000-0000-4000-8000-00000000abcd', seq: 99, provider_id: 'fake', source: 'fixture', payload: { stream: 'assistant', text: 'FOREIGN FAKE TEXT' } }],
    omission: { before: null, after: null, message: null }, stage: 'full' });
  assert.doesNotMatch(foreign, /FOREIGN FAKE TEXT/);
  const failed = renderTranscript({ run: { ...codexRun, state: 'failed', terminal_error: { code: 'codex_timeout', user_message: 'Codex did not finish within its time limit.', recoverable: true } },
    events, omission: { before: null, after: null, message: null }, stage: 'full' });
  assert.match(failed, /Codex error · codex_timeout: Codex did not finish within its time limit\./);
});
