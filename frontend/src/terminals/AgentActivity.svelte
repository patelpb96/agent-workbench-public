<script lang="ts">
  // Compact, read-only view of what a launched agent is doing, from its own transcript: state, turns, token
  // usage, where the time goes, the busiest tools and the latest calls. Names, counts and timings only
  // (the inspector never reads prompts, replies or tool output).
  import { onDestroy, onMount } from 'svelte';
  import { fmtAgo, fmtDuration, share, type InspectorError, type InspectorReply } from '../inspector/client.js';
  import { inspectorClient } from '../inspector/tauri.js';

  interface Props { source: string | null; agent: string }
  let { source, agent }: Props = $props();

  let reply = $state.raw<InspectorReply | null>(null);
  let error = $state.raw<InspectorError | null>(null);
  let now = $state(Date.now());
  let generation = 0;
  let timer: ReturnType<typeof setInterval> | undefined;
  let tick: ReturnType<typeof setInterval> | undefined;

  const snap = $derived(reply?.snapshot ?? null);
  const lastLine = $derived(reply?.peek.last_line ?? null);
  const wall = $derived(snap ? snap.breakdown.tool_ms + snap.breakdown.model_ms + snap.breakdown.human_ms : 0);
  const stateText: Record<string, string> = { empty: 'No activity yet', working: 'Working', waiting_for_human: 'Waiting for you', idle: 'Idle' };
  const topTools = $derived(snap ? [...snap.tools].sort((a, b) => b.total_ms - a.total_ms).slice(0, 8) : []);

  async function load(id: string, gen: number): Promise<void> {
    if (document.hidden) return;
    try {
      const r = await inspectorClient.snapshot(id);
      if (gen !== generation) return;
      reply = r;
      error = null;
    } catch (e) {
      if (gen === generation) error = e as InspectorError;
    }
  }

  // Follow the source: a new id (or none yet) restarts the view.
  $effect(() => {
    const id = source;
    generation++;
    const gen = generation;
    reply = null;
    error = null;
    clearInterval(timer);
    if (id) {
      void load(id, gen);
      timer = setInterval(() => void load(id, gen), 2000);
    }
    return () => clearInterval(timer);
  });
  onMount(() => { tick = setInterval(() => { now = Date.now(); }, 1000); });
  onDestroy(() => { generation++; clearInterval(timer); clearInterval(tick); });
</script>

<div class="activity" aria-label={`${agent} activity`}>
  {#if !source}
    <p class="note">Waiting for the agent's transcript. It appears once the agent has started{agent === 'Codex' ? '' : ' and you have sent its first message'}. The terminal view works meanwhile.</p>
  {:else if error}
    <p class="note error" role="status"><strong>{error.code}</strong>: {error.message}</p>
  {:else if !snap}
    <p class="note">Reading the transcript…</p>
  {:else}
    <p class="last"><span class="k">LAST OUTPUT</span><span class="text">{lastLine ?? 'Nothing from the agent yet.'}</span></p>
    <div class="cards">
      <div class="card"><span class="k">State</span><strong class={`state ${snap.state}`}>{stateText[snap.state]}</strong><small>last activity {fmtAgo(snap.last_event_ms, now)}</small></div>
      <div class="card"><span class="k">Turns</span><strong>{snap.turns}</strong><small>{snap.open_calls} call{snap.open_calls === 1 ? '' : 's'} in flight</small></div>
      <div class="card"><span class="k">Tokens in / out</span><strong>{snap.usage.input ?? '?'} / {snap.usage.output ?? '?'}</strong><small>reported, not a bill</small></div>
    </div>
    {#if wall > 0}
      <div class="bar" role="img" aria-label={`Tools ${share(snap.breakdown.tool_ms, wall)}, model ${share(snap.breakdown.model_ms, wall)}, waiting on you ${share(snap.breakdown.human_ms, wall)}`}>
        <span class="seg tool" style={`flex:${snap.breakdown.tool_ms}`}></span><span class="seg model" style={`flex:${snap.breakdown.model_ms}`}></span><span class="seg human" style={`flex:${snap.breakdown.human_ms}`}></span>
      </div>
      <p class="legend">Tools {fmtDuration(snap.breakdown.tool_ms)} · Model {fmtDuration(snap.breakdown.model_ms)} · Waiting on you {fmtDuration(snap.breakdown.human_ms)}</p>
    {/if}
    <h3>Busiest tools</h3>
    {#if topTools.length === 0}
      <p class="note">No tool calls yet.</p>
    {:else}
      <table>
        <thead><tr><th>Tool</th><th>Calls</th><th>Errors</th><th>Time</th><th>Last used</th></tr></thead>
        <tbody>
          {#each topTools as t (t.name)}
            <tr><td class="name">{t.name}</td><td>{t.calls}</td><td class:bad={t.errors > 0}>{t.errors}</td><td>{fmtDuration(t.total_ms)}</td><td>{fmtAgo(t.last_used_ms, now)}</td></tr>
          {/each}
        </tbody>
      </table>
    {/if}
    <h3>Latest calls</h3>
    {#if snap.recent.length === 0}
      <p class="note">None yet.</p>
    {:else}
      <ol class="recent">
        {#each snap.recent.slice(0, 8) as c, i (i)}
          <li><span class="r-tool">{c.tool}</span><span>{fmtAgo(c.started_ms, now)}</span><span>{fmtDuration(c.duration_ms)}</span><span class:bad={c.ok === false}>{c.ok === null ? 'outcome unknown' : c.ok ? 'ok' : 'error'}</span></li>
        {/each}
      </ol>
    {/if}
  {/if}
</div>

<style>
  .activity { height: 100%; overflow: auto; padding: 8px 10px; font-size: 12px; color: var(--text, #e4ebf5); background: #0b121a; }
  .note { margin: 6px 0; color: #a5b3c6; line-height: 1.5; }
  .note.error { color: #f3cfc7; }
  .last { display: flex; gap: .8em; align-items: baseline; margin: 0 0 8px; padding: .4em .6em; border: 1px solid #344256; background: #101923; }
  .last .k { flex: none; font: 9px ui-monospace, monospace; letter-spacing: 1px; color: #a5b3c6; }
  .last .text { font: 12px ui-monospace, monospace; overflow-wrap: anywhere; }
  .cards { display: flex; gap: 8px; flex-wrap: wrap; }
  .card { flex: 1 1 110px; border: 1px solid #344256; padding: 6px 8px; display: flex; flex-direction: column; gap: 2px; }
  .card .k { font: 9px ui-monospace, monospace; letter-spacing: .8px; color: #a5b3c6; }
  .card small { color: #a5b3c6; font-size: 10px; }
  .state.working { color: #80c49b; }
  .state.waiting_for_human { color: #e5bc6e; }
  .bar { display: flex; height: 10px; margin: 10px 0 4px; border: 1px solid #344256; }
  .seg.tool { background: #8abaff; } .seg.model { background: #b7a2e0; } .seg.human { background: #e5bc6e; }
  .legend { margin: 2px 0 8px; color: #a5b3c6; font-size: 11px; }
  h3 { margin: 10px 0 4px; font: 10px ui-monospace, monospace; letter-spacing: 1px; color: #a5b3c6; font-weight: 400; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 2px 6px 2px 0; font-size: 11px; white-space: nowrap; }
  th { color: #a5b3c6; font-weight: 400; }
  td.name { overflow: hidden; text-overflow: ellipsis; max-width: 160px; }
  .bad { color: #e29787; }
  .recent { list-style: none; margin: 0; padding: 0; }
  .recent li { display: flex; gap: 10px; padding: 1px 0; font-size: 11px; }
  .r-tool { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
</style>
