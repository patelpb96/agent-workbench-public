<script lang="ts">
  // A collapsible block of the right sidebar. The header always shows the title and a one-line summary, so a
  // collapsed block still says what is in it; whether it is open is remembered between runs.
  import { untrack, type Snippet } from 'svelte';
  import { sectionOpen, setSectionOpen } from './section-state.js';

  interface Props {
    id: string;
    title: string;
    /** One line shown on the header, mostly useful while collapsed ("4 sessions"). */
    summary?: string;
    /** Open the first time, before the user has chosen. */
    open?: boolean;
    /** Marks the header when something in a collapsed block needs attention. */
    attention?: boolean;
    children: Snippet;
  }
  let { id, title, summary = '', open: openFirst = true, attention = false, children }: Props = $props();
  // Read once, when the section appears: after that the user's own clicks decide.
  let open = $state(untrack(() => sectionOpen(id, openFirst)));
  function toggle(): void {
    open = !open;
    setSectionOpen(id, open);
  }
</script>

<section class="sec" class:closed={!open} aria-label={title}>
  <h3 class="sec-title">
    <button type="button" class="sec-head" class:attention={attention && !open} aria-expanded={open} onclick={toggle}>
      <span class="sec-chev" aria-hidden="true">{open ? '▾' : '▸'}</span>
      <span class="sec-name">{title}</span>
      {#if summary}<span class="sec-sum">{summary}</span>{/if}
    </button>
  </h3>
  {#if open}<div class="sec-body">{@render children()}</div>{/if}
</section>

<style>
  .sec { border-top: 1px solid var(--line); padding: 0 0 6px; margin: 0; min-width: 0; }
  .sec-title { margin: 0; font-size: inherit; font-weight: inherit; }
  .sec-head { display: flex; align-items: baseline; gap: 8px; width: 100%; padding: 9px 0; border: 0; background: transparent; color: var(--text); text-align: left; font: 10px/1.6 ui-monospace, monospace; letter-spacing: .8px; text-transform: uppercase; }
  .sec-head:hover:not(:disabled) { border-color: transparent; color: var(--text); background: var(--control); }
  .sec-head.attention .sec-name::after { content: ' ●'; color: #e5bc6e; }
  .sec-chev { width: 1.2em; color: var(--muted); flex: none; font-size: 12px; }
  .sec-name { flex: none; color: var(--muted); }
  .sec-sum { margin-left: auto; color: var(--muted); text-transform: none; letter-spacing: 0; font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; }
  .sec-body { padding: 2px 0 10px; min-width: 0; }
</style>
