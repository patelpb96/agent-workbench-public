<script lang="ts">
  import { displayText } from '../execution/reconcile.js';
  import { limits } from '../execution-v2/generated.js';
  import type { RunSnapshot, SessionSnapshot } from '../execution-v2/generated.js';
  interface Props {
    sessions: readonly SessionSnapshot[];
    runs: readonly RunSnapshot[];
    loaded: boolean;
    available: boolean;
    canInspect: boolean;
    refreshing: boolean;
    onInspect: (sessionId: string, runId: string) => void;
  }
  let { sessions, runs, loaded, available, canInspect, refreshing, onInspect }: Props = $props();
  let sessionFilter = $state('');
  const visible = $derived(runs.filter(run => !sessionFilter || run.session_id === sessionFilter).sort((a, b) => b.enqueue_order - a.enqueue_order));
  function subtotal(key: 'input_tokens' | 'output_tokens', estimated: boolean) {
    const known = visible.filter(run => run.usage.estimated === estimated && run.usage[key] !== null);
    // Individual wire counters are safe integers, but their sum need not be.
    // Keep exact arithmetic local to display; never change or serialize DTOs.
    const sum = known.reduce((total, run) => total + BigInt(run.usage[key]!), 0n);
    return { count: known.length, value: known.length ? sum.toString() : 'unknown' };
  }
  const totals = $derived((['input_tokens', 'output_tokens'] as const).map(key => ({
    key, title: key === 'input_tokens' ? 'Input tokens' : 'Output tokens',
    reported: subtotal(key, false), estimated: subtotal(key, true),
    unknown: visible.filter(run => run.usage[key] === null).length
  })));
  function sessionLabel(id: string) {
    return displayText(sessions.find(session => session.session_id === id)?.label ?? 'Unavailable session');
  }
  function quality(run: RunSnapshot) {
    return run.usage.input_tokens === null && run.usage.output_tokens === null ? 'unreported' : run.usage.estimated ? 'estimated' : 'reported';
  }
</script>

<section class="usage-overview" aria-label="Models and run usage" aria-busy={refreshing}>
  <header>
    <span class="badge simulated">STORED RUN USAGE</span>
    <h2>Models &amp; run usage</h2>
    <p>Latest stored usage snapshot per run, not a sum of transcript events. Known subtotals are partial when values are unknown; reported and estimated values are never combined.</p>
    <p class="authority">{!loaded ? 'No native usage data loaded.' : available ? 'Loaded native summaries · updates follow workspace reconciliation.' : 'Last loaded usage only · current execution availability is unverified.'}</p>
    <label>Filter usage by session<select bind:value={sessionFilter} disabled={!loaded || sessions.length === 0}>
      <option value="">All sessions</option>
      {#each sessions as session (session.session_id)}<option value={session.session_id}>{displayText(session.label)}</option>{/each}
    </select></label>
    <p class="coverage">{visible.length} shown / {runs.length} retained / {limits.workspace_runs} maximum. Null means unknown, not zero. Prompts and transcript bodies are not displayed.</p>
  </header>
  {#if loaded}
    {#if runs.length === 0}<p>No runs yet; no usage data to total.</p>{/if}
    <div class="usage-totals">
      {#each totals as total (total.key)}
        <section class="usage-total" aria-label={total.title}>
          <h3>{total.title}</h3>
          <dl>
            <dt>Reported subtotal</dt><dd data-total={`${total.key}-reported`}>{total.reported.value}</dd>
            <dt>Runs with reported value</dt><dd>{total.reported.count} / {visible.length}</dd>
            <dt>Estimated subtotal</dt><dd data-total={`${total.key}-estimated`}>{total.estimated.value}</dd>
            <dt>Runs with estimated value</dt><dd>{total.estimated.count} / {visible.length}</dd>
            <dt>Unknown values</dt><dd data-total={`${total.key}-unknown`}>{total.unknown} / {visible.length}</dd>
          </dl>
        </section>
      {/each}
    </div>
    <div class="usage-table">
      <table>
        <caption>Latest per-run usage · newest enqueued first</caption>
        <thead><tr><th scope="col">Session / run</th><th scope="col">Input</th><th scope="col">Output</th><th scope="col">Value kind</th><th scope="col">History</th></tr></thead>
        <tbody>
          {#each visible as run (run.run_id)}
            <tr data-run-id={run.run_id}>
              <th scope="row"><strong>{sessionLabel(run.session_id)}</strong><small>#{run.enqueue_order} · {run.state} · <code>{run.run_id}</code></small></th>
              <td>{run.usage.input_tokens ?? 'unknown'}</td><td>{run.usage.output_tokens ?? 'unknown'}</td><td>{quality(run)}</td>
              <td><button disabled={!canInspect} aria-label={`Inspect usage run #${run.enqueue_order} in ${sessionLabel(run.session_id)}`} onclick={() => onInspect(run.session_id, run.run_id)}>Inspect #{run.enqueue_order}</button></td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if visible.length === 0}<p>No usage rows in this selection.</p>{/if}
    </div>
  {/if}
  <footer>
    <h3>Provider boundaries — no live catalog</h3>
    <ul><li>Fake: simulated local runner; availability follows workspace status.</li><li>Codex: codex_text_v1 · gpt-6.1-sol; usage is reported by Codex, not a bill.</li><li>Claude: not supported in Workbench.</li></ul>
    <p>Model identifiers, prices, billing totals, subscription quotas and account limits are unavailable. No catalog, credential or network probe is performed.</p>
  </footer>
</section>

<style>
  .usage-overview{position:absolute;inset:0;z-index:10;overflow:auto;background:var(--view-bg,var(--bg));color:var(--text);padding:24px}
  header{max-width:900px}h2{margin:14px 0 8px}p{color:var(--muted);line-height:1.7}label{max-width:420px}
  .authority,.coverage,footer{font-size:11px}.authority{border-inline-start:2px solid var(--line);padding-inline-start:10px}
  .usage-totals{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,250px),1fr));gap:12px;margin:20px 0}
  .usage-total{border:1px solid var(--line);padding:16px;background:var(--surface)}h3{font-size:13px;font-weight:500}
  dl{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:10px;font-size:11px}dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere;font:12px ui-monospace,monospace}
  .usage-table{overflow-x:auto}table{width:100%;border-collapse:collapse;min-width:540px;font-size:11px}caption{text-align:left;padding:10px 0;color:var(--muted)}
  th,td{text-align:left;vertical-align:top;padding:12px 8px;border-bottom:1px solid var(--line)}tbody th{font-weight:400;max-width:260px;overflow-wrap:anywhere}
  strong{font-weight:500}small{display:block;color:var(--muted);margin-top:6px;line-height:1.6}code{font-size:10px}td button{font-size:11px}
  footer{border-top:1px solid var(--line);margin-top:24px;padding-top:12px}footer li{margin:8px 0;color:var(--muted)}
</style>
