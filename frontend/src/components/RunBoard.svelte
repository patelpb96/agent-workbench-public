<script lang="ts">
  import { displayText } from '../execution/reconcile.js';
  import { limits, runTransitions } from '../execution-v2/generated.js';
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
  const visible = $derived(runs.filter(run => !sessionFilter || run.session_id === sessionFilter));
  const groups = $derived([
    { id: 'active', title: 'Active', items: visible.filter(run => run.state !== 'queued' && runTransitions[run.state].length > 0).sort((a, b) => a.enqueue_order - b.enqueue_order) },
    { id: 'queued', title: 'Queued · FIFO order', items: visible.filter(run => run.state === 'queued').sort((a, b) => a.enqueue_order - b.enqueue_order) },
    { id: 'history', title: 'Retained history · newest first', items: visible.filter(run => runTransitions[run.state].length === 0).sort((a, b) => b.enqueue_order - a.enqueue_order) }
  ]);
  function sessionLabel(id: string) {
    return displayText(sessions.find(session => session.session_id === id)?.label ?? 'Unavailable session');
  }
</script>

<section class="run-board" aria-label="Run queue and history" aria-busy={refreshing}>
  <header>
    <span class="badge simulated">NATIVE RUN SUMMARIES</span>
    <h2>Run queue &amp; history</h2>
    <p>Inspect existing work; this board does not submit, cancel or schedule runs. Automatic handoffs and real providers remain disabled.</p>
    <p class="authority">{!loaded ? 'No native run data loaded.' : available ? 'Loaded native summaries · updates follow workspace reconciliation.' : 'Last loaded summaries only · current execution availability is unverified.'}</p>
    <label>Filter runs by session<select bind:value={sessionFilter} disabled={!loaded || sessions.length === 0}>
      <option value="">All sessions</option>
      {#each sessions as session (session.session_id)}<option value={session.session_id}>{displayText(session.label)}</option>{/each}
    </select></label>
    <p class="totals">{visible.length} shown / {runs.length} retained / {limits.workspace_runs} maximum. Private prompts and transcript bodies are not displayed here.</p>
  </header>
  {#if loaded}
    {#if runs.length === 0}<p class="empty">No runs yet. Use the Canvas composer for an explicit simulated run.</p>{/if}
    {#each groups as group (group.id)}
      <section class="run-group" data-group={group.id} aria-label={group.title}>
        <h3>{group.title} <span>{group.items.length}</span></h3>
        {#if group.items.length === 0}<p class="empty">No runs in this group.</p>{/if}
        <ul>
          {#each group.items as run (run.run_id)}
            <li data-run-id={run.run_id} data-state={run.state}>
              <div class="run-heading"><strong>{sessionLabel(run.session_id)}</strong><span class="run-state">{run.state}</span></div>
              <p class="run-meta">Run #{run.enqueue_order} · <code>{run.run_id}</code> · last sequence {run.last_seq}</p>
              <p class="run-meta">Updated <time datetime={run.updated_at}>{run.updated_at}</time> · input tokens {run.usage.input_tokens ?? 'unknown'} · output tokens {run.usage.output_tokens ?? 'unknown'}{run.usage.estimated ? ' (estimated)' : ''}</p>
              <button disabled={!canInspect} aria-label={`Inspect run #${run.enqueue_order} in ${sessionLabel(run.session_id)}`} onclick={() => onInspect(run.session_id, run.run_id)}>Inspect run #{run.enqueue_order}</button>
            </li>
          {/each}
        </ul>
      </section>
    {/each}
  {/if}
  <footer><h3>Handoffs — not connected</h3><p>No folder watcher, task manifest import, automatic dispatch or genuine approval action is enabled.</p><button disabled>Automatic handoff — unavailable</button></footer>
</section>

<style>
  .run-board{position:absolute;inset:0;z-index:10;overflow:auto;background:var(--view-bg,var(--bg));padding:24px;color:var(--text)}
  header{max-width:900px}h2{margin:14px 0 8px}p{line-height:1.7;color:var(--muted)}
  label{max-width:420px}.authority,.totals,.run-meta,.empty{font-size:11px}.authority{border-inline-start:2px solid var(--line);padding-inline-start:10px}
  .run-group,footer{margin-top:24px;border-top:1px solid var(--line);padding-top:12px}h3{font-size:13px;font-weight:500}h3 span{color:var(--muted);margin-left:8px}
  ul{list-style:none;padding:0;margin:12px 0;display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,320px),1fr));gap:10px}
  li{border:1px solid var(--line);background:var(--surface);padding:14px;min-width:0;overflow-wrap:anywhere}
  .run-heading{display:flex;gap:10px;align-items:start;justify-content:space-between}.run-heading strong{font-weight:500;min-width:0}
  .run-state{font:10px ui-monospace,monospace;border:1px solid var(--line);padding:3px;flex-shrink:0}
  .run-meta{margin:8px 0}li button,footer button{font-size:11px}code{font-size:10px}footer p{font-size:11px}
</style>
