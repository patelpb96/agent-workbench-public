<script lang="ts">
  // What the prefix key shows: "waiting for a command" after the prefix, a short message after a command, and the
  // list of bindings (prefix then ?). Square, out of the way at the bottom of the window, never catches clicks except
  // on the list itself.
  import { settingsStore } from '../settings/settings-store.svelte.js';
  import { BINDINGS, PREFIX_LABEL } from './keymap.js';
  import { keyControl } from './keys.svelte.js';

  const prefix = $derived(PREFIX_LABEL[settingsStore.value.keys.prefix]);
  let closeButton = $state<HTMLButtonElement | undefined>();
  $effect(() => { if (keyControl.helpOpen) closeButton?.focus(); });
</script>

{#if keyControl.waiting}
  <div class="key-hint" role="status"><strong>{prefix}</strong> · waiting for a command key · ? lists them · Esc cancels</div>
{:else if keyControl.message}
  <div class="key-hint" role="status">{keyControl.message}</div>
{/if}

{#if keyControl.helpOpen}
  <div class="key-help" role="dialog" aria-modal="false" aria-label="Terminal keyboard commands">
    <header>
      <span>Press <strong>{prefix}</strong> in a terminal, then:</span>
      <button type="button" bind:this={closeButton} aria-label="Close the list" onclick={() => keyControl.closeHelp()}>×</button>
    </header>
    <dl>
      {#each BINDINGS as b (b.keys)}<dt>{b.keys}</dt><dd>{b.what}</dd>{/each}
    </dl>
    <p>Pane commands work in the Terminals view. On the Canvas, the arrows, o and 1 to 9 move between windows by where they are on screen, and Ctrl or Alt with an arrow resizes the window (as tmux does for floating panes). Change the prefix, or turn it off, in Properties &gt; Terminals.</p>
  </div>
{/if}

<style>
  .key-hint { position: fixed; left: 50%; bottom: 34px; transform: translateX(-50%); z-index: 70; max-width: min(720px, calc(100% - 32px)); padding: 5px 10px; font: 11px ui-monospace, monospace; color: var(--text, #e4ebf5); background: #101720f2; border: 1px solid #e5bc6e; pointer-events: none; overflow-wrap: anywhere; }
  .key-hint strong { color: #e5bc6e; }
  .key-help { position: fixed; right: 24px; bottom: 34px; z-index: 70; width: min(440px, calc(100% - 32px)); max-height: 70vh; overflow: auto; padding: 8px 12px; font-size: 12px; color: var(--text, #e4ebf5); background: var(--bg, #101720); border: 1px solid #8abaff; box-shadow: 0 16px 40px #000a; }
  .key-help header { display: flex; align-items: center; justify-content: space-between; gap: 8px; margin-bottom: 6px; }
  .key-help header strong { color: #e5bc6e; }
  .key-help header button { padding: 0 6px; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 3px 12px; margin: 0; }
  dt { font: 11px ui-monospace, monospace; color: #8abaff; }
  dd { margin: 0; }
  p { margin: 8px 0 0; color: var(--muted, #a5b3c6); font-size: 11px; }
</style>
