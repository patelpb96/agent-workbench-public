<script lang="ts">
  // The Model tab on an agent terminal's bar: pick a model and the terminal's input is FILLED with `/model <name>`.
  // Nothing is sent; the person presses Enter. Favourites (star) are listed first and are kept per agent kind.
  import { untrack } from 'svelte';
  import { agentChoices, type AgentName } from './agents.js';
  import { modelCommand, parseFavourites, toggleFavourite, visibleModels, type Favourites } from './models.js';
  import { terminalClient } from './tauri.js';

  interface Props { terminal: string; agent: AgentName; onNotice: (text: string) => void }
  let { terminal, agent, onNotice }: Props = $props();

  const KEY = 'aw.models';
  // The terminal was launched as `agent`, but a person can quit it and start a different CLI in the same terminal (Codex, Pi, any
  // other). Which list to offer is therefore their choice, remembered per terminal; it starts as the agent that was launched.
  const CHOICE_KEY = 'aw.model-agent';
  let open = $state(false);
  let chosen = $state<AgentName | null>(untrack(() => loadChoice()));
  let search = $state('');
  let favourites = $state.raw<Favourites>(untrack(() => load()));
  let field = $state<HTMLInputElement | undefined>();

  function load(): Favourites {
    try { return parseFavourites(localStorage.getItem(KEY)); } catch { return {}; }
  }
  function save(next: Favourites): void {
    favourites = next;
    try { localStorage.setItem(KEY, JSON.stringify(next)); } catch { /* private window: favourites last for this run only */ }
  }

  function choices(): Record<string, AgentName> {
    try {
      const raw: unknown = JSON.parse(localStorage.getItem(CHOICE_KEY) ?? '{}');
      if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return {};
      return Object.fromEntries(Object.entries(raw).filter(([, v]) => agentChoices.some(a => a.id === v))) as Record<string, AgentName>;
    } catch { return {}; }
  }
  function loadChoice(): AgentName | null { return choices()[terminal] ?? null; }
  function choose(next: AgentName): void {
    chosen = next;
    try {
      const all = { ...choices(), [terminal]: next };
      // Terminals come and go; keep only the most recent few dozen choices.
      localStorage.setItem(CHOICE_KEY, JSON.stringify(Object.fromEntries(Object.entries(all).slice(-64))));
    } catch { /* private window: the choice lasts for this run only */ }
  }

  const listFor = $derived(chosen ?? agent);
  const shown = $derived(visibleModels(listFor, search, favourites[listFor] ?? []));

  function toggle(): void {
    open = !open;
    if (open) { search = ''; favourites = load(); queueMicrotask(() => field?.focus()); }
  }
  async function pick(name: string): Promise<void> {
    const line = modelCommand(name);
    if (line === null) return;
    open = false;
    try {
      await terminalClient.input(terminal, line);
      onNotice(`Typed ${line} into the terminal. Press Enter to switch.`);
    } catch {
      onNotice('The model command could not be typed into the terminal.');
    }
  }
  function key(e: KeyboardEvent): void {
    if (e.key === 'Escape') { open = false; e.stopPropagation(); }
    else if (e.key === 'Enter' && shown[0]) { e.preventDefault(); void pick(shown[0].name); }
  }
</script>

<span class="picker">
  <button type="button" class:on={open} aria-expanded={open} aria-haspopup="listbox" title="Fill the input with a model switch command (nothing is sent)" onclick={toggle}>Model</button>
  {#if open}
    <div class="pop" role="dialog" aria-label="Choose a model">
      <div class="which" role="group" aria-label="Models for">
        <span>List for</span>
        {#each agentChoices as a (a.id)}
          <button type="button" class:on={listFor === a.id} aria-pressed={listFor === a.id} onclick={() => choose(a.id)}>{a.label}</button>
        {/each}
      </div>
      <input bind:this={field} bind:value={search} type="search" placeholder="Search, or type a model name" aria-label="Search models" onkeydown={key} />
      <ul role="listbox" aria-label="Models">
        {#each shown as m (m.name)}
          {@const starred = (favourites[listFor] ?? []).includes(m.name)}
          <li class="row">
            <button type="button" class="star" class:on={starred} aria-pressed={starred} aria-label={`${starred ? 'Unstar' : 'Star'} ${m.name || 'model list'}`} onclick={() => save(toggleFavourite(favourites, listFor, m.name))}>{starred ? '★' : '☆'}</button>
            <button type="button" class="name" role="option" aria-selected="false" onclick={() => void pick(m.name)}>
              <strong>{m.name || '/model'}</strong><small>{m.note}</small>
            </button>
          </li>
        {:else}
          <li class="none">No match. A valid model name you type here is offered as a choice.</li>
        {/each}
      </ul>
      <p class="hint">Picking fills the input with <code>/model name</code>. Press Enter in the terminal to send it.</p>
    </div>
  {/if}
</span>

<style>
  .picker { position: relative; display: inline-flex; }
  .picker > button { font-size: 10px; padding: 1px 8px; }
  .picker > button.on { border-color: #8abaff; box-shadow: inset 0 -2px #8abaff; }
  .pop { position: absolute; top: 100%; left: 0; z-index: 40; width: 260px; margin-top: 2px; padding: 6px; background: #0b121a; border: 1px solid #60738d; box-shadow: 0 12px 32px #0008; }
  .which { display: flex; flex-wrap: wrap; align-items: center; gap: 3px; margin-bottom: 6px; font-size: 10px; color: #a5b3c6; }
  .which button { font-size: 10px; padding: 1px 6px; }
  .which button.on { border-color: #8abaff; box-shadow: inset 0 -2px #8abaff; }
  .pop input { width: 100%; box-sizing: border-box; font-size: 12px; }
  ul { list-style: none; margin: 6px 0; padding: 0; max-height: 220px; overflow: auto; }
  .row { display: flex; align-items: stretch; }
  .star { border: 0; background: transparent; color: #a5b3c6; padding: 0 6px; font-size: 14px; }
  .star.on { color: #e5bc6e; }
  .name { flex: 1; min-width: 0; display: flex; flex-direction: column; align-items: flex-start; text-align: left; border: 0; background: transparent; padding: 3px 4px; color: #e4ebf5; }
  .name:hover { background: #151e29; }
  .name strong { font: 12px ui-monospace, monospace; font-weight: 600; }
  .name small { font-size: 10px; color: #a5b3c6; }
  .none, .hint { margin: 0; font-size: 10px; color: #a5b3c6; }
  code { font-size: 10px; }
</style>
