<script lang="ts">
  // A folder text field for the Properties window: shows what is saved, checks what you type, saves on Enter or
  // when you leave the field, and says why a value was refused. Nothing invalid is ever saved.
  import { settingsStore } from './settings-store.svelte.js';
  import BrowseButton from './BrowseButton.svelte';

  interface Props {
    id: string;
    label: string;
    /** The saved value. */
    value: string;
    /** Why `text` is not acceptable (null when it is). */
    check: (text: string) => string | null;
    /** Build the new settings with `text` in place. */
    apply: (text: string) => (s: import('./settings.js').Settings) => import('./settings.js').Settings;
    placeholder?: string;
    clearLabel?: string;
  }
  let { id, label, value, check, apply, placeholder = '', clearLabel = 'Clear' }: Props = $props();

  let draft = $state('');
  let refused = $state<string | null>(null);
  // Follow the saved value (after a load or a save).
  $effect(() => { draft = value; refused = null; });

  const problem = $derived(draft.trim() === value ? null : check(draft));

  async function commit(): Promise<void> {
    const text = draft.trim();
    if (text === value) { refused = null; return; }
    const why = check(text);
    if (why !== null) { refused = why; return; }
    refused = null;
    if (!(await settingsStore.update(apply(text)))) refused = settingsStore.problem?.message ?? 'That could not be saved.';
  }
  async function picked(folder: string): Promise<void> {
    draft = folder;
    await commit();
  }
  async function clear(): Promise<void> {
    draft = '';
    await commit();
  }
</script>

<div class="field">
  <label for={id}>{label}</label>
  <div class="row">
    <div class="field-box">
      <input {id} type="text" bind:value={draft} {placeholder} autocomplete="off" spellcheck="false" aria-invalid={problem !== null || refused !== null}
        aria-describedby={`${id}-note`} onblur={() => void commit()} onkeydown={event => { if (event.key === 'Enter') { event.preventDefault(); void commit(); } }} />
      <BrowseButton start={draft} onPick={folder => void picked(folder)} onError={message => { refused = message; }} />
    </div>
    <button type="button" onclick={() => void clear()} disabled={value === '' && draft === ''}>{clearLabel}</button>
  </div>
  <p class="note" id={`${id}-note`} role="status">{problem ?? refused ?? ''}</p>
</div>

<style>
  .field { margin: 0 0 4px; }
  label { display: block; font-size: 12px; margin: 0; }
  .row { display: flex; gap: 8px; align-items: stretch; }
  .field-box { margin-top: 5px; }
  input { margin: 0; padding: 7px 8px; }
  input[aria-invalid='true'] { border-color: var(--danger); }
  .row > button { margin-top: 5px; font-size: 12px; }
  .note { min-height: 16px; margin: 4px 0 0; color: var(--warn); font-size: 11px; }
</style>
