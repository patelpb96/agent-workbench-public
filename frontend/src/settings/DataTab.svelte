<script lang="ts">
  // "Data": where Agent Workbench keeps things on this machine. Only places that exist are listed (so a fresh
  // machine shows little and a well-used one shows more). Read-only apart from "show in Explorer" and "copy path".
  import { onMount } from 'svelte';
  import { invoke, isTauri } from '@tauri-apps/api/core';
  import { createLocationsClient, type Location } from './locations.js';
  import type { TerminalError } from '../terminals/client.js';

  const client = createLocationsClient((command, args) =>
    isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'This is available in the desktop app.' }));

  let places = $state.raw<Location[] | null>(null);
  let problem = $state<string | null>(null);
  let copied = $state<string | null>(null);

  async function load(): Promise<void> {
    try {
      places = await client.list();
      problem = null;
    } catch (e) {
      places = [];
      problem = (e as TerminalError).message;
    }
  }
  async function open(place: Location): Promise<void> {
    try { await client.open(place.id); problem = null; }
    catch (e) { problem = (e as TerminalError).message; }
  }
  async function copy(place: Location): Promise<void> {
    try {
      await navigator.clipboard.writeText(place.path);
      copied = place.id;
      setTimeout(() => { if (copied === place.id) copied = null; }, 1500);
    } catch {
      problem = 'The path could not be copied; select it in the list instead.';
    }
  }
  onMount(() => void load());
</script>

<h3>Data</h3>
<p class="hint">Where Agent Workbench keeps things on this computer. Only places that exist here are listed. The private backup repository is encrypted and must never be shared.</p>
{#if problem}<p class="problem" role="status">{problem}</p>{/if}
{#if places === null}
  <p class="hint">Looking…</p>
{:else if places.length === 0 && !problem}
  <p class="hint">Nothing to show yet.</p>
{:else}
  <ul class="places" aria-label="Places on this computer">
    {#each places as place (place.id)}
      <li>
        <div class="what">
          <strong>{place.label}</strong>
          {#if place.detail}<small>{place.detail}</small>{/if}
          <code>{place.path}</code>
        </div>
        <div class="acts">
          <button type="button" onclick={() => void open(place)} title={place.file ? 'Show this file in Explorer' : 'Open this folder in Explorer'}>{place.file ? 'Show' : 'Open'}</button>
          <button type="button" onclick={() => void copy(place)} aria-label={`Copy the path of ${place.label}`}>{copied === place.id ? 'Copied' : 'Copy path'}</button>
        </div>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .hint { margin: 2px 0 12px; color: var(--muted, var(--muted)); font-size: 11px; line-height: 1.6; }
  .problem { margin: 0 0 10px; padding: .4em .6em; border: 1px solid #8b7042; color: var(--warn); font-size: 11px; }
  .places { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .5em; }
  li { display: flex; gap: .8em; align-items: center; justify-content: space-between; padding: .5em .7em; border: 1px solid var(--line, var(--line)); background: var(--surface, var(--surface)); }
  .what { min-width: 0; display: flex; flex-direction: column; gap: .2em; }
  strong { font-size: 12px; font-weight: 600; }
  small { color: var(--muted, var(--muted)); font-size: 10.5px; }
  code { font: 10.5px ui-monospace, monospace; color: var(--muted, var(--muted)); overflow-wrap: anywhere; user-select: text; }
  .acts { display: flex; gap: .4em; flex: none; }
  .acts button { font-size: 11px; white-space: nowrap; }
</style>
