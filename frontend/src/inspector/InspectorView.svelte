<script lang="ts">
  // The agent inspector: every session found on this machine as a small tile, grouped by state in tabs
  // (All, Active, Idle, Inactive, Other), sortable by agent, model or folder, with the last line each one wrote.
  // Selecting a tile shows the rest below it. The grouping and colours are pure functions in groups.ts, so a
  // future agent dashboard can show the same sessions the same way.
  import { onDestroy, onMount } from 'svelte';
  import { fmtAgo, fmtDuration, share, sortTools, type InspectorError, type InspectorReply, type SourceInfo } from './client.js';
  import { inspectorAvailable, inspectorClient } from './tauri.js';
  import { inspectorTags } from './tags-store.svelte.js';
  import { AGENT_COLOR, SORTS, TABS, TAG_COLOR, TAG_NAMES, agentName, classify, counts, displayName, inTab, modelColor, sections, shortModel, type SortKey as GroupSort, type TabId, type TagName } from './groups.js';
  import SessionTile from './SessionTile.svelte';
  import MapView from './MapView.svelte';
  import { inspectorView, MAP_HEIGHT_DEFAULT, MAP_HEIGHT_MIN } from './view-store.svelte.js';
  import { canvasTerminals } from '../terminals/canvas-store.svelte.js';
  import { floatWatch, isWatching } from '../terminals/floating.js';
  import { agentStore as agents } from '../terminals/agent-store.svelte.js';
  import ContextMenu, { type MenuItem } from '../ContextMenu.svelte';

  type SortKey = 'name' | 'calls' | 'errors' | 'last_used_ms' | 'total_ms' | 'median_ms';

  let menu = $state<{ x: number; y: number; source: SourceInfo } | null>(null);
  function openMenu(event: MouseEvent, source: SourceInfo): void {
    event.preventDefault();
    menu = { x: event.clientX, y: event.clientY, source };
  }
  const watch = (s: SourceInfo): void => canvasTerminals.commit(floatWatch(canvasTerminals.state, s.id, s.agent, s.label, { w: 1400, h: 800 }));
  const openInTerminal = (s: SourceInfo): Promise<void> => agents.openInTerminal(s.id, s.agent, displayName(s));
  const menuItems = (s: SourceInfo): MenuItem[] => {
    const tagged = inspectorTags.colorOf(s.id);
    return [
      { label: 'Open in terminal', hint: 'Resume this session in a new Workbench terminal', disabled: !inspectorAvailable() || agents.opening || s.kind === 'subagent', onselect: () => void openInTerminal(s) },
      { label: isWatching(canvasTerminals.state, s.id) ? 'Already watching' : 'Watch on the Canvas', disabled: isWatching(canvasTerminals.state, s.id), onselect: () => watch(s) },
      ...TAG_NAMES.map(name => ({ label: `Tag ${name}`, disabled: tagged === name, onselect: () => void inspectorTags.set(s.id, name) })),
      ...(tagged ? [{ label: 'Remove tag', onselect: () => void inspectorTags.set(s.id, null) }] : [])
    ];
  };

  let sources = $state.raw<SourceInfo[]>([]);
  // The selection and the Tiles | Map choice live in a module-level store, so they survive another rail item being opened.
  const selected = $derived(inspectorView.selected);
  let reply = $state.raw<InspectorReply | null>(null);
  let error = $state.raw<InspectorError | null>(null);
  let filter = $state<'all' | 'claude' | 'pi' | 'codex'>('all');
  let tab = $state<TabId>('all');
  let sort = $state<GroupSort>('newest');
  let sortKey = $state<SortKey>('total_ms');
  let sortDesc = $state(true);
  let now = $state(Date.now());

  let listTimer: ReturnType<typeof setInterval> | undefined;
  let snapTimer: ReturnType<typeof setInterval> | undefined;
  let tick: ReturnType<typeof setInterval> | undefined;
  let generation = 0;

  const available = inspectorAvailable();
  const pool = $derived(sources.filter(s => filter === 'all' || s.agent === filter));
  const tabCounts = $derived(counts(pool, now));
  const grouped = $derived(sections(inTab(pool, tab, now), sort));
  const current = $derived(sources.find(s => s.id === selected) ?? null);
  const names = $derived(new Map(sources.map(s => [s.id, displayName(s)])));
  const parentNameOf = (s: SourceInfo): string | null => (s.parent ? (names.get(s.parent) ?? null) : null);
  const SUBAGENT_NOTE = 'A subagent cannot be resumed on its own; open its parent session instead';
  const currentTag = $derived(current ? inspectorTags.colorOf(current.id) : null);
  // The freshest line and model: from the open snapshot when it is for this session, else from the list.
  const peek = $derived(reply && current && reply.source.id === current.id ? reply.peek : null);
  const snap = $derived(reply?.snapshot ?? null);
  const rows = $derived(snap ? sortTools(snap.tools, sortKey, sortDesc) : []);
  const toolTotal = $derived(snap ? snap.tools.reduce((a, t) => a + t.total_ms, 0) : 0);
  const wall = $derived(snap ? snap.breakdown.tool_ms + snap.breakdown.model_ms + snap.breakdown.human_ms : 0);
  const peak = $derived(snap?.timeline ? Math.max(1, ...snap.timeline.buckets.map(b => b.calls)) : 1);
  const stateText: Record<string, string> = { empty: 'No activity yet', working: 'Working', waiting_for_human: 'Waiting for you', idle: 'Idle' };

  async function loadSources(): Promise<void> {
    try {
      sources = await inspectorClient.sources();
      if (error?.code === 'inspector_unavailable') error = null;
    } catch (e) {
      error = e as InspectorError;
    }
  }

  async function loadSnapshot(id: string, gen: number): Promise<void> {
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

  function pick(id: string): void {
    generation++;
    inspectorView.selected = id;
    reply = null;
    const gen = generation;
    void loadSnapshot(id, gen);
  }

  function tabKeys(event: KeyboardEvent): void {
    const step = ({ ArrowRight: 1, ArrowLeft: -1 } as Record<string, number>)[event.key];
    const at = TABS.findIndex(t => t.id === tab);
    const to = event.key === 'Home' ? 0 : event.key === 'End' ? TABS.length - 1 : step === undefined ? -1 : (at + step + TABS.length) % TABS.length;
    if (to < 0) return;
    event.preventDefault();
    tab = TABS[to].id;
    queueMicrotask(() => document.getElementById(`ins-tab-${tab}`)?.focus());
  }

  const MODES = [{ id: 'tiles', label: 'Tiles' }, { id: 'map', label: 'Map' }] as const;
  function modeKeys(event: KeyboardEvent): void {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight' && event.key !== 'Home' && event.key !== 'End') return;
    event.preventDefault();
    const to = event.key === 'ArrowLeft' || event.key === 'Home' ? MODES[0] : MODES[1];
    inspectorView.mode = to.id;
    queueMicrotask(() => document.getElementById(`ins-mode-${to.id}`)?.focus());
  }

  // The bar under the map sets its height (drag, arrow keys, double-click for the default); the map scales to fill it.
  // At least the minimum, and always leaving room for the session details below.
  let bodyHeight = $state(0);
  const mapRoom = $derived(Math.max(MAP_HEIGHT_MIN, bodyHeight - 140));
  const mapHeight = $derived(Math.min(inspectorView.mapHeight, mapRoom));
  let mapDrag: { y: number; height: number } | null = null;
  function splitStart(event: PointerEvent): void {
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    mapDrag = { y: event.clientY, height: mapHeight };
    event.preventDefault();
  }
  function splitMove(event: PointerEvent): void {
    if (mapDrag) inspectorView.mapHeight = Math.min(mapRoom, mapDrag.height + event.clientY - mapDrag.y);
  }
  function splitEnd(event: PointerEvent): void {
    (event.currentTarget as HTMLElement).releasePointerCapture?.(event.pointerId);
    mapDrag = null;
  }
  function splitKeys(event: KeyboardEvent): void {
    const step = ({ ArrowUp: -24, ArrowDown: 24, PageUp: -120, PageDown: 120, Home: -Infinity, End: Infinity } as Record<string, number>)[event.key];
    if (step === undefined) return;
    event.preventDefault();
    inspectorView.mapHeight = Math.max(MAP_HEIGHT_MIN, Math.min(mapRoom, mapHeight + step));
  }

  function sortBy(key: SortKey): void {
    if (sortKey === key) sortDesc = !sortDesc;
    else { sortKey = key; sortDesc = key !== 'name'; }
  }
  const arrow = (key: SortKey): string => sortKey === key ? (sortDesc ? ' ▼' : ' ▲') : '';

  onMount(() => {
    void loadSources();
    void inspectorTags.load();
    if (inspectorView.selected) pick(inspectorView.selected);
    listTimer = setInterval(() => void loadSources(), 5000);
    snapTimer = setInterval(() => { if (selected) void loadSnapshot(selected, generation); }, 2000);
    tick = setInterval(() => { now = Date.now(); }, 1000);
  });
  onDestroy(() => {
    generation++;
    clearInterval(listTimer);
    clearInterval(snapTimer);
    clearInterval(tick);
  });
</script>

<section class="inspector" aria-label="Agent inspector">
  <header class="ins-head">
    <div>
      <span class="eyebrow">INSPECTOR</span>
      <h1>Agent inspector</h1>
    </div>
    <div class="ins-controls">
      <div class="ins-modes" role="tablist" aria-label="Inspector view">
        {#each MODES as m (m.id)}
          <button type="button" role="tab" id={`ins-mode-${m.id}`} aria-selected={inspectorView.mode === m.id} tabindex={inspectorView.mode === m.id ? 0 : -1} class:on={inspectorView.mode === m.id}
            onclick={() => { inspectorView.mode = m.id; }} onkeydown={modeKeys}>{m.label}</button>
        {/each}
      </div>
      <label class="ins-filter">Agent
        <select bind:value={filter} aria-label="Filter by agent">
          <option value="all">All agents</option>
          <option value="claude">Claude Code</option>
          <option value="pi">Pi</option>
          <option value="codex">Codex</option>
        </select>
      </label>
      <label class="ins-filter">Sort
        <select bind:value={sort} aria-label="Sort sessions by">
          {#each SORTS as option (option.id)}<option value={option.id}>{option.label}</option>{/each}
        </select>
      </label>
    </div>
  </header>
  <p class="ins-hint">
    Read-only. Counts, timings, tool names and reported token usage come from each agent's own transcript file. The one line of text shown is the last line the
    agent wrote, read from the end of its transcript so you can tell sessions apart; it is shown only here and on a session's Activity, never saved or logged.
    The state groups and the time split are heuristics.
  </p>
  {#if !available}<div class="ins-note">The inspector is available in the desktop app only.</div>{/if}
  {#if error}<div class="ins-note error" role="status"><strong>{error.code}</strong>: {error.message}</div>{/if}

  <div class="ins-body" bind:clientHeight={bodyHeight}>
    <!-- The same state groups choose what the Map shows, so "Active" is the agents working now. -->
    <div class="ins-tabs" role="tablist" aria-label="Session state">
      {#each TABS as t (t.id)}
        <button type="button" role="tab" id={`ins-tab-${t.id}`} aria-selected={tab === t.id} aria-controls={inspectorView.mode === 'map' ? 'ins-map' : 'ins-tiles'} tabindex={tab === t.id ? 0 : -1} title={t.hint}
          class:on={tab === t.id} class:zero={tabCounts[t.id] === 0} onclick={() => { tab = t.id; }} onkeydown={tabKeys}>
          {t.label}<span class="count">{tabCounts[t.id]}</span>
        </button>
      {/each}
    </div>

    {#if inspectorView.mode === 'map'}
      <div class="ins-map" id="ins-map" role="tabpanel" aria-labelledby={`ins-tab-${tab}`} style={`height:${mapHeight}px`}>
        <MapView sources={inTab(pool, tab, now)} {now} {selected} onSelect={pick} />
        {#if pool.length === 0}<p class="ins-empty">No transcripts found for this filter.</p>{:else if tabCounts[tab] === 0}<p class="ins-empty">No sessions in this group.</p>{/if}
      </div>
      <!-- The window-splitter pattern: a focusable separator, resizable by pointer and keyboard. -->
      <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
      <div class="ins-split" role="separator" aria-orientation="horizontal" tabindex="0"
        aria-label="Map height: drag, or use the arrow keys; double-click for the default" aria-valuemin={MAP_HEIGHT_MIN} aria-valuemax={mapRoom} aria-valuenow={mapHeight}
        onpointerdown={splitStart} onpointermove={splitMove} onpointerup={splitEnd} onpointercancel={splitEnd} onkeydown={splitKeys}
        ondblclick={() => { inspectorView.mapHeight = MAP_HEIGHT_DEFAULT; }}></div>
    {:else}
    <div class="ins-tiles" id="ins-tiles" role="tabpanel" aria-labelledby={`ins-tab-${tab}`}>
      {#each grouped as section (section.key)}
        {#if section.label}<h2 class="sec">{section.label} <small>{section.items.length}</small></h2>{/if}
        <div class="tiles">
          {#each section.items as s (s.id)}
            <SessionTile source={s} {now} selected={s.id === selected} tag={inspectorTags.colorOf(s.id)} parentName={parentNameOf(s)} onSelect={() => pick(s.id)} onMenu={event => openMenu(event, s)} />
          {/each}
        </div>
      {/each}
      {#if grouped.length === 0}<p class="ins-empty">{pool.length === 0 ? 'No transcripts found for this filter.' : 'No sessions in this group.'}</p>{/if}
    </div>
    {/if}

    <div class="ins-detail">
      {#if current}
        <div class="sel" style={`--stripe:${currentTag ? TAG_COLOR[currentTag] : AGENT_COLOR[current.agent]}`}>
          <div class="sel-main">
            <div class="sel-title">
              <span class={`dot ${classify(current, now)}`} role="img" aria-label={classify(current, now)}></span>
              <strong>{displayName(current, parentNameOf(current))}</strong>
              <span class="chip" style={`color:${AGENT_COLOR[current.agent]}`}>{agentName(current.agent)}</span>
              {#if peek?.model ?? current.model}{@const model = (peek?.model ?? current.model)!}<span class="chip" style={`color:${modelColor(model)}`} title={model}>{shortModel(model)}</span>{/if}
              {#if current.running}<span class="chip running">{current.running === 'background' ? 'running in the background' : 'open in another window'}</span>{/if}
              <small>{fmtAgo(current.modified_ms, now)} · {(current.bytes / 1024).toFixed(0)} KB</small>
            </div>
            <p class="sel-line"><span class="k">LAST OUTPUT</span><span class="text">{peek?.last_line ?? current.last_line ?? 'Nothing from the agent yet.'}</span></p>
          </div>
          <div class="sel-actions">
            <button type="button" disabled={!available || agents.opening || current.kind === 'subagent'} title={current.kind === 'subagent' ? SUBAGENT_NOTE : 'Resume this session in a new Workbench terminal, linked to its Activity (close the original first if it is still running)'} onclick={() => void openInTerminal(current)}>Open in terminal</button>
            <button type="button" disabled={isWatching(canvasTerminals.state, current.id)} title="Open this session's Activity in a floating window on the Canvas (the agent keeps running where it is)" onclick={() => watch(current)}>{isWatching(canvasTerminals.state, current.id) ? 'Watching' : 'Watch'}</button>
            <span class="tags" role="group" aria-label="Colour tag">
              {#each TAG_NAMES as name (name)}
                <button type="button" class="swatch" aria-label={`Tag ${name}`} aria-pressed={currentTag === name} title={`Tag ${name}`} style={`--swatch:${TAG_COLOR[name]}`} onclick={() => void inspectorTags.set(current.id, currentTag === name ? null : name)}></button>
              {/each}
            </span>
          </div>
        </div>
      {/if}
      {#if !current}
        <p class="ins-empty">Select a session above to see its details.</p>
      {:else if !snap}
        <p class="ins-empty">Reading transcript…</p>
      {:else}
        <div class="cards">
          <div class="card"><span class="k">State</span><strong class={`state ${snap.state}`}>{stateText[snap.state]}</strong><small>last activity {fmtAgo(snap.last_event_ms, now)}</small></div>
          <div class="card"><span class="k">Turns</span><strong>{snap.turns}</strong><small>{snap.open_calls} tool call{snap.open_calls === 1 ? '' : 's'} in flight</small></div>
          <div class="card"><span class="k">Tokens in / out</span><strong>{snap.usage.input ?? '?'} / {snap.usage.output ?? '?'}</strong><small>cache read {snap.usage.cache_read ?? 'unknown'}, reasoning {snap.usage.reasoning ?? 'unknown'}</small></div>
          <div class="card"><span class="k">Session span</span><strong>{snap.first_event_ms !== null && snap.last_event_ms !== null ? fmtDuration(snap.last_event_ms - snap.first_event_ms) : 'unknown'}</strong><small>reported usage, not a bill</small></div>
        </div>

        <h2>Where the time goes</h2>
        {#if wall > 0}
          <div class="bar" role="img" aria-label={`Tool ${share(snap.breakdown.tool_ms, wall)}, model ${share(snap.breakdown.model_ms, wall)}, waiting on you ${share(snap.breakdown.human_ms, wall)}`}>
            <span class="seg tool" style={`flex:${snap.breakdown.tool_ms}`}></span>
            <span class="seg model" style={`flex:${snap.breakdown.model_ms}`}></span>
            <span class="seg human" style={`flex:${snap.breakdown.human_ms}`}></span>
          </div>
          <p class="legend">
            <span class="dot tool"></span>Tools {fmtDuration(snap.breakdown.tool_ms)} ({share(snap.breakdown.tool_ms, wall)})
            <span class="dot model"></span>Model {fmtDuration(snap.breakdown.model_ms)} ({share(snap.breakdown.model_ms, wall)})
            <span class="dot human"></span>Waiting on you {fmtDuration(snap.breakdown.human_ms)} ({share(snap.breakdown.human_ms, wall)})
          </p>
          <p class="ins-hint">Tool time adds up each call, so parallel calls can exceed wall time. Model time is the gap before the agent's next step.</p>
        {:else}
          <p class="ins-hint">Not enough timestamps to split time.</p>
        {/if}

        {#if snap.timeline}
          {@const tl = snap.timeline}
          <h2>Tool activity over time</h2>
          <svg class="chart" viewBox={`0 0 ${tl.buckets.length * 10} 60`} preserveAspectRatio="none" role="img" aria-label="Tool calls per time slot">
            {#each tl.buckets as b, i (i)}
              {#if b.calls > 0}
                <rect x={i * 10 + 1} y={58 - (b.calls / peak) * 56} width="8" height={(b.calls / peak) * 56} class="bar-ok"><title>{b.calls} calls, {fmtDuration(b.tool_ms)} of tool time{b.errors ? `, ${b.errors} errors` : ''}</title></rect>
                {#if b.errors > 0}<rect x={i * 10 + 1} y={58 - (b.errors / peak) * 56} width="8" height={(b.errors / peak) * 56} class="bar-err"></rect>{/if}
              {/if}
            {/each}
          </svg>
          <p class="legend">Each bar is {fmtDuration(tl.bucket_ms)}; height is the number of calls (red: errors). From {fmtAgo(tl.start_ms, now)} to now, recent calls only.</p>
        {/if}

        <h2>Tools</h2>
        {#if rows.length === 0}
          <p class="ins-hint">No tool calls recorded yet.</p>
        {:else}
          <div class="table-wrap">
            <table>
              <thead>
                <tr>
                  <th><button type="button" onclick={() => sortBy('name')}>Tool{arrow('name')}</button></th>
                  <th><button type="button" onclick={() => sortBy('calls')}>Calls{arrow('calls')}</button></th>
                  <th><button type="button" onclick={() => sortBy('errors')}>Errors{arrow('errors')}</button></th>
                  <th><button type="button" onclick={() => sortBy('last_used_ms')}>Last used{arrow('last_used_ms')}</button></th>
                  <th><button type="button" onclick={() => sortBy('total_ms')}>Total time{arrow('total_ms')}</button></th>
                  <th><button type="button" onclick={() => sortBy('median_ms')}>Median{arrow('median_ms')}</button></th>
                  <th>Share</th>
                </tr>
              </thead>
              <tbody>
                {#each rows as t (t.name)}
                  <tr>
                    <td class="name">{t.name}</td>
                    <td>{t.calls}</td>
                    <td class:bad={t.errors > 0}>{t.errors}{t.unknown_outcome > 0 && t.unknown_outcome === t.completed ? ' (outcome not recorded)' : ''}</td>
                    <td>{fmtAgo(t.last_used_ms, now)}</td>
                    <td>{fmtDuration(t.total_ms)}</td>
                    <td>{fmtDuration(t.median_ms)}</td>
                    <td>{share(t.total_ms, toolTotal)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
          {#if snap.tools_omitted > 0}<p class="ins-hint">{snap.tools_omitted} more tools not shown.</p>{/if}
        {/if}

        <h2>Recent calls</h2>
        {#if snap.recent.length === 0}
          <p class="ins-hint">None yet.</p>
        {:else}
          <ol class="recent">
            {#each snap.recent.slice(0, 20) as c, i (i)}
              <li><span class="r-tool">{c.tool}</span><span>{fmtAgo(c.started_ms, now)}</span><span>{fmtDuration(c.duration_ms)}</span><span class:bad={c.ok === false}>{c.ok === null ? 'outcome unknown' : c.ok ? 'ok' : 'error'}</span></li>
            {/each}
          </ol>
        {/if}
      {/if}
    </div>
  </div>
</section>

{#if menu}<ContextMenu x={menu.x} y={menu.y} label={`Actions for ${displayName(menu.source)}`} items={menuItems(menu.source)} onClose={() => { menu = null; }} />{/if}

<style>
  .inspector { position: absolute; inset: 0; z-index: 11; display: flex; flex-direction: column; padding: 18px 22px 12px; background: var(--view-bg, var(--bg)); color: var(--text); overflow: hidden; }
  .ins-head { display: flex; align-items: flex-end; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
  .ins-controls { display: flex; gap: 14px; flex-wrap: wrap; }
  .ins-modes { display: flex; border: 1px solid var(--line); }
  .ins-modes button { border: 0; background: transparent; color: var(--muted); padding: .35em .9em; font-size: 12px; }
  .ins-modes button.on { color: var(--text); background: var(--control); box-shadow: inset 0 -2px var(--accent); }
  .ins-map { flex: none; min-height: 4rem; overflow: auto; padding: .2em .4em .4em 0; }
  /* The bar under the map: a thin line with a taller grab area; the details below lose their own top line in map mode. */
  .ins-split { flex: none; height: 7px; margin: -4px 0 -3px; cursor: row-resize; touch-action: none; background: linear-gradient(var(--line), var(--line)) center / 100% 1px no-repeat; }
  .ins-split:hover, .ins-split:focus-visible { background: linear-gradient(var(--accent), var(--accent)) center / 100% 3px no-repeat; outline: none; }
  .ins-split + .ins-detail { border-top: 0; }
  .ins-filter { display: flex; align-items: center; gap: 8px; margin: 0; }
  .ins-filter select { width: auto; margin: 0; }
  .ins-hint { margin: 6px 0 8px; color: var(--muted); font-size: 11px; line-height: 1.5; }
  .ins-note { padding: 7px 10px; margin-bottom: 8px; border: 1px solid var(--line); background: var(--surface); font-size: 12px; }
  .ins-note.error { border-inline-start: 3px solid var(--danger); }
  .ins-body { flex: 1; min-height: 0; display: flex; flex-direction: column; gap: 8px; }

  .ins-tabs { display: flex; flex-wrap: wrap; gap: 2px; border-bottom: 1px solid var(--line); flex: none; }
  .ins-tabs button { border: 0; border-bottom: 2px solid transparent; background: transparent; color: var(--muted); padding: .5em .9em; font-size: 12px; }
  .ins-tabs button.on { color: var(--text); border-bottom-color: var(--accent); }
  .ins-tabs button.zero:not(.on) { opacity: .55; }
  .count { margin-inline-start: .5em; padding: .05em .45em; border: 1px solid var(--line); font: 10px ui-monospace, monospace; }

  /* The tile area is as tall as its tiles up to a share of the height, then scrolls on its own; the details below take the rest. */
  .ins-tiles { flex: 0 1 auto; max-height: 42%; min-height: 4rem; overflow: auto; padding-inline-end: .4em; }
  .tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(14rem, 1fr)); gap: .5em; }
  h2.sec { margin: .9em 0 .4em; font: 10px ui-monospace, monospace; letter-spacing: 1px; color: var(--muted); font-weight: 400; text-transform: uppercase; }
  h2.sec:first-child { margin-top: 0; }
  h2.sec small { margin-inline-start: .4em; }

  .ins-detail { flex: 1 1 0; min-height: 0; min-width: 0; overflow: auto; padding-inline-end: .4em; border-top: 1px solid var(--line); padding-top: .6em; }
  .ins-empty { color: var(--muted); margin: 1.5em 0; text-align: center; font-size: 12px; }
  .sel { display: flex; flex-wrap: wrap; gap: .6em 1.2em; justify-content: space-between; padding: .6em .8em; margin-bottom: .6em; border: 1px solid var(--line); border-inline-start: .35em solid var(--stripe); background: var(--surface); }
  .sel-main { flex: 1 1 24em; min-width: 0; }
  .sel-title { display: flex; flex-wrap: wrap; align-items: center; gap: .5em; }
  .sel-title strong { font-size: 14px; }
  .sel-title small { color: var(--muted); }
  .sel-line { display: flex; gap: .8em; align-items: baseline; margin: .5em 0 0; }
  .sel-line .k { flex: none; color: var(--muted); font: 9px ui-monospace, monospace; letter-spacing: 1px; }
  .sel-line .text { font: 12px ui-monospace, monospace; overflow-wrap: anywhere; }
  .sel-actions { display: flex; flex-wrap: wrap; gap: .5em; align-items: center; align-content: flex-start; }
  .sel-actions > button { font-size: 12px; }
  .tags { display: inline-flex; gap: .3em; margin-inline-start: .4em; }
  .swatch { width: 1.3em; height: 1.3em; padding: 0; background: var(--swatch); border: 1px solid transparent; }
  .swatch[aria-pressed='true'] { outline: 2px solid var(--text); outline-offset: 1px; }
  .chip { font: 9px ui-monospace, monospace; letter-spacing: .4px; padding: .15em .45em; border: 1px solid currentColor; white-space: nowrap; }
  .chip.running { color: var(--accent); }
  .dot { display: inline-block; width: .7em; height: .7em; background: var(--muted); }
  .dot.active { background: var(--accent); box-shadow: 0 0 .6em var(--accent); }
  .dot.idle { background: var(--warn); }
  .dot.inactive { background: var(--dim); }
  .dot.other { background: transparent; border: 1px solid var(--muted); }

  .cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 8px; }
  .card { border: 1px solid var(--line); background: var(--surface); padding: 8px 10px; }
  .card .k { display: block; color: var(--muted); font: 9px ui-monospace, monospace; letter-spacing: 1px; text-transform: uppercase; }
  .card strong { display: block; font-size: 16px; margin: 4px 0; }
  .card small { color: var(--muted); font-size: 10px; }
  .state.working { color: var(--info); }
  .state.waiting_for_human { color: var(--warn); }
  .state.idle { color: var(--muted); }
  h2 { font-size: 13px; letter-spacing: .5px; margin: 16px 0 6px; }
  .bar { display: flex; height: 14px; border: 1px solid var(--line); background: var(--surface); }
  .seg.tool, .dot.tool { background: var(--info); }
  .seg.model, .dot.model { background: var(--accent); }
  .seg.human, .dot.human { background: var(--warn); }
  .legend { font-size: 11px; display: flex; flex-wrap: wrap; align-items: center; gap: 4px 12px; margin: 6px 0 0; }
  .legend .dot { margin-right: 5px; width: 8px; height: 8px; }
  .chart { width: 100%; height: 70px; border: 1px solid var(--line); background: var(--surface); display: block; }
  .bar-ok { fill: var(--info); }
  .bar-err { fill: var(--danger); }
  .table-wrap { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 12px; }
  th, td { text-align: left; padding: 5px 8px; border-bottom: 1px solid var(--line); white-space: nowrap; }
  th button { border: 0; background: transparent; padding: 0; font: 600 11px/1.4 inherit; color: var(--muted); }
  td.name { font-family: ui-monospace, monospace; white-space: normal; overflow-wrap: anywhere; }
  .bad { color: var(--danger); }
  .recent { list-style: none; margin: 0; padding: 0; font-size: 11px; }
  .recent li { display: grid; grid-template-columns: minmax(0, 1.4fr) 1fr 1fr 1fr; gap: 8px; padding: 3px 0; border-bottom: 1px solid var(--line); }
  .r-tool { font-family: ui-monospace, monospace; overflow-wrap: anywhere; }
</style>
