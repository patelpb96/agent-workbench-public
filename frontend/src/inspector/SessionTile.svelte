<script lang="ts">
  // One session as a small tile: a status dot, the folder it works in, how long ago it moved, who and which
  // model it is, and the last line it wrote. Selecting a tile shows the rest below; right-click opens its menu.
  import type { SourceInfo } from './client.js';
  import { AGENT_COLOR, TAG_COLOR, agentName, agoShort, classify, displayName, modelColor, shortModel, type TagName } from './groups.js';

  interface Props {
    source: SourceInfo;
    now: number;
    selected: boolean;
    tag: TagName | null;
    /** The parent session's name, for a subagent tile. */
    parentName?: string | null;
    onSelect: () => void;
    onMenu: (event: MouseEvent) => void;
  }
  let { source, now, selected, tag, parentName = null, onSelect, onMenu }: Props = $props();

  const group = $derived(classify(source, now));
  const stripe = $derived(tag ? TAG_COLOR[tag] : AGENT_COLOR[source.agent]);
  const live = $derived(source.running === null ? null : source.running === 'background' ? 'background' : 'open');
</script>

<button type="button" class="tile" class:selected aria-pressed={selected} style={`--stripe:${stripe}`} onclick={onSelect} oncontextmenu={onMenu}
  title={source.last_line ?? 'No output yet'}>
  <span class="top">
    <span class={`dot ${group}`} role="img" aria-label={group}></span>
    <strong>{displayName(source, parentName)}</strong>
    <time>{agoShort(source.modified_ms, now)}</time>
  </span>
  <span class="chips">
    <span class="chip" style={`color:${AGENT_COLOR[source.agent]}`}>{agentName(source.agent)}</span>
    {#if source.model}<span class="chip" style={`color:${modelColor(source.model)}`}>{shortModel(source.model)}</span>{/if}
    {#if live}<span class="chip running">{live}</span>{/if}
    {#if tag}<span class="chip tagged" style={`color:${TAG_COLOR[tag]}`}>{tag}</span>{/if}
  </span>
  <span class="line" class:none={!source.last_line}>{source.last_line ?? 'no output yet'}</span>
</button>

<style>
  .tile { display: flex; flex-direction: column; gap: .35em; width: 100%; min-width: 0; text-align: left; padding: .5em .6em .5em .75em; border: 1px solid var(--line); border-inline-start: .3em solid var(--stripe); background: var(--surface); color: var(--text); }
  .tile:hover { background: var(--control); }
  .tile.selected { border-color: #8abaff; border-inline-start-color: var(--stripe); background: var(--control); box-shadow: 0 0 0 1px #8abaff; }
  .top { display: flex; align-items: center; gap: .5em; min-width: 0; }
  .top strong { flex: 1; min-width: 0; font-size: 12px; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  time { color: var(--muted); font: 10px ui-monospace, monospace; flex: none; }
  .dot { flex: none; width: .65em; height: .65em; background: var(--muted); }
  .dot.active { background: #6fc48b; box-shadow: 0 0 .6em #6fc48b; }
  .dot.idle { background: #e5bc6e; }
  .dot.inactive { background: #6b7a8f; }
  .dot.other { background: transparent; border: 1px solid var(--muted); }
  .chips { display: flex; flex-wrap: wrap; gap: .3em; min-width: 0; }
  .chip { font: 9px ui-monospace, monospace; letter-spacing: .4px; padding: .15em .45em; border: 1px solid currentColor; white-space: nowrap; max-width: 100%; overflow: hidden; text-overflow: ellipsis; }
  .chip.running { color: #6fc48b; }
  .line { font: 10.5px ui-monospace, monospace; color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .line.none { opacity: .6; font-style: italic; }
</style>
