<script lang="ts">
  import { displayText } from '../execution/reconcile.js';
  import { limits } from '../execution-v2/generated.js';
  import type { RunSnapshot, SessionSnapshot } from '../execution-v2/generated.js';
  import type { EventWindow } from './event-metadata.js';
  interface Props {
    sessions: readonly SessionSnapshot[];
    runs: readonly RunSnapshot[];
    selectedRunId?: string;
    window: EventWindow | null;
    loaded: boolean;
    available: boolean;
    canRead: boolean;
    refreshing: boolean;
    onSelect: (sessionId: string, runId: string) => void;
    onEarlier: () => void;
    onLive: () => void;
  }
  let { sessions, runs, selectedRunId, window, loaded, available, canRead, refreshing, onSelect, onEarlier, onLive }: Props = $props();
  const ordered = $derived([...runs].sort((a, b) => b.enqueue_order - a.enqueue_order));
  const selected = $derived(runs.find(run => run.run_id === selectedRunId));
  const view = $derived(window?.run_id === selected?.run_id ? window : null);
  const headers = $derived(view?.loaded ? view.headers : []);
  function sessionLabel(id: string) {
    return displayText(sessions.find(session => session.session_id === id)?.label ?? 'Unavailable session');
  }
  function select(id: string) {
    const run = runs.find(run => run.run_id === id);
    if (run) onSelect(run.session_id, run.run_id);
  }
</script>

<section class="event-timeline" aria-label="Selected-run event metadata" aria-busy={refreshing}>
  <header>
    <span class="badge simulated">METADATA ONLY</span>
    <h2>Events &amp; approval boundaries</h2>
    <p>One selected run, one existing bounded history buffer. Event payloads, transcript text, prompts and tool arguments are not displayed here.</p>
    <p class="authority">{!loaded ? 'No native workspace data loaded.' : available ? 'Loaded native history · updates follow workspace reconciliation.' : 'Last loaded history only · current execution availability is unverified.'}</p>
    <label>Run for event metadata<select value={selectedRunId ?? ''} disabled={!canRead || runs.length === 0} onchange={event => select(event.currentTarget.value)}>
      <option value="" disabled>No run selected</option>
      {#each ordered as run (run.run_id)}<option value={run.run_id}>{sessionLabel(run.session_id)} · #{run.enqueue_order} · {run.state}</option>{/each}
    </select></label>
  </header>
  {#if loaded && runs.length === 0}<p>No runs yet. This view never starts work.</p>{/if}
  {#if selected}
    <p class="selection">{sessionLabel(selected.session_id)} · Run #{selected.enqueue_order} · <code>{selected.run_id}</code> · {selected.state}</p>
    {#if view}
      <p class="high-water">Native high-water #{view.known_last_seq}; summary sequence #{view.summary_seq}. Neither is evidence that all events are loaded.</p>
      <div class="history-controls">
        <span class="badge" data-view={view.view}>{view.view === 'live' ? 'LIVE TAIL' : 'HISTORY VIEW'}</span>
        <button disabled={!canRead || !view.loaded || view.pending || view.omitted.before === null || view.problem_code !== null} onclick={onEarlier}>Load earlier event metadata</button>
        <button disabled={!canRead || view.pending || (view.view === 'live' && view.loaded && view.problem_code === null)} onclick={onLive}>Return to live event metadata</button>
      </div>
      {#if view.problem_code}<p class="problem">History problem: {displayText(view.problem_code)}. Refresh or return to live; nothing is retried as a mutation.</p>{/if}
      {#if view.omitted.message}<p class="omission">{view.omitted.message}</p>{/if}
      {#if !view.loaded}<p>Selected run history has not been loaded. No event range is claimed.</p>
      {:else}
        <p class="coverage">{headers.length ? `Loaded metadata #${headers[0].seq}–#${headers[headers.length - 1].seq}` : 'No event metadata loaded'} · {headers.length}/{limits.visible_events} rows maximum. The existing transcript text-byte budget still applies.</p>
        <ol aria-label="Loaded event metadata">
          {#each headers as event (event.event_id)}
            <li value={event.seq} data-event-id={event.event_id} data-kind={event.kind} data-source={event.source}>
              <strong>#{event.seq} · {event.kind}</strong><span>Source: {event.source}</span>
              <time datetime={event.occurred_at}>{event.occurred_at}</time><code>{event.event_id}</code>
            </li>
          {/each}
        </ol>
      {/if}
    {:else}<p>Selected run history is unavailable or does not match this selection. No other run's events are shown.</p>{/if}
  {/if}
  <footer>
    <h3>Genuine approvals — unavailable</h3>
    <p>Simulated tool activity is display data, not an approval request or permission to execute tools. Codex runs use a read-only, no-tools profile. Approval responses and automatic handoffs remain disabled.</p>
    <button disabled>Approve — unavailable</button> <button disabled>Deny — unavailable</button>
  </footer>
</section>

<style>
  .event-timeline{position:absolute;inset:0;z-index:10;overflow:auto;background:var(--view-bg,var(--bg));color:var(--text);padding:24px}
  header{max-width:900px}h2{margin:14px 0 8px}p{line-height:1.7;color:var(--muted)}label{max-width:620px}
  .authority,.selection,.high-water,.coverage,.omission,.problem,footer{font-size:11px}.authority,.omission,.problem{border-inline-start:2px solid var(--line);padding-inline-start:10px}
  .selection{overflow-wrap:anywhere}.history-controls{display:flex;gap:8px;align-items:center;flex-wrap:wrap}.history-controls button,footer button{font-size:11px}
  ol{list-style:none;padding:0;margin:16px 0}li{border:1px solid var(--line);background:var(--surface);padding:12px;margin:8px 0;display:flex;gap:8px 16px;flex-wrap:wrap;align-items:center;overflow-wrap:anywhere}
  li strong{font-size:12px;font-weight:500}li span,time{font-size:11px;color:var(--muted)}li code{font-size:10px;flex-basis:100%;color:var(--muted)}
  footer{border-top:1px solid var(--line);margin-top:24px;padding-top:12px}h3{font-size:13px;font-weight:500}
</style>
