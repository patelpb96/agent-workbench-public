<script lang="ts">
  // Asks before resuming a session that looks active somewhere else, and reports why a session could not be
  // opened in a terminal. Driven by the shared agent store; shown over everything until answered.
  import { agentStore } from './agent-store.svelte.js';
  import { agentLabel } from './agents.js';

  const pending = $derived(agentStore.pendingResume);
  const failure = $derived(agentStore.resumeError);
  let dialog = $state<HTMLDivElement | undefined>();
  let opener: Element | null = null;
  const open = $derived(pending !== null || failure !== null);

  const ago = (ms: number): string => (ms < 90_000 ? `${Math.max(1, Math.round(ms / 1000))} seconds ago` : `${Math.round(ms / 60_000)} minutes ago`);

  function close(): void {
    agentStore.cancelResume();
    agentStore.dismissResumeError();
  }
  $effect(() => {
    if (open) {
      opener = document.activeElement;
      queueMicrotask(() => dialog?.querySelector<HTMLButtonElement>('button')?.focus());
    } else if (opener instanceof HTMLElement && document.contains(opener)) {
      opener.focus();
      opener = null;
    }
  });
  function keys(event: KeyboardEvent): void {
    if (event.key === 'Escape') { event.preventDefault(); close(); }
    if (event.key === 'Tab' && dialog) {
      const items = [...dialog.querySelectorAll<HTMLElement>('button')];
      const first = items[0], last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    }
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="backdrop" onkeydown={keys}>
    <div class="box" role="alertdialog" aria-modal="true" aria-labelledby="resume-title" aria-describedby="resume-text" bind:this={dialog}>
      {#if pending}
        <h2 id="resume-title">{pending.running ? 'This session is running in another window' : 'This session may still be running'}</h2>
        <p id="resume-text">
          {#if pending.running}
            The {agentLabel(pending.agent)} session <strong>{pending.label}</strong> is open in another terminal window right now.
          {:else}
            The {agentLabel(pending.agent)} session <strong>{pending.label}</strong> was last written {ago(pending.ageMs)}, so it may still be open in another
            window.
          {/if}
          Opening it here starts a second copy, and two copies writing to one session mix up its history.
          Close the other one first (type <code>/exit</code> in it), then open it here.
        </p>
        <div class="actions">
          <button type="button" onclick={() => agentStore.cancelResume()}>Cancel</button>
          <button type="button" class="danger" disabled={agentStore.opening} onclick={() => agentStore.confirmResume()}>Open anyway</button>
        </div>
      {:else}
        <h2 id="resume-title">Could not open in a terminal</h2>
        <p id="resume-text">{failure}</p>
        <div class="actions"><button type="button" onclick={() => agentStore.dismissResumeError()}>OK</button></div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .backdrop { position: fixed; inset: 0; z-index: 70; background: #000b; display: flex; align-items: center; justify-content: center; padding: 24px; }
  .box { max-width: 34em; padding: 1.2em 1.4em; background: var(--bg, var(--bg)); border: 1px solid var(--line, var(--line)); color: var(--text, var(--text)); box-shadow: 0 20px 60px #000c; }
  h2 { margin: 0 0 .6em; font-size: 15px; }
  p { margin: 0 0 1em; font-size: 12px; line-height: 1.65; color: var(--muted, var(--muted)); }
  p strong { color: var(--text, var(--text)); font-weight: 500; }
  code { font-family: ui-monospace, monospace; }
  .actions { display: flex; justify-content: flex-end; gap: .6em; }
  .danger { border-color: #b87333; }
</style>
