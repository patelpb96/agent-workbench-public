// Actual-component SSR exercises synthetic validated-shape usage combinations.
// It is not evidence of real provider billing or native known-token production.
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
let renderUsage;
before(async () => {
  const output = join(root, 'artifacts/frontend-component-tests');
  mkdirSync(output, { recursive: true });
  const component = join(root, 'frontend/src/components/UsageOverview.svelte'), bundle = join(output, 'usage-overview.cjs');
  await build({
    stdin: { contents: `import Usage from ${JSON.stringify(component)}; import { render } from 'svelte/server'; export const renderUsage = props => render(Usage, { props }).body;`, resolveDir: join(root, 'frontend') },
    bundle: true, platform: 'node', format: 'cjs', conditions: ['node'], outfile: bundle, logLevel: 'silent',
    plugins: [{ name: 'actual-usage-overview-ssr', setup(builder) {
      builder.onLoad({ filter: /UsageOverview\.svelte$/ }, args => ({ contents: compile(readFileSync(args.path, 'utf8'), { filename: args.path, generate: 'server', css: 'external', dev: false }).js.code, loader: 'js', resolveDir: join(root, 'frontend/src/components') }));
    } }]
  });
  ({ renderUsage } = createRequire(import.meta.url)(bundle));
});
function props(usages = []) {
  const { snapshot } = upgrade(JSON.parse(readFileSync(join(root, 'contracts/fixtures/fake-success.json'), 'utf8')));
  return { sessions: [{ session_id: snapshot.session_id, label: 'SIMULATED usage' }], runs: usages.map((usage, index) => ({ ...snapshot, run_id: `synthetic-${index}`, enqueue_order: index + 1, usage })), loaded: true, available: true, canInspect: true, refreshing: false, onInspect: () => assert.fail('SSR must not inspect') };
}
const unknown = { input_tokens: null, output_tokens: null, estimated: false };
function total(html, key) {
  const clean = html.replace(/<!--[\s\S]*?-->/g, '');
  return clean.match(new RegExp(`data-total="${key}"[^>]*>([^<]*)<`))?.[1];
}
test('usage authority distinguishes unloaded from loaded empty and never invents a zero bill', () => {
  const unloaded = renderUsage({ ...props(), loaded: false, available: false });
  assert.match(unloaded, /No native usage data loaded/); assert.doesNotMatch(unloaded, /data-total=/);
  const empty = renderUsage(props());
  assert.match(empty, /No runs yet; no usage data to total/);
  assert.equal(total(empty, 'input_tokens-reported'), 'unknown');
  assert.match(empty, /billing totals, subscription quotas and account limits are unavailable/);
});
test('all unknown values remain unknown, not zero, for both estimate buckets', () => {
  const html = renderUsage(props([unknown, { ...unknown, estimated: true }]));
  for (const direction of ['input_tokens','output_tokens']) {
    assert.equal(total(html, `${direction}-reported`), 'unknown');
    assert.equal(total(html, `${direction}-estimated`), 'unknown');
    assert.equal(total(html, `${direction}-unknown`), '2 / 2');
  }
  // Svelte scopes the table-cell CSS with generated class attributes.
  assert.equal((html.match(/<td\b[^>]*>unreported<\/td>/g) ?? []).length, 2);
});
test('reported zero, partial unknown and estimated values stay distinct without combining buckets', () => {
  const html = renderUsage(props([
    { input_tokens: 0, output_tokens: 10, estimated: false },
    { input_tokens: 17, output_tokens: null, estimated: true },
    { input_tokens: null, output_tokens: 0, estimated: false },
  ]));
  assert.equal(total(html, 'input_tokens-reported'), '0');
  assert.equal(total(html, 'input_tokens-estimated'), '17');
  assert.equal(total(html, 'input_tokens-unknown'), '1 / 3');
  assert.equal(total(html, 'output_tokens-reported'), '10');
  assert.equal(total(html, 'output_tokens-estimated'), 'unknown');
  assert.equal(total(html, 'output_tokens-unknown'), '1 / 3');
  assert.match(html, /reported and estimated values are never combined/);
});
test('200 maximum safe counters aggregate exactly above Number precision without mutating DTOs', () => {
  const value = props(Array.from({ length: 200 }, () => ({ input_tokens: Number.MAX_SAFE_INTEGER, output_tokens: 0, estimated: false })));
  const original = structuredClone(value.runs), html = renderUsage(value);
  assert.equal(total(html, 'input_tokens-reported'), (BigInt(Number.MAX_SAFE_INTEGER) * 200n).toString());
  assert.equal(total(html, 'output_tokens-reported'), '0');
  assert.equal(total(html, 'input_tokens-unknown'), '0 / 200');
  assert.deepEqual(value.runs, original);
  assert.ok(html.indexOf('data-run-id="synthetic-199"') < html.indexOf('data-run-id="synthetic-0"'));
});
test('usage labels escape markup and controls; stale summaries do not imply provider availability', () => {
  const value = props([unknown]); value.sessions[0].label = '<script>usage</script>\u001b[31m';
  const html = renderUsage({ ...value, available: false, canInspect: false });
  assert.match(html, /Last loaded usage only/); assert.match(html, /&lt;script>usage&lt;\/script>/); assert.match(html, /\\u001b\[31m/);
  assert.doesNotMatch(html, /<script>|\u001b/); assert.match(html, /<button disabled[^>]*aria-label="Inspect usage/);
  assert.match(html, /Codex: codex_text_v1 · gpt-6\.1-sol; usage is reported by Codex, not a bill/); assert.match(html, /Claude: not supported in Workbench/);
});
test('routine usage reconciliation changes only aria-busy, not visible refresh markup', () => {
  const value = props([unknown]);
  assert.equal(renderUsage(value).replace('aria-busy="false"', ''), renderUsage({ ...value, refreshing: true }).replace('aria-busy="true"', ''));
});
