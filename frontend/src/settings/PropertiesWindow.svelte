<script lang="ts">
  // The Properties window: configuration options split into tabs. Each option that exists in the app lives here
  // (and only there); candidate options are specified in docs/design/properties-window.md and are not added
  // without approval. Opened from the title bar; Esc or Close returns to where you were.
  import { onMount, type Snippet } from 'svelte';
  import { settingsStore } from './settings-store.svelte.js';
  import { launchFolderProblem, startFolderProblem } from './settings.js';
  import FolderField from './FolderField.svelte';
  import DataTab from './DataTab.svelte';
  import { terminalClient } from '../terminals/tauri.js';
  import { isPrefixSetting, PREFIX_LABEL, PREFIXES } from '../terminals/keymap.js';

  interface Props {
    onClose: () => void;
    /** The existing appearance controls (theme and background), rendered by the app that owns their state. */
    appearance: Snippet;
  }
  let { onClose, appearance }: Props = $props();

  const tabs = [
    { id: 'terminals', label: 'Terminals' },
    { id: 'agents', label: 'Agents' },
    { id: 'appearance', label: 'Appearance' },
    { id: 'data', label: 'Data' }
  ] as const;
  type TabId = (typeof tabs)[number]['id'];
  let selected = $state<TabId>('terminals');
  let dialog = $state<HTMLDivElement | undefined>();
  let opener: Element | null = null;

  // Service start at sign-in is a native (registry) setting, not part of settings.json.
  let autostart = $state(false);
  let autostartBusy = $state(false);
  let autostartError = $state<string | null>(null);
  async function toggleAutostart(): Promise<void> {
    autostartBusy = true;
    autostartError = null;
    try { autostart = await terminalClient.autostartSet(!autostart); }
    catch (e) { autostartError = (e as { message?: string }).message ?? 'It could not be changed.'; }
    finally { autostartBusy = false; }
  }

  function selectTab(id: TabId, focus = false): void {
    selected = id;
    if (focus) queueMicrotask(() => dialog?.querySelector<HTMLButtonElement>(`#tab-${id}`)?.focus());
  }
  function tabKeys(event: KeyboardEvent): void {
    const keys: Record<string, number> = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 };
    if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault();
      selectTab(tabs[event.key === 'Home' ? 0 : tabs.length - 1].id, true);
      return;
    }
    const step = keys[event.key];
    if (step === undefined) return;
    event.preventDefault();
    const at = tabs.findIndex(t => t.id === selected);
    selectTab(tabs[(at + step + tabs.length) % tabs.length].id, true);
  }
  function windowKeys(event: KeyboardEvent): void {
    if (event.key === 'Escape') { event.preventDefault(); onClose(); }
    // Keep Tab inside the dialog.
    if (event.key === 'Tab' && dialog) {
      const items = [...dialog.querySelectorAll<HTMLElement>('button, input, select, textarea, [tabindex]:not([tabindex="-1"])')].filter(el => !el.hasAttribute('disabled') && el.offsetParent !== null);
      if (items.length === 0) return;
      const first = items[0], last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    }
  }

  onMount(() => {
    opener = document.activeElement;
    void terminalClient.autostartGet().then(v => { autostart = v; }).catch(() => undefined);
    dialog?.querySelector<HTMLButtonElement>(`#tab-${selected}`)?.focus();
    return () => { if (opener instanceof HTMLElement) opener.focus(); };
  });
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="backdrop" onkeydown={windowKeys} onmousedown={event => { if (event.target === event.currentTarget) onClose(); }}>
  <div class="dialog" role="dialog" aria-modal="true" aria-label="Properties" bind:this={dialog}>
    <header>
      <h2>Properties</h2>
      <button type="button" class="close" onclick={onClose} aria-label="Close properties">×</button>
    </header>
    <div class="body">
      <div class="tabs" role="tablist" aria-label="Property groups" aria-orientation="vertical">
        {#each tabs as tab (tab.id)}
          <button type="button" role="tab" id={`tab-${tab.id}`} aria-selected={selected === tab.id} aria-controls={`panel-${tab.id}`} tabindex={selected === tab.id ? 0 : -1}
            class:on={selected === tab.id} onclick={() => selectTab(tab.id)} onkeydown={tabKeys}>{tab.label}</button>
        {/each}
      </div>
      <div class="panel" role="tabpanel" id={`panel-${selected}`} aria-labelledby={`tab-${selected}`}>
        {#if selected === 'terminals'}
          <h3>Terminals</h3>
          <FolderField id="start-folder" label="Start new terminals in" value={settingsStore.value.terminals.start_folder}
            placeholder="Empty: your user folder (C:\Users\you)" clearLabel="Use my user folder"
            check={startFolderProblem} apply={text => s => ({ ...s, terminals: { ...s.terminals, start_folder: text } })} />
          <p class="hint">Applies to terminals you create in Workbench from now on; terminals already running keep their folder. If the folder is later deleted, new terminals fall back to your user folder.</p>
          <label class="check"><input type="checkbox" checked={settingsStore.value.terminals.extended_history} disabled={settingsStore.saving}
            onchange={event => void settingsStore.update(s => ({ ...s, terminals: { ...s.terminals, extended_history: event.currentTarget.checked } }))} />
            Keep extended history by default (32 MiB per terminal)</label>
          <p class="hint">Extended history keeps far more output in the terminal's own memory (about 300,000 lines instead of 1 MiB). Memory only, never written to disk. You can still change it per terminal when you create one.</p>
          <label class="prefix">Prefix key (tmux-style commands in terminals)
            <select value={settingsStore.value.keys.prefix} disabled={settingsStore.saving}
              onchange={event => { const prefix = event.currentTarget.value; if (isPrefixSetting(prefix)) void settingsStore.update(s => ({ ...s, keys: { prefix } })); }}>
              {#each PREFIXES as p (p)}<option value={p}>{p === 'off' ? 'Off: no key is taken from the terminals' : `${PREFIX_LABEL[p]}${p === 'ctrl+b' ? ' (tmux)' : ' (screen)'}`}</option>{/each}
            </select></label>
          <p class="hint">In a terminal, press the prefix and then a key: % or " splits, arrows move between panes, z zooms, [ copies, / searches, S types into every pane, L logs to a file, ? lists them all. Only the prefix is taken from the terminal; pressing it twice sends it on (for programs that use it themselves).</p>
          <label class="check"><input type="checkbox" checked={autostart} disabled={autostartBusy} onchange={() => void toggleAutostart()} /> Start the terminal service when I sign in</label>
          <p class="hint">Off by default. The service starts on demand either way; this only starts it earlier. It runs as you, never elevated.{autostartError ? ` ${autostartError}` : ''}</p>
        {:else if selected === 'agents'}
          <h3>Agents</h3>
          <FolderField id="launch-folder" label="Default folder for Launch agent" value={settingsStore.value.agents.launch_folder}
            placeholder="C:\Users\you\project" check={launchFolderProblem} apply={text => s => ({ ...s, agents: { ...s.agents, launch_folder: text } })} />
          <p class="hint">Fills the folder box in the Terminals tab's Launch agent row. It is also updated to the last folder you launched an agent in.</p>
        {:else if selected === 'data'}
          <DataTab />
        {:else}
          <h3>Appearance</h3>
          {@render appearance()}
        {/if}
      </div>
    </div>
    <footer role="status">
      {#if settingsStore.problem}<span class="bad">{settingsStore.problem.message}</span>{:else if settingsStore.saving}<span>Saving…</span>{:else}<span>Changes are saved as you make them.</span>{/if}
    </footer>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; z-index: 60; background: #000b; display: flex; align-items: center; justify-content: center; padding: 24px; }
  .dialog { width: min(820px, 100%); height: min(560px, 100%); display: flex; flex-direction: column; background: var(--bg, #101720); border: 1px solid var(--line, #344256); color: var(--text, #e4ebf5); box-shadow: 0 20px 60px #000c; }
  header { display: flex; align-items: center; justify-content: space-between; padding: 8px 14px; border-bottom: 1px solid var(--line, #344256); }
  h2 { margin: 0; font-size: 15px; letter-spacing: .3px; }
  .close { border: 0; background: transparent; font-size: 20px; line-height: 1; padding: 2px 8px; }
  .close:hover { background: #b42318; color: #fff; }
  .body { flex: 1; min-height: 0; display: flex; }
  .tabs { flex: 0 0 160px; display: flex; flex-direction: column; border-right: 1px solid var(--line, #344256); padding: 10px 0; background: var(--surface, #151e29); }
  .tabs button { text-align: left; border: 0; background: transparent; padding: 10px 16px; font-size: 13px; color: var(--muted, #a5b3c6); }
  .tabs button.on { background: var(--control, #1b2735); color: var(--text, #e4ebf5); box-shadow: inset 2px 0 #86b6ff; }
  .panel { flex: 1; min-width: 0; overflow: auto; padding: 14px 20px 20px; }
  h3 { margin: 0 0 14px; font-size: 14px; font-weight: 500; }
  .hint { margin: 2px 0 16px; color: var(--muted, #a5b3c6); font-size: 11px; line-height: 1.6; }
  .check { display: flex; align-items: center; gap: 8px; margin: 10px 0 2px; font-size: 12px; }
  .prefix { display: block; margin: 12px 0 2px; font-size: 12px; }
  .prefix select { display: block; width: auto; margin-top: 4px; }
  .check input[type='checkbox'] { width: 16px; height: 16px; padding: 0; margin: 0; flex: none; display: inline-block; }
  footer { padding: 6px 14px; border-top: 1px solid var(--line, #344256); font-size: 11px; color: var(--muted, #a5b3c6); min-height: 28px; }
  .bad { color: #f3cfc7; }
</style>
