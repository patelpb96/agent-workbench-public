<script lang="ts">
  // Free-form terminal windows over the Canvas: drag by the title bar, resize from the corner,
  // overlap freely (a star, a cross, anything). Each window keeps its terminal while others move.
  import { canvasTerminals as store } from './canvas-store.svelte.js';
  import { clampWin, closeWatch, cycleWindow, dock, moveItem, raiseItem, resizeItem, shownBox, type Box, type Watch, type Win } from './floating.js';
  import AgentActivity from './AgentActivity.svelte';
  import { agentLabel } from './agents.js';
  import TerminalBody from './TerminalBody.svelte';
  import { agentStore } from './agent-store.svelte.js';
  import ContextMenu, { type MenuItem } from '../ContextMenu.svelte';
  import type { TerminalError } from './client.js';
  import { snapValue } from '../execution/layout.js';
  import { canvasWindow } from './keys.svelte.js';

  let width = $state(0);
  let height = $state(0);
  const size = $derived({ w: width, h: height });
  let errors = $state<Record<string, string>>({});
  let gesture = $state<{ id: string; kind: 'move' | 'resize'; px: number; py: number; win: Box } | null>(null);
  // Holding Shift during a drag or resize snaps the window to a 20 px grid and shows the grid (as on the session panels).
  let shiftHeld = $state(false);
  const snapping = $derived(gesture !== null && shiftHeld);

  // `key` is a terminal id (8 hex) or, for a watch window, an opaque inspector source id (16 hex).
  function begin(event: PointerEvent, key: string, box: Box, kind: 'move' | 'resize'): void {
    if (event.button !== 0) return;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    shiftHeld = event.shiftKey;
    // A minimized window moves from where it is drawn (the caller passes that position; its full size is kept).
    gesture = { id: key, kind, px: event.clientX, py: event.clientY, win: minimized(box) ? box : clampWin(box, size) };
    store.commit(raiseItem(store.state, key));
    event.preventDefault();
  }
  function move(event: PointerEvent): void {
    if (!gesture) return;
    const dx = event.clientX - gesture.px;
    const dy = event.clientY - gesture.py;
    const { id, kind, win } = gesture;
    shiftHeld = event.shiftKey;
    const snap = (n: number): number => snapValue(n, event.shiftKey);
    // Not saved on every move: the final position is stored when the pointer is released.
    store.preview(kind === 'move' ? moveItem(store.state, id, snap(win.x + dx), snap(win.y + dy), size) : resizeItem(store.state, id, snap(win.w + dx), snap(win.h + dy), size));
  }
  function end(event: PointerEvent): void {
    if (!gesture) return;
    (event.currentTarget as HTMLElement).releasePointerCapture?.(event.pointerId);
    gesture = null;
    shiftHeld = false;
    store.commit(store.state);
  }
  const minimized = (box: Box): boolean => 'stage' in box && (box as Win).stage !== undefined;
  /** A terminal window as drawn: a minimized one at its shown position, with its full size kept for restoring. */
  const asShown = (win: Win): Win => (win.stage ? { ...win, x: shownBox(win, size).x, y: shownBox(win, size).y } : win);
  /** The prefix's Ctrl/Alt+arrow resize (keys.svelte.ts): the bottom-right corner moves; a minimized window keeps its size. */
  function resizeBy(key: string, box: Box, dw: number, dh: number): boolean {
    if (minimized(box)) return false;
    const c = clampWin(box, size);
    store.commit(resizeItem(store.state, key, c.w + dw, c.h + dh, size));
    return true;
  }
  // Keyboard: arrows move the window by 16 px, Shift+arrows resize it (a minimized window only moves).
  function keys(event: KeyboardEvent, key: string, box: Box): void {
    const step = ({ ArrowLeft: [-16, 0], ArrowRight: [16, 0], ArrowUp: [0, -16], ArrowDown: [0, 16] } as Record<string, [number, number]>)[event.key];
    if (!step) return;
    event.preventDefault();
    const c = minimized(box) ? box : clampWin(box, size);
    store.commit(event.shiftKey ? resizeItem(store.state, key, c.w + step[0], c.h + step[1], size) : moveItem(store.state, key, c.x + step[0], c.y + step[1], size));
  }
  // Open a watched session in a terminal: from the button in its title bar or its right-click menu.
  let menu = $state<{ x: number; y: number; watch: Watch } | null>(null);
  const openWatch = (watch: Watch): Promise<void> => agentStore.openInTerminal(watch.source, watch.agent, watch.label);
  const menuItems = (watch: Watch): MenuItem[] => [
    { label: 'Open in terminal', hint: 'Resume this session in a Workbench terminal; this window becomes the terminal', disabled: agentStore.opening, onselect: () => void openWatch(watch) },
    { label: 'Stop watching', onselect: () => store.commit(closeWatch(store.state, watch.source)) }
  ];
  const fail = (id: string) => (e: TerminalError) => { errors = { ...errors, [id]: `${e.code}: ${e.message}` }; };
</script>

<svelte:window onkeydown={event => { if (gesture) shiftHeld = event.shiftKey; }} onkeyup={event => { if (gesture) shiftHeld = event.shiftKey; }} />
<div class="float-layer" bind:clientWidth={width} bind:clientHeight={height}>
  {#if snapping}<div class="float-grid" aria-hidden="true"></div>{/if}
  {#if width > 0}
    <!-- Stable DOM order (stacking is by z-index): moving a node in the DOM would drop the pointer capture of a drag in progress. -->
    {#each store.state.windows as win (win.terminal)}
      {@const info = store.terminals.find(t => t.id === win.terminal)}
      {@const c = shownBox(win, size)}
      {@const active = store.state.active === win.terminal}
      {@const name = info?.label ?? win.terminal}
      <!-- Minimized, the terminal itself is not drawn (it keeps running), so the window's own edge flashes when its agent waits for you. -->
      <section class="float-win" use:canvasWindow={{ resize: (dw, dh) => resizeBy(win.terminal, asShown(win), dw, dh) }} class:active class:gesture={gesture?.id === win.terminal} class:bar={win.stage === 'compact'} class:block={win.stage === 'block'} class:flash={win.stage !== undefined && agentStore.needsHuman(win.terminal)}
        style={`left:${c.x}px;top:${c.y}px;width:${c.w}px;height:${c.h}px;z-index:${win.z};`} aria-label={`Terminal window: ${name}${win.stage === 'compact' ? '; minimized to a bar' : win.stage === 'block' ? '; minimized to a block' : ''}${win.stage !== undefined && agentStore.needsHuman(win.terminal) ? '; waiting for you' : ''}`}>
        <div class="float-head">
          <button type="button" class="float-grip" title={win.stage === 'block' ? name : undefined} aria-label={`Move terminal ${name}; arrow keys move${win.stage ? '' : ', Shift plus arrows resize'}`}
            onpointerdown={event => begin(event, win.terminal, asShown(win), 'move')} onpointermove={move} onpointerup={end} onpointercancel={end}
            onfocus={() => store.commit(raiseItem(store.state, win.terminal))} onkeydown={event => keys(event, win.terminal, asShown(win))}>
            <span aria-hidden="true">::</span> <strong>{info?.label ?? 'Unavailable terminal'}</strong>
          </button>
          <button type="button" class="float-stage" aria-label={win.stage === undefined ? `Minimize ${name} to a bar` : win.stage === 'compact' ? `Minimize ${name} to a block` : `Restore ${name}`}
            title="Full → bar → block → full. The terminal keeps running while minimized." onclick={() => store.commit(cycleWindow(store.state, win.terminal, size))}>{win.stage === 'block' ? '+' : '−'}</button>
          {#if win.stage !== 'block'}<button type="button" class="float-close" aria-label={`Close terminal window ${name}`} title="Close this window. The terminal keeps running; show it again from the Terminals tab (Float) or a session panel." onclick={() => store.commit(dock(store.state, win.terminal))}>×</button>{/if}
        </div>
        {#if win.stage === undefined}
        <div class="float-body">
          {#if info}
            <TerminalBody session={info} focused={active} onFocus={() => store.commit(raiseItem(store.state, win.terminal))} onError={fail(win.terminal)} onNotice={() => undefined} />
          {:else}
            <p class="float-missing">{store.listed ? 'This terminal no longer exists.' : 'Waiting for the terminal service…'}</p>
          {/if}
          {#if errors[win.terminal]}<p class="float-error" role="status">{errors[win.terminal]}</p>{/if}
        </div>
        <button type="button" class="float-resize" aria-label={`Resize terminal ${name}`}
          onpointerdown={event => begin(event, win.terminal, win, 'resize')} onpointermove={move} onpointerup={end} onpointercancel={end}>↘</button>
        {/if}
      </section>
    {/each}
    <!-- Watch windows: the Activity of an existing agent session (no terminal); same gestures as terminal windows. -->
    {#each store.state.watches as watch (watch.source)}
      {@const c = clampWin(watch, size)}
      <section class="float-win watch" use:canvasWindow={{ resize: (dw, dh) => resizeBy(watch.source, watch, dw, dh) }} class:gesture={gesture?.id === watch.source} style={`left:${c.x}px;top:${c.y}px;width:${c.w}px;height:${c.h}px;z-index:${watch.z};`} aria-label={`Watching ${agentLabel(watch.agent)}: ${watch.label}`}
        oncontextmenu={event => { event.preventDefault(); menu = { x: event.clientX, y: event.clientY, watch }; }}>
        <div class="float-head">
          <button type="button" class="float-grip" aria-label={`Move the ${agentLabel(watch.agent)} watch window ${watch.label}; arrow keys move, Shift plus arrows resize`}
            onpointerdown={event => begin(event, watch.source, watch, 'move')} onpointermove={move} onpointerup={end} onpointercancel={end}
            onfocus={() => store.commit(raiseItem(store.state, watch.source))} onkeydown={event => keys(event, watch.source, watch)}>
            <span aria-hidden="true">::</span> <strong>{agentLabel(watch.agent)} · {watch.label}</strong> <small>watching</small>
          </button>
          <button type="button" class="float-action" aria-label={`Open ${watch.label} in a terminal`} title="Resume this session in a Workbench terminal; this window becomes the terminal. Close the original first if it is still running." disabled={agentStore.opening} onclick={() => void openWatch(watch)}>Open in terminal</button>
          <button type="button" class="float-close" aria-label={`Stop watching ${watch.label}`} title="Close this window. The agent session itself is untouched." onclick={() => store.commit(closeWatch(store.state, watch.source))}>×</button>
        </div>
        <div class="float-body"><AgentActivity source={watch.source} agent={agentLabel(watch.agent)} /></div>
        <button type="button" class="float-resize" aria-label={`Resize the watch window ${watch.label}`}
          onpointerdown={event => begin(event, watch.source, watch, 'resize')} onpointermove={move} onpointerup={end} onpointercancel={end}>↘</button>
      </section>
    {/each}
  {/if}
</div>
{#if menu}<ContextMenu x={menu.x} y={menu.y} label={`Actions for ${menu.watch.label}`} items={menuItems(menu.watch)} onClose={() => { menu = null; }} />{/if}

<style>
  .float-layer { position: absolute; inset: 0; z-index: 6; pointer-events: none; overflow: hidden; }
  .float-grid { position: absolute; inset: 0; z-index: 0; pointer-events: none; background-image: linear-gradient(#8298b733 1px, transparent 1px), linear-gradient(90deg, #8298b733 1px, transparent 1px); background-size: 20px 20px; }
  .float-win { position: absolute; box-sizing: border-box; display: flex; flex-direction: column; pointer-events: auto; border: 1px solid var(--line); background: var(--bg); box-shadow: none; overflow: hidden; }
  .float-win.active { border-color: var(--accent); box-shadow: none; outline: 1px solid var(--accent); outline-offset: -1px; }
  .float-win.gesture { user-select: none; }
  .float-win.watch { border-style: dashed; }
  .float-grip small { color: var(--muted); font-size: 10px; margin-left: 6px; }
  .float-head { display: flex; align-items: stretch; height: 28px; flex: none; background: var(--surface); border-bottom: 1px solid var(--line); }
  .float-grip { flex: 1; min-width: 0; text-align: left; border: 0; background: transparent; color: var(--text); cursor: grab; touch-action: none; padding: 0 8px; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .float-win.gesture .float-grip { cursor: grabbing; }
  .float-action { border: 0; border-left: 1px solid var(--line); background: transparent; color: var(--text); padding: 0 .9em; font-size: 11px; white-space: nowrap; }
  .float-action:hover:not(:disabled) { background: var(--control); color: var(--text); }
  .float-stage { border: 0; background: transparent; color: var(--text); padding: 0 9px; font-size: 15px; }
  .float-stage:hover { background: var(--control); color: var(--text); }
  .float-win.bar .float-head, .float-win.block .float-head { height: 100%; border-bottom: 0; }
  /* A block: the grip fills the square (two letters of the name show; the whole name is its tooltip), restore on the right. */
  .float-win.block .float-grip { padding: 0 4px; font-size: 11px; text-overflow: clip; }
  .float-win.block .float-grip span { display: none; }
  .float-win.block .float-stage { padding: 0 6px; }
  /* Same amber edge as a full agent terminal waiting for you (TerminalBody); solid without motion. */
  .float-win.flash::after { content: ''; position: absolute; inset: 0; z-index: 2; pointer-events: none; box-shadow: inset 0 0 0 3px var(--warn); animation: float-needs-human 1s ease-in-out infinite; }
  @keyframes float-needs-human { 0%, 100% { opacity: 1; } 50% { opacity: .15; } }
  @media (prefers-reduced-motion: reduce) { .float-win.flash::after { animation: none; } }
  .float-close { border: 0; background: transparent; color: var(--text); padding: 0 10px; font-size: 16px; }
  .float-close:hover { background: var(--danger); }
  .float-body { position: relative; flex: 1; min-height: 0; }
  .float-missing { margin: 12px; color: var(--muted); font-size: 12px; }
  .float-error { position: absolute; left: 0; right: 0; bottom: 0; margin: 0; padding: 4px 8px; background: var(--surface); color: var(--danger); font-size: 11px; }
  .float-resize { position: absolute; right: 0; bottom: 0; width: 18px; height: 18px; padding: 0; border: 0; background: var(--control); color: var(--text); font-size: 12px; line-height: 18px; cursor: nwse-resize; touch-action: none; z-index: 1; }
</style>
