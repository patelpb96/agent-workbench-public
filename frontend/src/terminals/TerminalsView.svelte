<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import { terminalRequests } from './requests.svelte.js';
  import { fmtRemaining, filterPhrase, grantDurations, validAgentName, type DraftView, type GrantDuration, type GrantView, type TerminalError, type TerminalInfo } from './client.js';
  import { applyPreset, assign, clear, close, defaultState, DIVIDER_PX, droppedSession, focus, focusDirection, focusedSession as paneSession, focusIndex, leafCount, leafForNew, leafOf, leaves, MAX_LEAVES, MIN_PANE_HEIGHT_PX, MIN_PANE_WIDTH_PX, nextPreset, parse, PRESETS, ratioInRegion, reconcile, rects, resizeFocused, setRatio, split, splitRegions, tile, zoomedRects, type PaneState, type Preset, type Split, type SplitDir } from './panes.js';
  import { keyControl } from './keys.svelte.js';
  import { broadcast } from './broadcast.svelte.js';
  import { logStore } from './logs.svelte.js';
  import { PREFIX_LABEL, type Action } from './keymap.js';
  import { agentClient, loadSavedPanes, savePanes, terminalAvailable, terminalClient } from './tauri.js';
  import TerminalBody from './TerminalBody.svelte';
  import { agentStore } from './agent-store.svelte.js';
  import { agentChoices, agentLabel, folderProblem, type AgentName } from './agents.js';
  import { settingsStore } from '../settings/settings-store.svelte.js';
  import BrowseButton from '../settings/BrowseButton.svelte';
  import { canvasTerminals as canvas } from './canvas-store.svelte.js';
  import { floatTerminal, isFloating, occupied, release } from './floating.js';

  const STORE_KEY = 'wb.terminals.panes';

  let sessions = $state.raw<TerminalInfo[]>([]);
  let panes = $state.raw<PaneState>(loadPanes());
  let error = $state.raw<TerminalError | null>(null);
  let notice = $state<string | null>(null);
  let confirmEnd = $state<string | null>(null);
  let extended = $state(false);
  let draft = $state.raw<DraftView | null>(null);
  let grants = $state.raw<GrantView[]>([]);
  let agentName = $state('');
  let duration = $state<GrantDuration>('fifteen_minutes');
  let keepAfterDetach = $state(false);
  let phrase = $state('');
  let phraseCurrent = $state('');
  let phraseNew = $state('');
  let accessNote = $state<string | null>(null);

  let listTimer: ReturnType<typeof setInterval> | undefined;
  let accessTimer: ReturnType<typeof setInterval> | undefined;
  // Timers must not stack requests while the service is slow or restarting.
  let listBusy = false;
  let accessBusy = false;

  const available = terminalAvailable();
  const agentOk = $derived(validAgentName(agentName));
  const order = $derived(leaves(panes.root));
  // tmux's zoom (prefix+z): one pane over the whole view until zoomed again or another pane is chosen. Not saved.
  let zoomed = $state<number | null>(null);
  const paneRects = $derived(zoomedRects(rects(panes.root), zoomed));
  const regions = $derived(zoomed === null ? splitRegions(panes.root) : []);
  // The layout the last prefix+Space (or the layout menu) applied, so the next press moves on from it.
  let preset = $state<Preset | null>(null);
  // Typing into every shown pane at once (prefix+S); off whenever this view closes.
  let synced = $state(false);
  const prefixName = $derived(PREFIX_LABEL[settingsStore.value.keys.prefix]);
  const presetLabel: Readonly<Record<Preset, string>> = { 'even-horizontal': 'Side by side', 'even-vertical': 'Stacked', 'main-horizontal': 'Main on top', 'main-vertical': 'Main on the left', tiled: 'Tiled' };
  const focusedId = $derived(paneSession(panes));
  const focusedSession = $derived(sessions.find(s => s.id === focusedId));
  let paneArea = $state<HTMLDivElement | undefined>();
  let dragging = $state<number | null>(null);
  const numberOf = (leafId: number): number => order.findIndex(l => l.id === leafId) + 1;
  const pct = (n: number): string => `${(n * 100).toFixed(4)}%`;

  function loadPanes(): PaneState {
    try {
      const raw = localStorage.getItem(STORE_KEY);
      return raw ? parse(JSON.parse(raw)) : defaultState();
    } catch {
      return defaultState();
    }
  }
  // The arrangement lives in the app data folder (native file); localStorage is only the first-run
  // source for an arrangement saved by an earlier build, and the fallback outside the desktop app.
  let touched = false;
  function update(next: PaneState): void {
    touched = true;
    panes = next;
    savePanes(next);
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify(next));
    } catch {
      /* storage unavailable: the arrangement just is not remembered */
    }
  }
  onMount(() => {
    void loadSavedPanes().then(saved => {
      // A change made before the file was read wins; never overwrite what the user just did.
      if (touched || saved === null || saved === undefined) {
        if (!touched && (saved === null || saved === undefined)) savePanes(panes);
        return;
      }
      panes = parse(saved);
    });
  });

  // Dividers (one per split): drag with the pointer, nudge with the arrow keys, double-click to reset.
  function dragStart(node: Split, event: PointerEvent): void {
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    dragging = node.id;
    event.preventDefault();
  }
  function dragMove(node: Split, at: { x: number; y: number; w: number; h: number }, event: PointerEvent): void {
    if (dragging !== node.id || !paneArea) return;
    const box = paneArea.getBoundingClientRect();
    if (box.width <= 0 || box.height <= 0) return;
    const px = (event.clientX - box.left) / box.width;
    const py = (event.clientY - box.top) / box.height;
    // Not persisted on every move: the terminals re-fit while dragging, and the final size is stored on release.
    panes = setRatio(panes, node.id, ratioInRegion(px, py, node.dir, at));
  }
  function dragEnd(event: PointerEvent): void {
    if (dragging === null) return;
    (event.currentTarget as HTMLElement).releasePointerCapture?.(event.pointerId);
    dragging = null;
    update(panes);
  }
  function nudge(node: Split, event: KeyboardEvent): void {
    const grow = node.dir === 'row' ? 'ArrowRight' : 'ArrowDown';
    const shrink = node.dir === 'row' ? 'ArrowLeft' : 'ArrowUp';
    if (event.key !== grow && event.key !== shrink) return;
    event.preventDefault();
    update(setRatio(panes, node.id, node.ratio + (event.key === grow ? 0.02 : -0.02)));
  }

  // Keyboard: Alt+arrows move focus between panes, Alt+1..9 jump to the n-th pane (like Windows Terminal / tmux).
  // Handled in the capture phase so the focused terminal never receives these keys.
  function onKeydown(event: KeyboardEvent): void {
    if (!event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const dir = ({ ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' } as const)[event.key as 'ArrowLeft'];
    const digit = /^[1-9]$/.test(event.key) ? Number(event.key) - 1 : null;
    if (dir === undefined && digit === null) return;
    const next = dir !== undefined ? focusDirection(panes, dir) : focusIndex(panes, digit!);
    event.preventDefault();
    event.stopPropagation();
    if (next !== panes) update(next);
  }

  // Dragging a terminal tab onto a pane shows it there.
  let dropTarget = $state<number | null>(null);
  function onDragStart(id: string, event: DragEvent): void {
    event.dataTransfer?.setData('text/plain', id);
    if (event.dataTransfer) event.dataTransfer.effectAllowed = 'move';
  }
  function onDrop(leafId: number, event: DragEvent): void {
    event.preventDefault();
    dropTarget = null;
    const id = droppedSession(event.dataTransfer?.getData('text/plain'));
    if (id !== null && sessions.some(s => s.id === id && s.state !== 'lost')) { claim(id); update(assign(panes, leafId, id)); }
  }

  // tmux-style pane commands. A split needs room: like tmux ("no space for new pane") it is refused
  // when either half would end up smaller than a usable terminal.
  function splitPane(leafId: number, dir: SplitDir): boolean {
    const el = paneArea?.querySelector<HTMLElement>(`.pane[data-leaf="${leafId}"]`);
    if (leafCount(panes) >= MAX_LEAVES) { notice = `No more panes: the limit is ${MAX_LEAVES}.`; return false; }
    const box = el?.getBoundingClientRect();
    const room = box ? (dir === 'row' ? box.width >= 2 * MIN_PANE_WIDTH_PX + DIVIDER_PX : box.height >= 2 * MIN_PANE_HEIGHT_PX + DIVIDER_PX) : true;
    if (!room) { notice = 'No space for a new pane here. Make the window larger, close a pane, or split a larger one.'; return false; }
    notice = null;
    zoomed = null;
    update(split(panes, leafId, dir));
    return true;
  }
  function applyLayout(next: Preset): void {
    if (order.every(l => l.session === null)) { notice = 'No terminals in the panes to arrange yet.'; return; }
    preset = next;
    zoomed = null;
    update(applyPreset(panes, next));
    notice = `Layout: ${presetLabel[next]}.`;
  }
  function toggleSync(): void {
    synced = !synced;
    notice = synced ? `Typing goes to every pane shown here. ${prefixName} then S turns it off.` : 'Typing goes to one pane again.';
  }
  // Typing into all panes reaches only the panes on screen: a zoom hides the others, and they get nothing until it ends.
  const visibleSessions = $derived(order.filter(l => paneRects.has(l.id)).map(l => l.session).filter((s): s is string => s !== null));
  $effect(() => {
    broadcast.set(synced ? visibleSessions : []);
  });
  // A different pane chosen while one is zoomed shows the whole arrangement again (as in tmux).
  $effect(() => {
    if (zoomed !== null && zoomed !== panes.focused) zoomed = null;
  });

  /** The keymap's pane commands (prefix then a key), while this view is open. */
  function runAction(action: Action): boolean {
    switch (action.kind) {
      case 'split':
        // Like tmux, a split from the keyboard starts a shell in the new pane.
        if (splitPane(panes.focused, action.dir)) void create();
        return true;
      case 'focus': update(focusDirection(panes, action.dir)); return true;
      // tmux: Ctrl+arrow by a little, Alt+arrow by more (1 and 5 cells there).
      case 'resize': update(resizeFocused(panes, action.dir, action.big ? 0.1 : 0.02)); return true;
      case 'select': update(focusIndex(panes, action.index)); return true;
      case 'next-pane': {
        const at = order.findIndex(l => l.id === panes.focused);
        update(focusIndex(panes, (at + 1) % order.length));
        return true;
      }
      case 'close-pane': closePane(panes.focused); return true;
      case 'zoom':
        if (order.length < 2) { notice = 'There is only one pane: nothing to zoom.'; return true; }
        zoomed = zoomed === null ? panes.focused : null;
        return true;
      case 'next-layout': applyLayout(nextPreset(preset)); return true;
      case 'new-terminal': void create(); return true;
      case 'sync': toggleSync(); return true;
      default: return false;
    }
  }
  function closePane(leafId: number): void {
    if (zoomed === leafId) zoomed = null;
    update(close(panes, leafId));
  }
  function tileAll(): void {
    const ids = sessions.filter(s => s.state !== 'lost').map(s => s.id);
    if (ids.length === 0) { notice = 'No terminals to arrange yet.'; return; }
    notice = ids.length > MAX_LEAVES ? `Showing the first ${MAX_LEAVES} of ${ids.length} terminals.` : null;
    update(tile(panes, ids));
  }
  function singlePane(): void {
    const keep = focusedId;
    update(keep ? assign(defaultState(), 1, keep) : defaultState());
  }

  async function refresh(): Promise<void> {
    if (listBusy) return;
    listBusy = true;
    try {
      sessions = await terminalClient.list();
      // Forget panes whose session no longer exists, but only after a successful listing.
      update(reconcile(panes, new Set(sessions.map(s => s.id))));
      // REC marks in the list, and logs that stopped by themselves (the size cap, a write error).
      void logStore.poll(sessions.filter(s => s.state !== 'lost').map(s => s.id)).then(stopped => { if (stopped.length > 0) notice = stopped.join(' '); });
      if (error?.code === 'terminal_unavailable') error = null;
    } catch (e) {
      error = e as TerminalError;
    } finally {
      listBusy = false;
    }
  }

  const fail = (e: TerminalError) => { error = e; };

  // A terminal is shown in one place only: floating it takes it out of its pane, and showing it in a pane
  // takes it off the canvas (floating window or flipped session panel).
  const CANVAS_GUESS = { w: 1400, h: 800 };
  function floatOut(id: string): void {
    canvas.commit(floatTerminal(canvas.state, id, CANVAS_GUESS));
    notice = 'Opened on the Canvas. Drag it anywhere there; use Terminals to bring it back.';
  }
  function claim(id: string): void {
    if (occupied(canvas.state).has(id)) canvas.commit(release(canvas.state, id));
  }
  $effect(() => {
    const onCanvas = occupied(canvas.state);
    let next = panes;
    for (const leaf of leaves(next.root)) if (leaf.session !== null && onCanvas.has(leaf.session)) next = clear(next, leaf.id);
    if (next !== panes) update(next);
  });

  function show(info: TerminalInfo): void {
    claim(info.id);
    // Showing a terminal that is already in a pane just focuses that pane.
    const existing = leafOf(panes, info.id);
    update(existing !== null ? focus(panes, existing) : assign(panes, panes.focused, info.id));
  }

  async function create(folder?: string): Promise<void> {
    try {
      const info = await terminalClient.create(100, 30, `Terminal ${sessions.length + 1}`, extended, folder);
      await refresh();
      update(assign(panes, leafForNew(panes), info.id));
      if (folder !== undefined) notice = `New terminal started in ${folder}.`;
    } catch (e) {
      error = e as TerminalError;
    }
  }
  // The Files panel's "Open terminal here".
  $effect(() => {
    if (terminalRequests.openHere === null) return;
    const folder = untrack(() => terminalRequests.takeOpen());
    if (folder !== null) void create(folder);
  });

  // Launch an agent (Claude Code, Pi, Codex) in a new terminal and show it; its transcript is linked automatically.
  const FOLDER_KEY = 'wb.agents.folder'; // read once to carry an earlier remembered folder into Properties
  let launchAgent = $state<AgentName>('claude');
  let launchFolder = $state('');
  // The Properties settings are the source of truth: follow them when they load or change.
  $effect(() => { extended = settingsStore.value.terminals.extended_history; });
  $effect(() => {
    const saved = settingsStore.value.agents.launch_folder;
    if (saved !== '' || !settingsStore.loaded) { launchFolder = saved; return; }
    // First run after this setting existed: bring the folder an earlier build remembered in the browser storage along.
    let old = '';
    try { old = localStorage.getItem(FOLDER_KEY) ?? ''; } catch { /* none */ }
    if (old !== '' && folderProblem(old) === null) void settingsStore.update(s => ({ ...s, agents: { ...s.agents, launch_folder: old } }));
    launchFolder = old;
  });
  // The Files panel's "Launch agent here" (after the settings above, so it is not overwritten when the tab opens).
  $effect(() => {
    if (terminalRequests.launchHere === null) return;
    const folder = untrack(() => terminalRequests.takeLaunch());
    if (folder === null) return;
    launchFolder = folder;
    notice = folderProblem(folder) ?? `Folder set to ${folder}. Pick the agent and press Launch.`;
  });
  let launching = $state(false);
  const folderNote = $derived(launchFolder === '' ? null : folderProblem(launchFolder));
  async function launchNow(): Promise<void> {
    const problem = folderProblem(launchFolder);
    if (problem !== null) { notice = problem; return; }
    launching = true;
    try {
      const { terminal } = await agentStore.launch(launchAgent, launchFolder.trim());
      // The folder just used becomes the default for next time (shown and editable in Properties > Agents).
      void settingsStore.update(s => ({ ...s, agents: { ...s.agents, launch_folder: launchFolder.trim() } }));
      await refresh();
      update(assign(panes, leafForNew(panes), terminal.id));
      notice = `${agentLabel(launchAgent)} is starting in ${launchFolder.trim()}. Its transcript is linked automatically once it exists; switch a pane to Activity to follow it.`;
    } catch (e) {
      error = e as TerminalError;
    } finally {
      launching = false;
    }
  }

  async function end(id: string): Promise<void> {
    if (confirmEnd !== id) {
      confirmEnd = id;
      return;
    }
    confirmEnd = null;
    try {
      const leaf = leafOf(panes, id);
      if (leaf !== null) update(clear(panes, leaf));
      await terminalClient.end(id);
    } catch (e) {
      error = e as TerminalError;
    }
    await refresh();
  }

  // Terminals that did not survive a Windows restart are listed as lost; a click recreates them (and resumes their agents).
  const lostSessions = $derived(sessions.filter(s => s.state === 'lost'));
  let restoring = $state(false);
  async function restoreOne(id: string): Promise<void> {
    try {
      if (agentStore.entryOf(id)) await agentClient.restore(id);
      else await terminalClient.restore(id);
    } catch (e) {
      const err = e as TerminalError;
      notice = `Terminal ${id} could not be restored: ${err.message}`;
    }
  }
  async function restoreAll(): Promise<void> {
    if (restoring) return;
    restoring = true;
    notice = null;
    try {
      for (const s of lostSessions) await restoreOne(s.id);
    } finally {
      restoring = false;
      await refresh();
      await agentStore.refresh();
    }
  }
  async function restoreSingle(id: string): Promise<void> {
    if (restoring) return;
    restoring = true;
    notice = null;
    try {
      await restoreOne(id);
    } finally {
      restoring = false;
      await refresh();
      await agentStore.refresh();
    }
  }

  async function refreshAccess(): Promise<void> {
    const id = focusedId;
    if (id === null) {
      draft = null;
      grants = [];
      return;
    }
    if (document.hidden || accessBusy) return;
    accessBusy = true;
    try {
      const [d, g] = await Promise.all([terminalClient.draft(id), terminalClient.grants(id)]);
      if (id !== focusedId) return;
      draft = d.draft;
      grants = g;
    } catch (e) {
      error = e as TerminalError;
    } finally {
      accessBusy = false;
    }
  }

  async function accessAction(action: () => Promise<unknown>, done?: string): Promise<void> {
    try {
      await action();
      accessNote = done ?? null;
    } catch (e) {
      accessNote = null;
      error = e as TerminalError;
    }
    await refreshAccess();
  }

  const acceptDraft = () => focusedId && accessAction(() => terminalClient.draftAccept(focusedId!), 'Typed into the terminal without pressing Enter. Review it and press Enter yourself.');
  const discardDraft = () => focusedId && accessAction(() => terminalClient.draftDiscard(focusedId!));
  const revoke = (agent: string) => focusedId && accessAction(() => terminalClient.grantRevoke(agent, focusedId!), `Revoked ${agent}.`);
  async function allow(): Promise<void> {
    if (!focusedId || !agentOk) return;
    const id = focusedId;
    await accessAction(() => terminalClient.grantCreate(agentName, id, duration, keepAfterDetach, phrase), `Allowed ${agentName} to type here.`);
    phrase = '';
  }
  async function changePhrase(): Promise<void> {
    await accessAction(() => terminalClient.gateSet(phraseCurrent, phraseNew), 'Confirmation phrase changed.');
    phraseCurrent = '';
    phraseNew = '';
  }

  // The access panel follows whichever pane has focus.
  $effect(() => {
    void focusedId;
    draft = null;
    grants = [];
    accessNote = null;
    void refreshAccess();
  });

  let stopPaneKeys: (() => void) | undefined;
  onMount(() => {
    void refresh();
    listTimer = setInterval(() => void refresh(), 3000);
    accessTimer = setInterval(() => void refreshAccess(), 1500);
    window.addEventListener('keydown', onKeydown, true);
    stopPaneKeys = keyControl.registerPanes(runAction);
  });
  onDestroy(() => {
    clearInterval(listTimer);
    clearInterval(accessTimer);
    window.removeEventListener('keydown', onKeydown, true);
    stopPaneKeys?.();
    broadcast.set([]);
  });

  async function toggleLog(id: string): Promise<void> {
    notice = await logStore.toggle(id);
  }
</script>

<section class="terminals" aria-label="Terminal sessions">
  <header class="terminal-head">
    <div>
      <span class="eyebrow">TERMINALS</span>
      <h1>Persistent terminals</h1>
    </div>
    <div class="new-terminal">
      <label class="extended" title="Keeps up to 32 MiB of output in the terminal's own memory (about 300,000 lines) instead of 1 MiB. Memory only, never written to disk; it ends with the session.">
        <input type="checkbox" bind:checked={extended} disabled={!available} /> Extended history (32 MiB)
      </label>
      <button type="button" onclick={() => void create()} disabled={!available}>New terminal</button>
    </div>
  </header>
  <p class="terminal-hint">
    Each terminal runs as its own background process. Closing Workbench does not end it; reopen and it is still here.
    Use <strong>End session</strong> to stop one. Click a terminal below to show it in the highlighted pane, or drag it onto a pane.
    Split a pane with the buttons in its header (up to {MAX_LEAVES} panes), or use <strong>Tile all</strong> to show every terminal at once.
    Alt+arrows move between panes and Alt+1 to Alt+9 jump to one.
    {#if settingsStore.value.keys.prefix !== 'off'}In a terminal, <strong>{prefixName}</strong> works like tmux's prefix: {prefixName} then % or " splits, then [ copies, then / searches, then ? lists everything. Change or turn it off in Properties.{/if}
  </p>
  <p class="terminal-hint small">Start folder, extended-history default and start-at-sign-in are in <strong>Properties</strong> (the gear in the title bar).</p>
  {#if !available}<div class="terminal-note">Terminals are available in the desktop app only.</div>{/if}
  {#if error}<div class="terminal-note error" role="status"><strong>{error.code}</strong>: {error.message}</div>{/if}
  {#if notice}<div class="terminal-note" role="status">{notice}</div>{/if}

  <form class="launch" onsubmit={event => { event.preventDefault(); void launchNow(); }} aria-label="Launch an agent in a terminal">
    <label>Launch agent
      <select bind:value={launchAgent} aria-label="Agent to launch" disabled={!available || launching}>{#each agentChoices as choice (choice.id)}<option value={choice.id}>{choice.label}</option>{/each}</select>
    </label>
    <label class="folder">In folder
      <span class="field-box">
        <input type="text" bind:value={launchFolder} placeholder="C:\Users\you\project" autocomplete="off" spellcheck="false" aria-invalid={folderNote !== null} disabled={!available || launching} />
        <BrowseButton start={launchFolder} disabled={!available || launching} label="Browse for the agent folder" onPick={folder => { launchFolder = folder; notice = null; }} onError={message => { notice = message; }} />
      </span>
    </label>
    <button type="submit" disabled={!available || launching || folderProblem(launchFolder) !== null}>{launching ? 'Starting…' : 'Launch'}</button>
    {#if folderNote}<span class="launch-note" role="status">{folderNote}</span>{/if}
  </form>
  <p class="terminal-hint small">Starts a PowerShell terminal in that folder and types the agent's start command for you, so you can see exactly what ran. It keeps running if you close Workbench, and you get a normal shell back when the agent exits.</p>

  {#if lostSessions.length > 0}
    <div class="previous" role="group" aria-label="Terminals that did not survive a restart">
      <span><strong>{lostSessions.length} {lostSessions.length === 1 ? 'terminal' : 'terminals'}</strong> did not survive the last restart (the programs in {lostSessions.length === 1 ? 'it' : 'them'} ended). They can be recreated in their folders; agents are resumed.</span>
      <button type="button" onclick={() => void restoreAll()} disabled={restoring}>{restoring ? 'Restoring…' : 'Restore all'}</button>
    </div>
  {/if}

  <div class="terminal-body">
    <div class="bar">
      <ul class="terminal-list" aria-label="Terminal sessions">
        {#each sessions as s (s.id)}
          {@const leaf = leafOf(panes, s.id)}
          <li class:active={s.id === focusedId} draggable={s.state !== 'lost'} ondragstart={event => onDragStart(s.id, event)}>
            <button type="button" class="pick" title="Click to show in the highlighted pane, or drag onto a pane" disabled={s.state === 'lost'} onclick={() => show(s)} aria-current={s.id === focusedId ? 'true' : undefined}>
              <strong>{s.label}</strong>
              <small>{s.state} · {s.cols}×{s.rows}{leaf !== null ? ` · pane ${numberOf(leaf)}` : ''}</small>
            </button>
            {#if agentStore.entryOf(s.id)}<span class="badge agent-tag" title="Launched by Workbench; its transcript is linked once it exists.">{agentLabel(agentStore.entryOf(s.id)!.agent)}{agentStore.linkOf(s.id)?.state === 'linked' ? ' · linked' : ''}</span>{/if}
            {#if (s.history_mib ?? 1) > 1}<span class="badge" title="Keeps up to {s.history_mib} MiB of output in memory (never on disk).">{s.history_mib} MiB history</span>{/if}
            {#if logStore.on(s.id)}<span class="badge rec" title={`Output is being saved to ${logStore.of(s.id)?.path ?? 'a log file'}`}>REC</span>{/if}
            <span class="badge" title="Runs as its own background process. Keeps running if you close Workbench, if Workbench's background service restarts, or if it is updated.">{s.persistent ? 'Persistent' : 'In-process'}</span>
            {#if s.state === 'lost'}<button type="button" class="restore" title="Recreate this terminal in its folder{agentStore.entryOf(s.id) ? ' and resume its agent' : ''}" disabled={restoring} onclick={() => void restoreSingle(s.id)}>Restore</button>{/if}
            <button type="button" class="float-out" title="Open this terminal in a free-form window on the Canvas" disabled={s.state === 'lost'} onclick={() => floatOut(s.id)}>{isFloating(canvas.state, s.id) ? 'Floating' : 'Float'}</button>
            <button type="button" class="end" onclick={() => void end(s.id)}>{confirmEnd === s.id ? 'Confirm end' : 'End session'}</button>
          </li>
        {/each}
        {#if sessions.length === 0}<li class="empty">No terminals yet.</li>{/if}
      </ul>
      <div class="layouts" role="group" aria-label="Pane layout">
        <button type="button" onclick={tileAll} disabled={sessions.length === 0} title="Show every terminal, arranged in a grid (tmux: tiled)">Tile all ({Math.min(sessions.filter(s => s.state !== 'lost').length, MAX_LEAVES)})</button>
        <button type="button" onclick={singlePane} title="Keep only the focused pane (terminals keep running)">Single pane</button>
        <select class="layout-pick" aria-label="Arrange the panes" title={`Arrange the terminals in the panes (tmux layouts; ${prefixName} then Space cycles them)`}
          value="" onchange={event => { const v = event.currentTarget.value as Preset; event.currentTarget.value = ''; if (PRESETS.includes(v)) applyLayout(v); }}>
          <option value="" disabled>Layout…</option>
          {#each PRESETS as p (p)}<option value={p}>{presetLabel[p]}</option>{/each}
        </select>
        <button type="button" class:on={synced} aria-pressed={synced} onclick={toggleSync} disabled={order.filter(l => l.session !== null).length < 2}
          title={`Type into every pane shown here at once (${prefixName} then S)`}>Type in all</button>
        <span class="pane-count">{order.length} {order.length === 1 ? 'pane' : 'panes'}{zoomed !== null ? ' · zoomed' : ''}</span>
      </div>
    </div>

    <div class="panes" class:dragging={dragging !== null} bind:this={paneArea}>
      <!-- One keyed entry per pane, positioned from the split tree: a pane keeps its terminal while others split or close. -->
      {#each order as leaf (leaf.id)}
        {@const s = leaf.session === null ? undefined : sessions.find(x => x.id === leaf.session)}
        {@const r = paneRects.get(leaf.id)}
        {#if r}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="pane" class:focused={panes.focused === leaf.id} class:drop={dropTarget === leaf.id} class:synced={s !== undefined && broadcast.includes(s.id)} data-leaf={leaf.id}
          style={`left:${pct(r.x)};top:${pct(r.y)};width:${pct(r.w)};height:${pct(r.h)};`}
          ondragover={event => { event.preventDefault(); dropTarget = leaf.id; }} ondragleave={() => { if (dropTarget === leaf.id) dropTarget = null; }} ondrop={event => onDrop(leaf.id, event)}>
          <div class="pane-head">
            <span>Pane {numberOf(leaf.id)}{s ? ` · ${s.label}` : ''}{#if zoomed === leaf.id}<strong class="mode"> · ZOOMED</strong>{/if}{#if s && broadcast.includes(s.id)}<strong class="mode sync"> · TYPING INTO ALL PANES</strong>{:else if synced && s && zoomed !== null}<strong class="mode sync"> · TYPING HERE ONLY WHILE ZOOMED</strong>{/if}</span>
            <span class="pane-actions">
              {#if s}<button type="button" class:rec-on={logStore.on(s.id)} aria-pressed={logStore.on(s.id)} aria-label={`${logStore.on(s.id) ? 'Stop logging' : 'Log the output of'} ${s.label} to a file`}
                title={logStore.on(s.id) ? `Logging to ${logStore.of(s.id)?.path ?? 'a file'}: click to stop (${prefixName} then L)` : `Save everything this terminal prints to a log file (${prefixName} then L)`} onclick={() => void toggleLog(s.id)}>●</button>{/if}
              {#if order.length > 1}<button type="button" aria-label={zoomed === leaf.id ? `Show all panes again` : `Zoom pane ${numberOf(leaf.id)} to the whole view`} aria-pressed={zoomed === leaf.id}
                title={zoomed === leaf.id ? `Show all panes (${prefixName} then z)` : `Zoom this pane to the whole view (${prefixName} then z)`} onclick={() => { update(focus(panes, leaf.id)); zoomed = zoomed === leaf.id ? null : leaf.id; }}>⛶</button>{/if}
              <button type="button" aria-label={`Split pane ${numberOf(leaf.id)} to the right`} title={`Split right (${prefixName} then %)`} onclick={() => splitPane(leaf.id, 'row')}>▥</button>
              <button type="button" aria-label={`Split pane ${numberOf(leaf.id)} downwards`} title={`Split down (${prefixName} then ")`} onclick={() => splitPane(leaf.id, 'column')}>▤</button>
              {#if s}<button type="button" aria-label={`Float terminal ${s.label} on the Canvas`} title="Move this terminal to a free-form window on the Canvas" onclick={() => floatOut(s.id)}>⧉</button>{/if}
              <button type="button" aria-label={`Close pane ${numberOf(leaf.id)}`} title={order.length > 1 ? 'Close this pane (the terminal keeps running)' : 'Stop showing this terminal here (it keeps running)'} onclick={() => closePane(leaf.id)}>×</button>
            </span>
          </div>
          <div class="pane-body">
            {#if s}
              <TerminalBody session={s} focused={panes.focused === leaf.id} onFocus={() => update(focus(panes, leaf.id))} onError={fail} onNotice={text => { notice = text; }} />
            {:else}
              <button type="button" class="empty-pane" onclick={() => update(focus(panes, leaf.id))}>Empty pane. Pick a terminal above, or create one.</button>
            {/if}
          </div>
          {#if s?.state === 'unresponsive'}<div class="terminal-note error" role="status">This terminal's host is not responding. Other terminals are unaffected; it reconnects automatically if the host recovers.</div>{/if}
          {#if s?.state === 'exited'}<div class="terminal-note" role="status">The shell in this terminal has exited. Its output is kept; use End session to remove it.</div>{/if}
        </div>
        {/if}
      {/each}
      {#each regions as region (region.split.id)}
        {@const node = region.split}
        {@const at = region.at}
        <!-- A focusable separator is the WAI-ARIA window-splitter pattern (keyboard and pointer resizable).
             Each divider stops half a divider-width short of its region's ends, so at a T-junction the
             vertical divider stays grabbable instead of being covered by the horizontal one. -->
        <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
        <div class="divider {node.dir}" role="separator" aria-orientation={node.dir === 'row' ? 'vertical' : 'horizontal'}
          aria-label="Resize panes; arrow keys nudge, double-click resets" aria-valuemin="15" aria-valuemax="85" aria-valuenow={Math.round(node.ratio * 100)} tabindex="0"
          style={node.dir === 'row'
            ? `left:calc(${pct(at.x + at.w * node.ratio)} - ${DIVIDER_PX / 2}px);top:calc(${pct(at.y)} + ${DIVIDER_PX / 2}px);height:calc(${pct(at.h)} - ${DIVIDER_PX}px);width:${DIVIDER_PX}px;`
            : `top:calc(${pct(at.y + at.h * node.ratio)} - ${DIVIDER_PX / 2}px);left:calc(${pct(at.x)} + ${DIVIDER_PX / 2}px);width:calc(${pct(at.w)} - ${DIVIDER_PX}px);height:${DIVIDER_PX}px;`}
          onpointerdown={event => dragStart(node, event)} onpointermove={event => dragMove(node, at, event)} onpointerup={dragEnd} onpointercancel={dragEnd}
          onkeydown={event => nudge(node, event)} ondblclick={() => update(setRatio(panes, node.id, 0.5))}></div>
      {/each}
    </div>

    {#if focusedSession}
      <details class="access" open={draft !== null}>
        <summary>Agent access for {focusedSession.label}{draft ? ' · 1 proposal waiting' : ''}{grants.length ? ` · ${grants.length} grant${grants.length === 1 ? '' : 's'}` : ''}</summary>
        <p class="access-hint">Agents can propose text here, but nothing is typed unless you accept it, and an agent can read or type only while you have allowed it. Allowing needs your confirmation phrase.</p>
        {#if draft}
          <div class="draft" role="status">
            <strong>{draft.agent} proposes:</strong>
            <pre>{draft.text}</pre>
            {#if draft.multiline}<small>Several lines: they are pasted, not run, until you press Enter.</small>{/if}
            <div class="row"><button type="button" onclick={() => void acceptDraft()}>Type it here (I press Enter)</button><button type="button" onclick={() => void discardDraft()}>Discard</button></div>
          </div>
        {/if}
        {#each grants as g (g.agent)}
          <div class="grant"><span><strong>{g.agent}</strong> may read and type here · {fmtRemaining(g.remaining_ms)} left{g.keep_after_detach ? ' · stays after I detach' : ''}</span><button type="button" onclick={() => void revoke(g.agent)}>Revoke</button></div>
        {/each}
        <form class="allow" onsubmit={event => { event.preventDefault(); void allow(); }}>
          <label>Agent name<input type="text" bind:value={agentName} maxlength="64" placeholder="e.g. claude" autocomplete="off" /></label>
          <label>For<select bind:value={duration}>{#each grantDurations as d (d.id)}<option value={d.id}>{d.label}</option>{/each}</select></label>
          <label>Confirmation phrase<input type="password" value={phrase} oninput={event => { phrase = filterPhrase((event.currentTarget as HTMLInputElement).value); }} autocomplete="off" placeholder="empty is fine if none is set" /></label>
          <label class="keep"><input type="checkbox" bind:checked={keepAfterDetach} /> Stay allowed after I detach</label>
          <button type="submit" disabled={!agentOk}>Allow</button>
        </form>
        <details class="gate"><summary>Change the confirmation phrase</summary>
          <form class="allow" onsubmit={event => { event.preventDefault(); void changePhrase(); }}>
            <label>Current<input type="password" value={phraseCurrent} oninput={event => { phraseCurrent = filterPhrase((event.currentTarget as HTMLInputElement).value); }} autocomplete="off" /></label>
            <label>New (letters, digits, ! @ # $ % ^ & * ( ), empty allowed)<input type="password" value={phraseNew} oninput={event => { phraseNew = filterPhrase((event.currentTarget as HTMLInputElement).value); }} autocomplete="off" /></label>
            <button type="submit">Change</button>
          </form>
        </details>
        {#if accessNote}<p class="access-hint" role="status">{accessNote}</p>{/if}
      </details>
    {/if}
  </div>
</section>

<style>
  .terminals { position: absolute; inset: 0; z-index: 11; display: flex; flex-direction: column; padding: 18px 22px 12px; background: var(--view-bg, var(--bg)); color: var(--text); overflow: hidden; }
  .terminal-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .terminal-hint { margin: 8px 0 10px; color: var(--muted); font-size: 12px; }
  .new-terminal { display: flex; align-items: center; gap: 12px; }
  .extended { display: flex; align-items: center; justify-content: flex-start; gap: 6px; margin: 0; font-size: 11px; white-space: nowrap; }
  /* The app's global input rule (full width, padding, top margin) must not apply to checkboxes. */
  .extended input[type=checkbox], .allow .keep input[type=checkbox] { width: 16px; height: 16px; padding: 0; margin: 0; flex: none; display: inline-block; }
  .terminal-hint.small { font-size: 11px; margin: 4px 0 10px; }
  .previous { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 8px 10px; margin-bottom: 8px; border: 1px solid #8b7042; background: var(--surface); font-size: 12px; }
  .previous button { flex: none; }
  li button.restore { border-color: #8b7042; }
  .terminal-note { padding: 7px 10px; margin-bottom: 8px; border: 1px solid var(--line); background: var(--surface); font-size: 12px; }
  .terminal-note.error { border-inline-start: 3px solid #dc8976; }
  .terminal-body { flex: 1; min-height: 0; display: flex; flex-direction: column; gap: 8px; }
  .bar { display: flex; gap: 12px; align-items: flex-start; justify-content: space-between; flex-shrink: 0; }
  .terminal-list { list-style: none; margin: 0; padding: 0 0 4px; display: flex; gap: 8px; overflow-x: auto; min-width: 0; }
  .terminal-list li { border: 1px solid var(--line); padding: 5px 6px; display: flex; align-items: center; gap: 8px; flex-shrink: 0; }
  .terminal-list li.active { border-color: #8abaff; box-shadow: inset 2px 0 #8abaff; }
  .terminal-list li.empty { color: var(--muted); border-style: dashed; }
  .terminal-list .badge { font-size: 8px; }
  .pick { text-align: left; }
  .pick strong, .pick small { display: block; white-space: nowrap; }
  .pick small { color: var(--muted); font-size: 10px; margin-top: 3px; }
  .end, .float-out { font-size: 11px; }
  .launch { display: flex; flex-wrap: wrap; align-items: flex-end; gap: 10px; margin: 6px 0 2px; }
  .launch label { margin: 0; font-size: 11px; }
  .launch label.folder { flex: 1 1 280px; }
  .launch select, .launch input { margin-top: 3px; padding: 5px 6px; }
  .launch .field-box { margin-top: 3px; }
  .launch button { font-size: 12px; }
  .launch-note { flex: 1 1 100%; color: #e5bc6e; font-size: 11px; }
  .badge.agent-tag { color: #8abaff; border-color: #4a6b99; }
  .layouts { display: flex; gap: 4px; flex-shrink: 0; align-items: center; }
  .layouts button { font-size: 11px; }
  .pane-count { font: 10px ui-monospace, monospace; color: var(--muted); padding: 0 6px; white-space: nowrap; }
  .layouts button.on { border-color: #e5bc6e; box-shadow: inset 0 -2px #e5bc6e; }
  .layout-pick { width: auto; margin: 0; padding: 3px 4px; font-size: 11px; }
  .pane.synced { border-color: #e5bc6e; }
  .pane-head .mode { color: #8abaff; font-weight: 700; }
  .pane-head .mode.sync { color: #e5bc6e; }
  .pane-head button.rec-on { color: #e2685a; }
  .badge.rec { color: #fff; background: #b8473a; border-color: #b8473a; }
  .panes { flex: 1; min-height: 120px; position: relative; }
  .panes.dragging { user-select: none; }
  .divider { position: absolute; background: var(--line); touch-action: none; z-index: 2; }
  .divider:hover, .divider:focus-visible, .panes.dragging .divider { background: #8abaff; }
  .divider.row { cursor: col-resize; }
  .divider.column { cursor: row-resize; }
  .pane { position: absolute; box-sizing: border-box; min-width: 0; min-height: 0; display: flex; flex-direction: column; border: 1px solid var(--line); background: #0b121a; overflow: hidden; }
  .pane.focused { border-color: #8abaff; }
  .pane.drop { outline: 2px dashed #8abaff; outline-offset: -2px; }
  .terminal-list li[draggable=true] { cursor: grab; }
  .pane-head { display: flex; align-items: center; justify-content: space-between; gap: 6px; padding: 2px 6px; border-bottom: 1px solid var(--line); background: var(--surface); font: 10px ui-monospace, monospace; color: var(--muted); white-space: nowrap; overflow: hidden; }
  .pane.focused .pane-head { color: var(--text); }
  .pane-actions { display: flex; flex: none; }
  .pane-head button { border: 0; background: transparent; padding: 0 6px; font-size: 13px; line-height: 1.2; }
  .pane-body { flex: 1; min-height: 0; }
  .empty-pane { width: 100%; height: 100%; border: 0; background: transparent; color: var(--muted); font-size: 12px; }
  .access { border: 1px solid var(--line); padding: 6px 10px; background: var(--surface); max-height: 38%; overflow: auto; flex-shrink: 0; }
  .access summary { cursor: pointer; font-size: 12px; }
  .access-hint { margin: 6px 0; color: var(--muted); font-size: 11px; }
  .draft { border: 1px solid #8abaff; padding: 6px 8px; margin: 6px 0; }
  .draft pre { margin: 4px 0; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; }
  .draft .row, .grant { display: flex; gap: 8px; align-items: center; justify-content: space-between; flex-wrap: wrap; }
  .grant { border: 1px solid var(--line); padding: 4px 8px; margin: 4px 0; font-size: 12px; }
  .allow { display: flex; flex-wrap: wrap; gap: 8px; align-items: flex-end; margin: 6px 0; }
  .allow label { margin: 0; font-size: 11px; flex: 1 1 140px; }
  .allow input[type=text], .allow input[type=password], .allow select { margin-top: 3px; padding: 5px 6px; }
  .allow .keep { flex: 1 1 100%; display: flex; align-items: center; gap: 6px; }
  .gate { margin-top: 4px; font-size: 12px; }
</style>
