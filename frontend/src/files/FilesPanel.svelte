<script lang="ts">
  // The Files panel is a lazy, read-only tree of one root folder. Right-click offers folder/file actions; nothing is opened, run, renamed or deleted here.
  import { onDestroy, onMount, untrack } from 'svelte';
  import { invoke, isTauri } from '@tauri-apps/api/core';
  import ContextMenu, { type MenuItem } from '../ContextMenu.svelte';
  import { baseName, createFilesClient, isFolderPath, isPinned, joinPath, maxPins, parentPath, parsePins, quotedPath, sizeText, togglePin, type Entry, type Listing, type Pin } from './files.js';
  import { createFollowPolicy, sameFolder, type FocusSnapshot, type Update } from './follow.js';
  import { settingsStore } from '../settings/settings-store.svelte.js';
  import { lastFocused } from '../terminals/last-focused.svelte.js';
  import { agentStore } from '../terminals/agent-store.svelte.js';
  import { terminalClient } from '../terminals/tauri.js';
  import { terminalRequests } from '../terminals/requests.svelte.js';

  interface Props { onShowTerminals: () => void; }
  let { onShowTerminals }: Props = $props();
  const client = createFilesClient((command, args) => isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'Files are listed in the desktop app.' }));
  const KEY = 'aw.files', PINS_KEY = 'aw.files.pins', MAX_REMEMBERED = 200;
  const INELIGIBLE_NOTICE = 'Follow is on, but the focused terminal has no valid recorded launch folder. Keeping this folder.';
  const NO_FOCUS_NOTICE = 'Follow is on. Click in an agent terminal to show its recorded launch folder.';
  type Loaded = { state: 'loading' } | { state: 'ready'; listing: Listing } | { state: 'failed'; message: string };

  function remembered(): { root: string | null; expanded: string[]; hidden: boolean; following: unknown } {
    try {
      const raw: unknown = JSON.parse(localStorage.getItem(KEY) ?? 'null');
      if (typeof raw !== 'object' || raw === null) return { root: null, expanded: [], hidden: false, following: false };
      const r = raw as Record<string, unknown>;
      return {
        root: typeof r.root === 'string' ? r.root : null,
        expanded: Array.isArray(r.expanded) ? r.expanded.filter((p): p is string => typeof p === 'string').slice(0, MAX_REMEMBERED) : [],
        hidden: r.hidden === true, following: r.following
      };
    } catch { return { root: null, expanded: [], hidden: false, following: false }; }
  }
  const restored = remembered();
  const followPolicy = createFollowPolicy({ root: restored.root, following: restored.following });
  let root = $state<string | null>(followPolicy.state().root);
  let following = $state(followPolicy.state().following);
  let showHidden = $state(false);
  let expanded = $state.raw<ReadonlySet<string>>(new Set());
  let folders = $state.raw<ReadonlyMap<string, Loaded>>(new Map());
  let notice = $state<string | null>(null);
  let menu = $state<{ x: number; y: number; items: MenuItem[]; label: string } | null>(null);
  let mounted = $state(false), requestSequence = 0;
  const requestIds = new Map<string, number>();
  let lastIneligibleFocusKey: string | null = null;

  function loadPins(): Pin[] {
    try { return parsePins(JSON.parse(localStorage.getItem(PINS_KEY) ?? '[]')); } catch { return []; }
  }
  let pins = $state.raw<Pin[]>(loadPins());
  let pinsOpen = $state.raw<ReadonlySet<string>>(new Set());
  function remember(): void {
    try { localStorage.setItem(KEY, JSON.stringify({ root, expanded: [...expanded].slice(-MAX_REMEMBERED), hidden: showHidden, following })); } catch { /* not remembered */ }
  }
  function pin(path: string, folder: boolean): void {
    const was = isPinned(pins, path), next = togglePin(pins, { path, folder });
    if (!was && next.length === pins.length) { notice = `At most ${maxPins} pins; unpin one first.`; return; }
    pins = next;
    try { localStorage.setItem(PINS_KEY, JSON.stringify(pins)); } catch { /* pins last for this run only */ }
  }
  function focusSnapshot(): FocusSnapshot | null {
    const terminalId = lastFocused.id;
    return terminalId === null ? null : { terminalId, recordedFolder: agentStore.entryOf(terminalId)?.cwd };
  }
  function focusKey(focus: FocusSnapshot | null): string {
    return focus === null ? '<no-focused-terminal>' : `${focus.terminalId}\u0000${focus.recordedFolder ?? ''}`;
  }
  function announceIneligible(focus: FocusSnapshot | null): void {
    const key = focusKey(focus);
    if (lastIneligibleFocusKey !== key) notice = focus === null ? NO_FOCUS_NOTICE : root === null ? INELIGIBLE_NOTICE.replace(' Keeping this folder.', '') : INELIGIBLE_NOTICE;
    lastIneligibleFocusKey = key;
  }
  function followedNotice(path: string | null): string {
    return `Showing the focused agent’s recorded launch folder: ${path}.`;
  }
  function folderKey(path: string): string {
    const lower = path.toLowerCase().replace(/\\+$/, '');
    return /^[a-z]:$/.test(lower) ? `${lower}\\` : lower;
  }
  function loadedFor(path: string): Loaded | undefined {
    for (const [candidate, loaded] of folders) if (sameFolder(candidate, path)) return loaded;
    return undefined;
  }
  function storeLoaded(path: string, loaded: Loaded): void {
    const next = new Map(folders);
    for (const candidate of next.keys()) if (sameFolder(candidate, path)) next.delete(candidate);
    next.set(path, loaded);
    folders = next;
  }
  async function load(path: string, token = followPolicy.captureContentToken(), force = false): Promise<void> {
    if (!mounted || !isFolderPath(path) || !followPolicy.isContentCurrent(token)) return;
    const cached = loadedFor(path);
    if (!force && cached && cached.state !== 'failed') return;
    const key = folderKey(path), requestId = ++requestSequence;
    requestIds.set(key, requestId);
    storeLoaded(path, { state: 'loading' });
    let next: Loaded;
    try { next = { state: 'ready', listing: await client.list(path, showHidden) }; }
    catch (e) { next = { state: 'failed', message: (e as { message?: string }).message ?? 'That folder cannot be read.' }; }
    if (!mounted || !followPolicy.isContentCurrent(token) || requestIds.get(key) !== requestId) return;
    storeLoaded(path, next);
  }
  function loadVisible(): void {
    const token = followPolicy.captureContentToken(), paths: string[] = [];
    const add = (path: string) => { if (isFolderPath(path) && !paths.some(p => sameFolder(p, path))) paths.push(path); };
    if (root !== null) add(root);
    for (const path of expanded) add(path);
    for (const path of pinsOpen) add(path);
    folders = new Map();
    for (const path of paths) void load(path, token, true);
  }
  function applyUpdate(update: Update, keepExpanded = false): void {
    untrack(() => {
      root = update.root; following = update.following;
      if (update.changed && !keepExpanded) expanded = new Set();
      remember();
      if (update.changed) loadVisible();
    });
  }
  function observeFocus(focus: FocusSnapshot | null): Update {
    const update = followPolicy.focusChanged(focus);
    if (update.reason === 'ineligible-focus') announceIneligible(focus);
    else {
      const wasIneligible = lastIneligibleFocusKey !== null;
      lastIneligibleFocusKey = null;
      if (update.changed) { applyUpdate(update); notice = followedNotice(update.root); }
      else if (wasIneligible) notice = null;
    }
    return update;
  }
  function changeFollowing(enabled: boolean): void {
    const focus = focusSnapshot(), update = followPolicy.setFollowing(enabled, focus);
    applyUpdate(update);
    if (update.reason === 'follow-disabled') {
      lastIneligibleFocusKey = null; notice = 'Follow paused. This folder will stay selected.';
    } else if (update.reason === 'follow-enabled-navigation' || update.reason === 'follow-enabled') {
      if (focus && isFolderPath(focus.recordedFolder)) {
        lastIneligibleFocusKey = null; notice = followedNotice(update.root);
      } else announceIneligible(focus);
    }
  }
  function refresh(): void { applyUpdate(followPolicy.refresh(), true); }
  function toggleHidden(): void { showHidden = !showHidden; refresh(); }
  function up(): void {
    const update = followPolicy.manualUp();
    applyUpdate(update); lastIneligibleFocusKey = null;
    notice = update.changed ? `Follow paused. Showing parent folder: ${update.root}.` : 'Follow paused. There is no parent folder to show.';
  }
  function showFolder(path: string): void {
    const update = followPolicy.manualChoose(path);
    applyUpdate(update); lastIneligibleFocusKey = null;
    notice = update.reason === 'manual-choice-invalid'
      ? 'Follow paused. That folder could not be shown.'
      : `Follow paused. Showing folder: ${update.root}.`;
  }
  function toggle(path: string): void {
    const next = new Set(expanded);
    if (next.has(path)) next.delete(path);
    else { next.add(path); void load(path); }
    expanded = next; remember();
  }
  function togglePinOpen(path: string): void {
    const next = new Set(pinsOpen);
    if (next.has(path)) next.delete(path);
    else { next.add(path); void load(path); }
    pinsOpen = next;
  }
  async function initialize(token: ReturnType<typeof followPolicy.captureIntentToken>): Promise<void> {
    let start: string | null = null;
    const fromSettings = untrack(() => settingsStore.value.terminals.start_folder);
    if (isFolderPath(fromSettings)) start = fromSettings;
    else {
      try { start = await client.home(); }
      catch (e) {
        if (mounted && followPolicy.isIntentCurrent('startup', token)) notice = (e as { message?: string }).message ?? 'Choose a folder to show.';
        return;
      }
    }
    if (!mounted || !followPolicy.isIntentCurrent('startup', token)) return;
    if (!isFolderPath(start)) { notice = 'Choose a folder to show.'; return; }
    applyUpdate(followPolicy.setInitialRoot(start), true);
  }
  async function pickRoot(): Promise<void> {
    applyUpdate(followPolicy.pause());
    lastIneligibleFocusKey = null; notice = 'Follow paused while choosing a folder.';
    const token = followPolicy.captureIntentToken('picker');
    try {
      const chosen = await settingsStore.pickFolder(root ?? '');
      if (!mounted || !followPolicy.isIntentCurrent('picker', token)) return;
      if (chosen === null) { notice = root === null ? null : 'Follow paused. Keeping this folder.'; return; }
      const update = followPolicy.manualChoose(chosen);
      applyUpdate(update);
      notice = update.reason === 'manual-choice-invalid'
        ? 'Follow paused. That folder could not be shown.'
        : `Follow paused. Showing folder: ${update.root}.`;
    } catch (e) {
      if (mounted && followPolicy.isIntentCurrent('picker', token)) notice = (e as { message?: string }).message ?? 'The folder picker is unavailable.';
    }
  }
  async function insert(path: string): Promise<void> {
    const text = quotedPath(path), target = lastFocused.id;
    if (text === null) { notice = 'That path cannot be typed.'; return; }
    if (target === null) { notice = 'Click in a terminal first, then Insert path types into it.'; return; }
    try {
      await terminalClient.input(target, text);
      if (mounted) notice = `Typed the path into ${lastFocused.label || 'the terminal'} (not sent; press Enter yourself).`;
    } catch (e) { if (mounted) notice = (e as { message?: string }).message ?? 'The path could not be typed.'; }
  }
  async function copy(path: string): Promise<void> {
    try { await navigator.clipboard.writeText(path); if (mounted) notice = 'Path copied.'; }
    catch { if (mounted) notice = 'The clipboard is unavailable.'; }
  }
  async function reveal(path: string): Promise<void> {
    try { await client.reveal(path); if (mounted) notice = null; }
    catch (e) { if (mounted) notice = (e as { message?: string }).message ?? 'File Explorer could not be opened.'; }
  }
  function itemsFor(path: string, folder: boolean): MenuItem[] {
    return [
      { label: isPinned(pins, path) ? 'Unpin' : 'Pin to top', onselect: () => pin(path, folder) },
      ...(folder ? [{ label: 'Show this folder', onselect: () => showFolder(path) }] : []),
      { label: 'Open terminal here', disabled: !folder, hint: folder ? undefined : 'Folders only', onselect: () => { terminalRequests.openHere = path; onShowTerminals(); } },
      { label: 'Launch agent here', disabled: !folder, hint: folder ? undefined : 'Folders only', onselect: () => { terminalRequests.launchHere = path; onShowTerminals(); } },
      { label: 'Insert path', hint: 'Types the quoted path into the last terminal you used, without Enter', onselect: () => void insert(path) },
      { label: 'Copy path', onselect: () => void copy(path) },
      { label: 'Reveal in File Explorer', onselect: () => void reveal(path) }
    ];
  }
  function openMenu(event: MouseEvent | KeyboardEvent, path: string, folder: boolean, name: string): void {
    event.preventDefault();
    const at = event instanceof MouseEvent && event.clientX > 0 ? { x: event.clientX, y: event.clientY } : (() => {
      const r = (event.currentTarget as HTMLElement).getBoundingClientRect(); return { x: r.left + 16, y: r.bottom };
    })();
    menu = { ...at, items: itemsFor(path, folder), label: `Actions for ${name}` };
  }
  function rowKeys(event: KeyboardEvent, path: string, folder: boolean, name: string): void {
    if (event.key === 'ContextMenu' || (event.key === 'F10' && event.shiftKey)) openMenu(event, path, folder, name);
  }
  type Row = { kind: 'entry'; path: string; entry: Entry; depth: number } | { kind: 'note'; text: string; depth: number; failed?: boolean };
  function walkInto(out: Row[], path: string, depth: number): void {
    const walk = (current: string, level: number) => {
      const loaded = loadedFor(current);
      if (!loaded || loaded.state === 'loading') { out.push({ kind: 'note', text: 'Reading…', depth: level }); return; }
      if (loaded.state === 'failed') { out.push({ kind: 'note', text: loaded.message, depth: level, failed: true }); return; }
      const { entries, more, hidden } = loaded.listing;
      if (entries.length === 0) out.push({ kind: 'note', text: hidden > 0 ? `Empty (${hidden} hidden)` : 'Empty', depth: level });
      for (const entry of entries) {
        const child = joinPath(current, entry.name);
        out.push({ kind: 'entry', path: child, entry, depth: level });
        if (entry.kind === 'folder' && expanded.has(child)) walk(child, level + 1);
        if (out.length > 5000) return;
      }
      if (more > 0) out.push({ kind: 'note', text: `${more} more not shown`, depth: level });
    };
    walk(path, depth);
  }
  const rows = $derived.by(() => {
    const out: Row[] = [];
    if (root !== null) walkInto(out, root, 0);
    return out;
  });
  const pinRows = $derived.by(() => pins.map(p => {
    const children: Row[] = [];
    if (p.folder && pinsOpen.has(p.path)) walkInto(children, p.path, 1);
    return { pin: p, children };
  }));

  onMount(() => {
    mounted = true;
    const startupToken = followPolicy.captureIntentToken('startup');
    untrack(() => { showHidden = restored.hidden; expanded = new Set(restored.expanded); remember(); });
    const update = untrack(() => observeFocus(focusSnapshot()));
    if (followPolicy.state().root !== null) {
      if (!update.changed) untrack(() => loadVisible());
      return;
    }
    void initialize(startupToken);
  });
  $effect(() => {
    if (!mounted) return;
    const focus = focusSnapshot();
    untrack(() => observeFocus(focus));
  });
  onDestroy(() => { mounted = false; followPolicy.dispose(); });
</script>

<section class="files" aria-label="Files">
  <div class="head">
    <span class="root" title={root ?? ''}>{root ?? 'No folder'}</span>
    <div class="tools">
      <button type="button" aria-label="Up one folder" title="Up one folder" disabled={root === null || parentPath(root) === null} onclick={up}>↑</button>
      <button type="button" aria-label="Choose the folder to show" title="Choose the folder to show" onclick={() => void pickRoot()}>…</button>
      <button type="button" aria-label="Refresh the files" title="Refresh" disabled={root === null} onclick={refresh}>↻</button>
      <button type="button" class:on={showHidden} aria-label="Show hidden files" aria-pressed={showHidden} title={showHidden ? 'Hidden files are shown (click to hide them)' : 'Show hidden files'} onclick={toggleHidden}><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8 12.1 12.5 8 12.5 1.5 8 1.5 8z" /><circle cx="8" cy="8" r="2" />{#if !showHidden}<path d="M2.5 13.5l11-11" />{/if}</svg></button>
    </div>
  </div>
  <label class="follow">
    <input type="checkbox" checked={following} aria-describedby="files-follow-help" onchange={event => changeFollowing(event.currentTarget.checked)} />
    <span>Follow focused agent’s recorded launch folder</span>
  </label>
  <p class="follow-help" id="files-follow-help">Uses the recorded launch folder, not live shell CWD.</p>
  {#if notice}<p class="notice" role="status">{notice}</p>{/if}
  {#if pins.length > 0}
    <ul class="pins" aria-label="Pinned files and folders">
      {#each pinRows as { pin: p, children } (p.path)}
        <li>
          <button type="button" class="row" aria-expanded={p.folder ? pinsOpen.has(p.path) : undefined} title={p.folder ? `${p.path} (click to open or close; right-click for more)` : `${p.path} (click for actions)`}
            onclick={event => { if (p.folder) togglePinOpen(p.path); else openMenu(event, p.path, false, baseName(p.path)); }}
            oncontextmenu={event => openMenu(event, p.path, p.folder, baseName(p.path))}
            onkeydown={event => rowKeys(event, p.path, p.folder, baseName(p.path))}>
            <span class="twisty" aria-hidden="true">{p.folder ? (pinsOpen.has(p.path) ? '▾' : '▸') : '·'}</span><span class="name">{baseName(p.path)}</span><small>pinned</small>
          </button>
          {#if children.length > 0}<ul class="tree" role="group">{#each children as row, i (row.kind === 'entry' ? row.path : `note-${i}`)}{@render treeRow(row)}{/each}</ul>{/if}
        </li>
      {/each}
    </ul>
  {/if}
  <ul class="tree" role="tree" aria-label="Files in the chosen folder">
    {#each rows as row, i (row.kind === 'entry' ? row.path : `note-${i}`)}{@render treeRow(row)}{/each}
  </ul>
  <p class="hint">Right-click a file or folder (or press the menu key) for actions. Read-only: nothing is opened, run or changed from here.</p>
</section>
{#snippet treeRow(row: Row)}
  {#if row.kind === 'entry'}
    {@const folder = row.entry.kind === 'folder'}
    <li role="treeitem" aria-selected="false" aria-expanded={folder ? expanded.has(row.path) : undefined} style={`--depth:${row.depth}`}>
      <button type="button" class="row" class:link={row.entry.kind === 'link'} title={row.path}
        onclick={() => { if (folder) toggle(row.path); }}
        oncontextmenu={event => openMenu(event, row.path, folder, row.entry.name)}
        onkeydown={event => rowKeys(event, row.path, folder, row.entry.name)}>
        <span class="twisty" aria-hidden="true">{folder ? (expanded.has(row.path) ? '▾' : '▸') : ''}</span>
        <span class="name">{row.entry.name}</span>
        {#if isPinned(pins, row.path)}<span class="pin-mark" aria-label="pinned" title="Pinned">◆</span>{/if}
        {#if row.entry.kind === 'file'}<small>{sizeText(row.entry.size)}</small>{:else if row.entry.kind === 'link'}<small>link</small>{/if}
      </button>
    </li>
  {:else}
    <li class="note" class:failed={row.failed} style={`--depth:${row.depth}`}>{row.text}</li>
  {/if}
{/snippet}
{#if menu}<ContextMenu x={menu.x} y={menu.y} items={menu.items} label={menu.label} onClose={() => { menu = null; }} />{/if}

<style>
  .files { display: flex; flex-direction: column; min-height: 0; }
  .head { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; }
  .root { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left; font: 11px ui-monospace, monospace; color: var(--muted); }
  .tools { display: flex; gap: 3px; flex: none; }
  .tools button { width: 26px; height: 24px; padding: 0; font-size: 12px; display: inline-flex; align-items: center; justify-content: center; }
  .tools button.on { color: var(--text); border-color: #8abaff; box-shadow: inset 0 -2px #8abaff; }
  .follow { display: flex; align-items: center; gap: 6px; font-size: 11px; }
  .follow input { margin: 0; }
  .follow-help { font-size: 10px; color: var(--muted); margin: 3px 0 8px 20px; }
  .pins { list-style: none; margin: 0 0 8px; padding: 0 0 6px; font-size: 12px; border-bottom: 1px solid var(--line); }
  .pins small { color: #e5bc6e; }
  .notice { font-size: 11px; color: var(--muted); margin: 0 0 8px; overflow-wrap: anywhere; }
  .tree { list-style: none; margin: 0; padding: 0; font-size: 12px; }
  .tree li { padding-left: calc(var(--depth) * 14px); }
  .row { display: flex; align-items: center; gap: 4px; width: 100%; border: 0; background: transparent; padding: 3px 4px; text-align: left; color: var(--text); }
  .row:hover, .row:focus-visible { background: var(--control); }
  .row.link .name { font-style: italic; }
  .twisty { width: 12px; flex: none; color: var(--muted); font-size: 10px; }
  .name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row small { color: var(--muted); font-size: 10px; flex: none; }
  .pin-mark { color: #e5bc6e; font-size: 8px; flex: none; }
  .note { color: var(--muted); font-size: 11px; padding-top: 3px; padding-bottom: 3px; margin-left: 20px; }
  .note.failed { color: #f3cfc7; }
  .hint { font-size: 10px; color: var(--muted); line-height: 1.7; margin: 10px 0 0; }
</style>
