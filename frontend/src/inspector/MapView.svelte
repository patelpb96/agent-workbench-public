<script lang="ts">
  // The agent map: every listed session and subagent as a small pixel-snapped glyph in one compact block (a session with its
  // subagents beside it), with the tools they use outside the block on two rails, each named and sized by how much it is used.
  // Pointing at an agent lights its routes to its tools; pointing at (or clicking) a tool lights the agents that use it; a call
  // that is running right now is lit amber from the agent to the tool whoever is selected. The layout is the pure `buildMap` in
  // map.ts; this file draws it, follows the pointer and handles the keyboard.
  import { onMount } from 'svelte';
  import { toMapFacts, type MapPayload, type InspectorError, type SourceInfo } from './client.js';
  import { inspectorClient } from './tauri.js';
  import { agoShort, classify, sortSources } from './groups.js';
  import { buildMap, levelFor, type MapFacts, type MapGlyph, type MapNode } from './map.js';

  interface Props {
    sources: readonly SourceInfo[];
    now: number;
    selected: string | null;
    onSelect: (id: string) => void;
  }
  let { sources, now, selected, onSelect }: Props = $props();

  const POLL_MS = 2000;
  let width = $state(0);
  let height = $state(0);
  let hidden = $state(typeof document !== 'undefined' && document.hidden);
  let facts = $state.raw<MapFacts | undefined>(undefined);
  let reports = $state.raw<MapPayload['reports']>([]);
  let note = $state<string | null>(null);
  /** What the pointer (or keyboard focus) is on, and a tool pinned by a click. */
  let hover = $state<{ kind: 'agent' | 'tool'; id: string } | null>(null);
  let pinned = $state<string | null>(null);
  const els: Record<string, SVGGElement | undefined> = {};

  const byId = $derived(new Map(sources.map(s => [s.id, s])));
  const hoverAgent = $derived(hover?.kind === 'agent' ? hover.id : null);
  // The map fills the height it is given (the bar under it sets that): laid out for a box `zoom` times smaller, then drawn
  // `scale` times larger, so taller means bigger glyphs and names, never a small drawing in a large empty area.
  const MAX_ZOOM = 4;
  const ordered = $derived(sortSources(sources, 'newest'));
  const layout = (box: { width: number; height: number }) => buildMap({ sources: ordered, now, facts, box, selected, highlight: hoverAgent, reports, level: levelFor(width) });
  const natural = $derived(layout({ width, height }));
  const zoom = $derived(width > 0 && height > 0 && natural.height > 0 ? Math.min(MAX_ZOOM, Math.max(1, height / natural.height)) : 1);
  const map = $derived(zoom > 1 ? layout({ width: width / zoom, height: height / zoom }) : natural);
  const scale = $derived(width > 0 && height > 0 ? Math.max(1, Math.min(MAX_ZOOM, height / map.height, width / map.width)) : 1);
  const agents = $derived(map.nodes.filter(n => n.source !== null));
  // Exactly one agent glyph is in the tab order: the selected one, else the first.
  const focusId = $derived((agents.find(n => n.source === selected) ?? agents[0])?.id ?? null);

  // What stays bright: with a tool in focus, the tool and the agents that use it; with an agent hovered, it and the tools it uses.
  const focusTool = $derived(hover?.kind === 'tool' ? hover.id : pinned);
  const litAgents = $derived.by((): Set<string> | null => {
    const tool = focusTool ? map.nodes.find(n => n.id === focusTool) : undefined;
    return tool ? new Set(tool.users) : null;
  });
  const litTools = $derived.by((): Set<string> | null => {
    if (focusTool) return new Set([focusTool]);
    if (!hoverAgent) return null;
    return new Set(map.edges.filter(e => e.from === `s:${hoverAgent}` && (e.kind === 'call' || e.kind === 'live')).map(e => e.to));
  });
  const dimmed = (n: MapNode): boolean => {
    if (n.kind === 'tool') return litTools !== null && !litTools.has(n.id) && !n.live;
    return litAgents !== null && n.source !== null && !litAgents.has(n.source) && n.source !== selected;
  };

  async function load(): Promise<void> {
    if (document.hidden) return;
    try {
      const payload = await inspectorClient.map();
      facts = toMapFacts(payload);
      reports = payload.reports;
      note = null;
    } catch (e) {
      // Keep the last good facts on screen.
      note = `Tool details are not available right now (${(e as InspectorError).code}).`;
    }
  }

  onMount(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    const visibility = (): void => {
      hidden = document.hidden;
      if (!hidden) void load();
    };
    document.addEventListener('visibilitychange', visibility);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', visibility);
    };
  });

  const centre = (n: MapNode): [number, number] => [n.x + n.w / 2, n.y + n.h / 2];
  /** The agent glyph nearest in a direction: along that axis first, the cross axis counts double. */
  function neighbour(from: MapNode, dx: number, dy: number): MapNode | null {
    const [fx, fy] = centre(from);
    let best: MapNode | null = null;
    let bestScore = Infinity;
    for (const n of agents) {
      if (n.id === from.id) continue;
      const [x, y] = centre(n);
      const along = (x - fx) * dx + (y - fy) * dy;
      if (along <= 0) continue;
      const across = Math.abs((x - fx) * dy) + Math.abs((y - fy) * dx);
      const score = along + 2 * across;
      if (score < bestScore) { best = n; bestScore = score; }
    }
    return best;
  }
  function go(n: MapNode | null | undefined): void {
    if (n) els[n.id]?.focus();
  }
  function keys(event: KeyboardEvent, node: MapNode): void {
    const arrow = ({ ArrowDown: [0, 1], ArrowUp: [0, -1], ArrowRight: [1, 0], ArrowLeft: [-1, 0] } as Record<string, [number, number]>)[event.key];
    if (arrow) { event.preventDefault(); go(neighbour(node, arrow[0], arrow[1])); }
    else if (event.key === 'Home') { event.preventDefault(); go(agents[0]); }
    else if (event.key === 'End') { event.preventDefault(); go(agents[agents.length - 1]); }
    else if ((event.key === 'Enter' || event.key === ' ') && node.source) { event.preventDefault(); onSelect(node.source); }
  }
  function name(node: MapNode): string {
    const s = node.source ? byId.get(node.source) : undefined;
    return s ? `${node.label}, ${classify(s, now)}, ${agoShort(s.modified_ms, now)}` : node.label;
  }
  const point = (node: MapNode): void => { hover = { kind: node.kind === 'tool' ? 'tool' : 'agent', id: node.source ?? node.id }; };
  const unpoint = (): void => { hover = null; };
  const togglePin = (node: MapNode): void => { pinned = pinned === node.id ? null : node.id; };

  const dash = (g: MapGlyph): string | undefined => (g.fill === 'dashed' ? '3 2' : undefined);
  const paint = (g: MapGlyph): { fill: string; opacity: number } => ({ fill: g.fill === 'solid' ? g.hue : 'none', opacity: g.fill === 'empty' ? 0.45 : 1 });
</script>

{#snippet glyph(n: MapNode)}
  {@const g = n.glyph}
  {@const p = paint(g)}
  <title>{n.source !== null ? `${name(n)}${n.detail ? ` - ${n.detail}` : ''}` : `${n.label}: ${n.detail}`}</title>
  {#if g.selected}<rect class="sel" x="-3" y="-3" width={n.w + 6} height={n.h + 6} fill="none" />{/if}
  {#if n.live}<rect class="live-ring" x="-3" y="-3" width={n.w + 6} height={n.h + 6} fill="none" />{/if}
  {#if n.kind === 'tool' && n.side}
    <!-- the pointer target covers the glyph and its name -->
    <rect class="hit" x={n.side === 'left' ? -n.reach : -3} y="-3" width={n.reach + n.w + 3} height={n.h + 6} fill="transparent" stroke="none" />
  {:else}
    <rect class="hit" x="-3" y="-3" width={n.w + 6} height={n.h + 6} fill="transparent" stroke="none" />
  {/if}
  {#if g.shape === 'diamond'}
    <polygon points={`${n.w / 2},0 ${n.w},${n.h / 2} ${n.w / 2},${n.h} 0,${n.h / 2}`} fill={p.fill} stroke={g.hue} stroke-width="1.5" stroke-dasharray={dash(g)} opacity={p.opacity} />
  {:else if g.shape === 'sheet'}
    <polygon points={`0,0 ${n.w - 4},0 ${n.w},4 ${n.w},${n.h} 0,${n.h}`} fill={p.fill} stroke={g.hue} stroke-width="1" stroke-dasharray={dash(g)} opacity={p.opacity} />
  {:else if g.shape === 'chevron'}
    <polyline points={`2,1 ${n.w - 3},${n.h / 2} 2,${n.h - 1}`} fill="none" stroke={g.hue} stroke-width="2" stroke-dasharray={dash(g)} opacity={p.opacity} />
  {:else if g.shape === 'globe'}
    <polygon points={`3,0 ${n.w - 3},0 ${n.w},3 ${n.w},${n.h - 3} ${n.w - 3},${n.h} 3,${n.h} 0,${n.h - 3} 0,3`} fill={p.fill} stroke={g.hue} stroke-width="1" stroke-dasharray={dash(g)} opacity={p.opacity} />
  {:else if g.shape === 'stack'}
    {#each [0, 5, 10] as y (y)}<rect x="0" {y} width={n.w} height="2" fill={g.hue} opacity={g.fill === 'dashed' ? 0.55 : 1} />{/each}
  {:else}
    <rect x="0.5" y="0.5" width={n.w - 1} height={n.h - 1} fill={p.fill} stroke={g.hue} stroke-width="1.5" stroke-dasharray={dash(g)} opacity={p.opacity} />
  {/if}
  {#if g.inFlight && n.kind !== 'tool'}<rect class="flight" x={n.w - 3} y="-2" width="5" height="5" />{/if}
  {#if g.errors && n.kind !== 'tool'}<rect class="fault" x={n.w - 3} y={n.h - 3} width="5" height="5" />{/if}
  {#if n.kind === 'tool' && n.side}
    <!-- the name beside the glyph, outward from the block, with a bar under it for how much the tool is used -->
    <text class="tool-name" class:live={n.live} x={n.side === 'left' ? -6 : n.w + 6} y="5" text-anchor={n.side === 'left' ? 'end' : 'start'} dominant-baseline="central">{n.text}</text>
    <rect class="tool-bar" class:err={g.errors} x={n.side === 'left' ? -6 - n.bar : n.w + 6} y="11" width={n.bar} height="2" />
  {/if}
{/snippet}

<div class="map-wrap" bind:clientWidth={width} bind:clientHeight={height}>
  {#if !hidden}
    <svg viewBox={`0 0 ${map.width} ${map.height}`} width={Math.floor(map.width * scale)} height={Math.floor(map.height * scale)} shape-rendering="crispEdges" role="group" aria-label={map.summary} preserveAspectRatio="xMinYMin meet">
      {#if map.frame}<rect class="frame" x={map.frame.x} y={map.frame.y} width={map.frame.w} height={map.frame.h} fill="none" />{/if}
      {#each map.edges as e (`${e.kind}:${e.from}>${e.to}`)}
        <polyline class={`edge ${e.kind}`} points={e.points.map(p => p.join(',')).join(' ')} fill="none" />
      {/each}
      {#each map.nodes as n (n.id)}
        {#if n.source !== null}
          {@const source = n.source}
          <g class="node agent" class:dim={dimmed(n)} transform={`translate(${n.x} ${n.y})`} data-node={n.id} role="button" tabindex={n.id === focusId ? 0 : -1} aria-label={name(n)} aria-pressed={n.glyph.selected}
            bind:this={els[n.id]} onclick={() => onSelect(source)} onkeydown={event => keys(event, n)}
            onpointerenter={() => point(n)} onpointerleave={unpoint} onfocus={() => point(n)} onblur={unpoint}>
            {@render glyph(n)}
          </g>
        {:else}
          <g class="node tool" class:dim={dimmed(n)} class:pinned={pinned === n.id} transform={`translate(${n.x} ${n.y})`} data-node={n.id} role="button" tabindex="-1"
            aria-label={`${n.label}: ${n.detail}`} aria-pressed={pinned === n.id} onclick={() => togglePin(n)} onkeydown={event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); togglePin(n); } }}
            onpointerenter={() => point(n)} onpointerleave={unpoint}>
            {@render glyph(n)}
          </g>
        {/if}
      {/each}
    </svg>
  {/if}
  {#if note}<p class="map-note" role="status">{note}</p>{:else if !facts}<p class="map-note" role="status">Reading what each agent has used…</p>{/if}
</div>

<style>
  .map-wrap { position: relative; width: 100%; height: 100%; }
  svg { display: block; }
  .edge { stroke: var(--line); stroke-width: 1; }
  .edge.spawn { stroke: var(--muted); }
  .edge.reports { stroke: var(--muted); stroke-dasharray: 3 2; }
  .edge.rail { stroke: var(--line); }
  .edge.call { stroke: #8abaff; stroke-width: 1.5; }
  .edge.live { stroke: #e5bc6e; stroke-width: 2; }
  .frame { stroke: var(--line); stroke-width: 1; stroke-dasharray: 2 3; }
  .node { transition: none; }
  .node.agent, .node.tool { cursor: pointer; }
  .node.agent:focus, .node.tool:focus { outline: none; }
  .node.agent:focus-visible .hit, .node.tool:focus-visible .hit { stroke: #8abaff; stroke-width: 1; stroke-dasharray: 2 2; }
  .node.dim { opacity: .28; }
  .node.agent:hover .hit, .node.tool:hover .hit { stroke: var(--muted); stroke-width: 1; }
  .node.tool.pinned .hit { stroke: #8abaff; stroke-width: 1; }
  .sel { stroke: #8abaff; stroke-width: 2; }
  .live-ring { stroke: #e5bc6e; stroke-width: 2; }
  .flight { fill: #e5bc6e; }
  .fault { fill: #e29787; }
  .tool-name { fill: var(--muted); font: 10px ui-monospace, monospace; pointer-events: none; }
  .tool-name.live { fill: #e5bc6e; }
  .tool-bar { fill: #4a5b73; pointer-events: none; }
  .tool-bar.err { fill: #b8665a; }
  .map-note { margin: 4px 0 0; color: var(--muted); font-size: 11px; }
</style>
