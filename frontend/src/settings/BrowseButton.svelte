<script lang="ts">
  // A small folder button that sits inside the right edge of a folder text box and opens the Windows folder
  // picker. The chosen path goes to `onPick`; a cancelled dialog changes nothing. Put it in a `position: relative`
  // wrapper next to the input (see .browse in style.css).
  import { settingsStore } from './settings-store.svelte.js';

  interface Props {
    /** Where the picker opens (the text currently in the box). */
    start: string;
    onPick: (folder: string) => void;
    onError?: (message: string) => void;
    disabled?: boolean;
    label?: string;
  }
  let { start, onPick, onError, disabled = false, label = 'Browse for a folder' }: Props = $props();
  let busy = $state(false);

  async function browse(): Promise<void> {
    busy = true;
    try {
      const folder = await settingsStore.pickFolder(start);
      if (folder !== null) onPick(folder);
    } catch (e) {
      onError?.((e as { message?: string }).message ?? 'The folder picker is not available.');
    } finally {
      busy = false;
    }
  }
</script>

<button type="button" class="browse" aria-label={label} title={label} disabled={disabled || busy} onclick={() => void browse()}>
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" focusable="false"><path d="M1.5 3.5h4.2l1.4 1.6h7.4v7.9h-13z" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="miter" /></svg>
</button>
