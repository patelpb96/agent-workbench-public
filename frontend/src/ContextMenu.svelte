<script lang="ts" module>
  export interface MenuItem { label: string; hint?: string; disabled?: boolean; onselect: () => void }
</script>

<script lang="ts">
  // A small right-click menu: opens at the pointer (kept inside the window), arrow keys move, Enter picks,
  // Escape or a click elsewhere closes it, and focus goes back to where it was.
  import { onMount } from 'svelte';

  interface Props { x: number; y: number; items: readonly MenuItem[]; label: string; onClose: () => void }
  let { x, y, items, label, onClose }: Props = $props();

  let menu = $state<HTMLDivElement | undefined>();
  let width = $state(0);
  let height = $state(0);
  let opener: Element | null = null;
  // Keep the whole menu on screen without fixed sizes: measured after it renders.
  const left = $derived(Math.max(0, Math.min(x, window.innerWidth - width)));
  const top = $derived(Math.max(0, Math.min(y, window.innerHeight - height)));

  function buttons(): HTMLButtonElement[] {
    return [...(menu?.querySelectorAll<HTMLButtonElement>('button[role="menuitem"]:not(:disabled)') ?? [])];
  }
  function keys(event: KeyboardEvent): void {
    const all = buttons();
    const at = all.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onClose(); }
    else if (event.key === 'ArrowDown') { event.preventDefault(); all[(at + 1) % all.length]?.focus(); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); all[(at - 1 + all.length) % all.length]?.focus(); }
    else if (event.key === 'Home') { event.preventDefault(); all[0]?.focus(); }
    else if (event.key === 'End') { event.preventDefault(); all[all.length - 1]?.focus(); }
    else if (event.key === 'Tab') { event.preventDefault(); onClose(); }
  }
  function pick(item: MenuItem): void {
    onClose();
    item.onselect();
  }

  onMount(() => {
    opener = document.activeElement;
    buttons()[0]?.focus();
    const away = (event: Event) => { if (!menu?.contains(event.target as Node)) onClose(); };
    window.addEventListener('pointerdown', away, true);
    window.addEventListener('blur', onClose);
    window.addEventListener('resize', onClose);
    return () => {
      window.removeEventListener('pointerdown', away, true);
      window.removeEventListener('blur', onClose);
      window.removeEventListener('resize', onClose);
      if (opener instanceof HTMLElement && document.contains(opener)) opener.focus();
    };
  });
</script>

<div class="menu" role="menu" aria-label={label} tabindex="-1" bind:this={menu} bind:clientWidth={width} bind:clientHeight={height} style={`left:${left}px;top:${top}px;`}
  onkeydown={keys} oncontextmenu={event => event.preventDefault()}>
  {#each items as item (item.label)}
    <button type="button" role="menuitem" disabled={item.disabled} title={item.hint} onclick={() => pick(item)}>{item.label}</button>
  {/each}
</div>

<style>
  .menu { position: fixed; z-index: 80; display: flex; flex-direction: column; min-width: 12em; padding: 3px 0; background: var(--surface, #151e29); border: 1px solid var(--line, #344256); box-shadow: 0 10px 28px #000a; }
  button { display: block; width: 100%; text-align: left; border: 0; background: transparent; color: var(--text, #e4ebf5); padding: .5em 1em; font-size: 12px; white-space: nowrap; }
  button:hover:not(:disabled), button:focus-visible { background: var(--control, #1b2735); outline: none; box-shadow: inset 2px 0 #8abaff; }
  button:disabled { color: var(--muted, #a5b3c6); cursor: default; }
</style>
