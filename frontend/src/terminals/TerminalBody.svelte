<script lang="ts">
  // What a pane or window shows for a terminal: the terminal itself, and for an agent Workbench launched, a
  // Terminal | Activity switch (the activity view follows the agent's transcript once it is found).
  import TerminalPane from './TerminalPane.svelte';
  import AgentActivity from './AgentActivity.svelte';
  import ModelPicker from './ModelPicker.svelte';
  import { agentStore } from './agent-store.svelte.js';
  import { agentLabel } from './agents.js';
  import type { TerminalError, TerminalInfo } from './client.js';

  interface Props {
    session: TerminalInfo;
    focused: boolean;
    onFocus: () => void;
    onError: (error: TerminalError) => void;
    onNotice: (text: string) => void;
  }
  let { session, focused, onFocus, onError, onNotice }: Props = $props();

  const entry = $derived(agentStore.entryOf(session.id));
  const link = $derived(agentStore.linkOf(session.id));
  const view = $derived(agentStore.viewOf(session.id));
  // The agent is waiting for a person and this terminal is not the one being used: the body's edge flashes.
  const flash = $derived(agentStore.needsHuman(session.id) && !focused);
</script>

{#if entry}
  <div class="agent-body" class:flash>
    <div class="agent-bar" role="group" aria-label={`${agentLabel(entry.agent)} view`}>
      <button type="button" class:on={view === 'terminal'} aria-pressed={view === 'terminal'} onclick={() => agentStore.setView(session.id, 'terminal')}>Terminal</button>
      <button type="button" class:on={view === 'activity'} aria-pressed={view === 'activity'} onclick={() => agentStore.setView(session.id, 'activity')}>Activity</button>
      <ModelPicker terminal={session.id} agent={entry.agent} currentModel={link?.state === 'linked' ? link.model : null} {onNotice} />
      <span class="agent-tag" title={entry.cwd}>{agentLabel(entry.agent)} · {link?.state === 'linked' ? 'transcript linked' : 'waiting for transcript'}</span>
    </div>
    <div class="agent-view">
      {#if view === 'activity'}
        <AgentActivity source={link?.state === 'linked' ? link.source : null} agent={agentLabel(entry.agent)} />
      {:else}
        <TerminalPane {session} {focused} {onFocus} {onError} {onNotice} />
      {/if}
    </div>
  </div>
{:else}
  <TerminalPane {session} {focused} {onFocus} {onError} {onNotice} />
{/if}

<style>
  .agent-body { position: relative; display: flex; flex-direction: column; width: 100%; height: 100%; min-height: 0; }
  /* Square, inside the window edge, never catches the mouse. Without motion it stays solid instead of pulsing. */
  .agent-body.flash::after { content: ''; position: absolute; inset: 0; z-index: 30; pointer-events: none; box-shadow: inset 0 0 0 3px var(--warn); animation: needs-human 1s ease-in-out infinite; }
  @keyframes needs-human { 0%, 100% { opacity: 1; } 50% { opacity: .15; } }
  @media (prefers-reduced-motion: reduce) { .agent-body.flash::after { animation: none; } }
  .agent-bar { display: flex; align-items: center; gap: 4px; flex: none; padding: 2px 6px; background: var(--bg); border-bottom: 1px solid var(--line); }
  .agent-bar button { font-size: 10px; padding: 1px 8px; }
  .agent-bar button.on { border-color: var(--accent); box-shadow: inset 0 -2px var(--accent); }
  .agent-tag { margin-left: auto; font: 10px ui-monospace, monospace; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .agent-view { flex: 1; min-height: 0; }
</style>
