<script lang="ts">
  // The start screen shown after "Sign out" (docs/design/start-screen.md). Today it is a stand-in: signing out only covers
  // the window, nothing is ended and no credential is touched, and "Sign in" returns because the keys already on this PC
  // are used as they are. The provider list is the planned layout, not a live check.
  import { onMount } from 'svelte';
  import { logoPath } from './brand.js';
  import type { TerminalInfo } from './terminals/client.js';

  interface Props {
    terminals: readonly TerminalInfo[];
    onSignIn: () => void;
  }
  let { terminals, onSignIn }: Props = $props();

  const running = $derived(terminals.filter(t => t.state === 'running'));
  const previous = $derived(terminals.filter(t => t.state !== 'running'));
  const groups = [
    { name: 'LLM', providers: [{ name: 'Claude Code', status: 'Saved sign-in on this PC' }, { name: 'Codex', status: 'Saved sign-in on this PC' }, { name: 'Pi', status: 'Saved sign-in on this PC' }] },
    { name: 'Sys1', providers: [{ name: 'Laya', status: 'Local model' }, { name: 'Jev', status: 'API key (not checked here)' }, { name: 'Other', status: 'Not set up' }] },
    { name: 'NCM', providers: [{ name: 'NCM', status: 'Not supported' }] }
  ] as const;

  let button = $state<HTMLButtonElement>();
  onMount(() => { button?.focus(); });
</script>

<div class="start" role="dialog" aria-modal="true" aria-label="Agent Workbench start screen">
  <div class="card">
    <header>
      <svg class="mark" viewBox="0 0 256 256" aria-hidden="true"><rect x="3" y="3" width="250" height="250" /><path d={logoPath} /></svg>
      <div><h1>AGENT WORKBENCH</h1><p class="sub">Signed out. Your terminals and agents keep running.</p></div>
    </header>

    <section aria-label="Sessions">
      <h2>Sessions</h2>
      <div class="cols">
        <div><h3>Running <span>{running.length}</span></h3>
          {#if running.length === 0}<p class="none">None</p>{:else}<ul>{#each running.slice(0, 6) as t (t.id)}<li>{t.label}</li>{/each}{#if running.length > 6}<li class="more">+{running.length - 6} more</li>{/if}</ul>{/if}</div>
        <div><h3>Previous <span>{previous.length}</span></h3>
          {#if previous.length === 0}<p class="none">None</p>{:else}<ul>{#each previous.slice(0, 6) as t (t.id)}<li>{t.label} <small>{t.state}</small></li>{/each}</ul>{/if}</div>
        <div><h3>Archive</h3><p class="none">Not built yet</p></div>
      </div>
    </section>

    <section aria-label="Providers">
      <h2>Providers</h2>
      <div class="cols">
        {#each groups as group (group.name)}
          <div><h3>{group.name}</h3><ul>{#each group.providers as p (p.name)}<li class:off={p.status === 'Not supported'}>{p.name} <small>{p.status}</small></li>{/each}</ul></div>
        {/each}
      </div>
      <p class="note">Preview: these lines are the planned layout, not a live check of your keys. Signing in uses what is already saved on this PC.</p>
    </section>

    <footer>
      <button type="button" class="primary" bind:this={button} onclick={onSignIn}>Sign in</button>
      <button type="button" disabled title="Planned: enter or replace a key (stored with Windows credential protection)">Set API key…</button>
      <button type="button" disabled title="Planned: sign in through the provider's web page">Sign in with browser…</button>
    </footer>
  </div>
</div>

<style>
  /* Below the title bar (z-index 60), so the window can still be moved, minimized and closed while signed out. */
  .start { position: fixed; inset: 0; z-index: 59; display: flex; align-items: center; justify-content: center; padding: 24px; background: var(--bg, #101720); color: var(--text, #e4ebf5); overflow: auto; }
  .card { width: min(760px, 100%); border: 1px solid var(--line, #344256); background: var(--surface, #151e29); padding: 22px 26px; }
  header { display: flex; align-items: center; gap: 16px; margin-bottom: 18px; }
  .mark { width: 52px; height: 52px; flex: none; }
  .mark rect { fill: #3a3f45; }
  .mark path { fill: #ece8df; }
  h1 { margin: 0; font-size: 15px; letter-spacing: 1.6px; font-weight: 600; }
  .sub { margin: 4px 0 0; color: var(--muted, #a5b3c6); font-size: 12px; }
  section { border-top: 1px solid var(--line, #344256); padding: 14px 0 6px; }
  h2 { margin: 0 0 10px; font: 10px ui-monospace, monospace; letter-spacing: 1px; color: var(--muted, #a5b3c6); font-weight: 400; text-transform: uppercase; }
  .cols { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 16px; }
  h3 { margin: 0 0 6px; font-size: 12px; font-weight: 600; }
  h3 span { color: var(--muted, #a5b3c6); font-weight: 400; margin-left: 4px; }
  ul { list-style: none; margin: 0; padding: 0; font-size: 12px; }
  li { padding: 3px 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  li small { color: var(--muted, #a5b3c6); font-size: 10px; margin-left: 4px; }
  li.off { color: var(--muted, #a5b3c6); }
  .more, .none { color: var(--muted, #a5b3c6); font-size: 11px; margin: 0; }
  .note { color: var(--muted, #a5b3c6); font-size: 10px; line-height: 1.7; margin: 12px 0 4px; }
  footer { display: flex; gap: 8px; flex-wrap: wrap; border-top: 1px solid var(--line, #344256); padding-top: 14px; margin-top: 8px; }
  footer button { font-size: 12px; padding: 8px 14px; }
  .primary { background: #2c5d9c; border-color: #4c7fc0; color: #fff; }
  @media (max-width: 640px) { .cols { grid-template-columns: 1fr; } }
</style>
