<script lang="ts">
  import { untrack } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import { SearchAddon } from '@xterm/addon-search';
  import '@xterm/xterm/css/xterm.css';
  import type { TerminalError, TerminalInfo } from './client.js';
  import { outputHub, terminalClient } from './tauri.js';
  import { lastFocused } from './last-focused.svelte.js';
  import { acceptsResults, canSearch, emptySearch, HIGHLIGHT_LIMIT, MAX_QUERY_LENGTH, NO_RESULTS, resultLabel, searchOptions, toggleCaseSensitive, toggleRegex, validate, withQuery, type SearchState } from './search.js';
  import { copyKey, copyStatus, highlight, scrollFor, startCopy, type CopyState, type Extent } from './copy-mode.js';
  import { keyControl } from './keys.svelte.js';
  import { logStore } from './logs.svelte.js';
  import { broadcast } from './broadcast.svelte.js';

  interface Props {
    session: TerminalInfo;
    focused: boolean;
    onFocus: () => void;
    onError: (error: TerminalError) => void;
    onNotice: (text: string) => void;
  }
  let { session, focused, onFocus, onError, onNotice }: Props = $props();

  // Only the session id decides when the emulator is rebuilt; list refreshes hand us new
  // objects for the same session and must not tear the terminal down.
  const id = $derived(session.id);
  let host = $state<HTMLDivElement | undefined>();
  // Polling cost scales with the number of panes (16 or 32 terminals is normal): an idle pane backs off
  // from 70 ms to 600 ms, the focused pane never slower than 120 ms, and any output or typing resets it.
  let focusedNow = false;
  let idleReads = 0;
  // Bumped to rebuild the emulator (and replay the session's history into it) after its screen state broke.
  let generation = $state(0);
  let rebuilds: number[] = [];

  // Find in scrollback (tmux's search in copy mode). The addon and the emulator live in the effect below; these
  // hold the current pair so the exported functions (and the search bar) can reach them.
  let emulator: Terminal | undefined;
  let finder: SearchAddon | undefined;
  // The current emulator's input path (what typing goes through), for text the keymap sends (the prefix pressed twice).
  let typeInto: ((data: string) => void) | undefined;
  let wrap = $state<HTMLDivElement | undefined>();
  let searchOpen = $state(false);
  let search = $state<SearchState>(emptySearch());
  let resultIndex = $state(NO_RESULTS);
  let resultCount = $state(NO_RESULTS);
  let queryField: HTMLInputElement | undefined = $state();
  const searchMessage = $derived(validate(search));
  // Query-aware: an empty query or a bad pattern never renders a count, whatever the addon last reported.
  const resultLine = $derived(resultLabel(search, resultIndex, resultCount));

  function forgetResults(): void {
    resultIndex = NO_RESULTS;
    resultCount = NO_RESULTS;
  }

  // 1 = towards the newest output, -1 = back through the scrollback. An empty or invalid query clears the
  // highlight instead of asking the addon (which would throw on a bad pattern).
  function run(direction: 1 | -1): void {
    const addon = finder;
    if (!addon) return;
    if (!canSearch(search)) {
      addon.clearDecorations();
      forgetResults();
      return;
    }
    if (direction === 1) addon.findNext(search.query, searchOptions(search));
    else addon.findPrevious(search.query, searchOptions(search));
  }

  // Exported for the wave-2 keymap: no keyboard shortcut is bound here, so the terminal keeps every key it had.
  export function openSearch(): void {
    searchOpen = true;
    // The field does not exist until this renders.
    requestAnimationFrame(() => {
      queryField?.focus();
      queryField?.select();
    });
  }

  export function closeSearch(): void {
    searchOpen = false;
    finder?.clearDecorations();
    forgetResults();
    emulator?.focus();
  }

  export function findNext(): void { run(1); }
  export function findPrevious(): void { run(-1); }

  function onQueryInput(event: Event & { currentTarget: HTMLInputElement }): void {
    search = withQuery(search, event.currentTarget.value);
    run(1);
  }

  function flipQuery(flip: (state: SearchState) => SearchState): void {
    search = flip(search);
    // The addon keeps its matches for an unchanged term: clearing first makes it search again with the new options.
    finder?.clearDecorations();
    run(1);
  }

  // Copy mode (tmux prefix+[): every key moves a cursor through the scrollback or selects; none reaches the shell.
  // The cursor and the selection are drawn with the emulator's own selection.
  let copy = $state.raw<CopyState | null>(null);
  let copyLine = $state<string | null>(null);
  function extentOf(t: Terminal): Extent {
    return { lines: t.buffer.active.length, cols: t.cols, rows: t.rows };
  }
  function paintCopy(): void {
    const t = emulator;
    if (!t || !copy) return;
    const h = highlight(copy, t.cols);
    t.select(h.col, h.row, h.length);
    t.scrollToLine(scrollFor(copy.cursor.row, t.buffer.active.viewportY, t.rows));
    copyLine = copyStatus(copy, extentOf(t));
  }
  function enterCopy(): void {
    const t = emulator;
    if (!t) return;
    const b = t.buffer.active;
    copy = startCopy({ row: b.baseY + b.cursorY, col: b.cursorX }, extentOf(t));
    t.focus();
    paintCopy();
  }
  function leaveCopy(): void {
    copy = null;
    copyLine = null;
    emulator?.clearSelection();
    // With the search bar still open the person is looking at a match: stay there.
    if (!searchOpen) emulator?.scrollToBottom();
  }
  /** A key while copy mode is on (from the emulator's key handler). Always false: the shell gets nothing. */
  function copyKeydown(e: KeyboardEvent): boolean {
    const t = emulator;
    if (!t || !copy || e.type !== 'keydown') return false;
    // Cancelled, so the browser sends no keypress after it: the key that leaves copy mode (q, y) would otherwise be
    // typed into the shell once the mode is already off.
    e.preventDefault();
    const step = copyKey(copy, e, extentOf(t));
    copy = step.state;
    if (step.effect === 'copy') {
      const text = t.getSelection();
      void navigator.clipboard?.writeText(text).then(() => onNotice(`Copied ${text.length} characters.`), () => onNotice('The clipboard is unavailable.'));
      leaveCopy();
    } else if (step.effect === 'exit') {
      leaveCopy();
    } else if (step.effect === 'search') {
      openSearch();
    } else if (step.effect === 'next-match' || step.effect === 'previous-match') {
      run(step.effect === 'next-match' ? 1 : -1);
      // The cursor jumps to the match the search selected.
      const at = t.getSelectionPosition();
      if (at) { copy = { cursor: { row: at.start.y, col: at.start.x }, anchor: null }; copyLine = copyStatus(copy, extentOf(t)); }
    } else {
      paintCopy();
    }
    return false;
  }

  function onSearchKey(event: KeyboardEvent): void {
    // The bar owns its keys: they must never reach the shell.
    event.stopPropagation();
    if (event.key === 'Enter') {
      event.preventDefault();
      run(event.shiftKey ? -1 : 1);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      closeSearch();
    }
  }

  // The emulator must never answer program queries (device attributes, cursor position,
  // colour and version queries, mode reports): replies would be typed into the shell.
  function suppressQueryReplies(t: Terminal): void {
    const swallow = () => true;
    for (const f of ['c', 'n', 't']) t.parser.registerCsiHandler({ final: f }, swallow);
    t.parser.registerCsiHandler({ prefix: '>', final: 'c' }, swallow);
    t.parser.registerCsiHandler({ prefix: '=', final: 'c' }, swallow);
    t.parser.registerCsiHandler({ prefix: '?', final: 'n' }, swallow);
    t.parser.registerCsiHandler({ prefix: '>', final: 'q' }, swallow);
    t.parser.registerCsiHandler({ intermediates: '$', final: 'p' }, swallow);
    t.parser.registerCsiHandler({ prefix: '?', intermediates: '$', final: 'p' }, swallow);
    for (const code of [4, 10, 11, 12, 52, 104, 110, 111, 112]) t.parser.registerOscHandler(code, swallow);
    t.parser.registerDcsHandler({ intermediates: '$', final: 'q' }, swallow);
  }

  $effect(() => {
    const sessionId = id;
    const el = host;
    void generation; // a rebuild request re-runs this effect
    if (!el) return;
    const initial = untrack(() => session);
    let alive = true;
    let from = 0;
    let lastSize = '';
    let pollTimer: ReturnType<typeof setTimeout> | undefined;
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let inputQueue: Promise<unknown> = Promise.resolve();
    // After a keystroke the shell's echo is a few milliseconds away: read at once and then fast for a moment,
    // instead of waiting out the idle timer (which is what made typing feel laggy).
    let reading = false;
    let again = false;
    let burstUntil = 0;
    const BURST_MS = 400;
    const BURST_INTERVAL_MS = 6;
    // Output normally arrives through the shared watcher (hub.ts); this pane polls only when that is unavailable
    // or has stopped for it.
    let polling = false;
    const wake = (): void => {
      burstUntil = performance.now() + BURST_MS;
      if (!polling) return; // the watcher delivers the echo as soon as the shell produces it
      if (reading) { again = true; return; }
      clearTimeout(pollTimer);
      void pump();
    };

    const t = new Terminal({
      cols: initial.cols, rows: initial.rows, scrollback: 10_000, cursorBlink: true, convertEol: false,
      // The proposed API is needed for decorations, which the search addon draws its highlights and result count with
      // (xterm 6 refuses registerDecoration without it). It only widens what this code may call, not what output can do.
      fontFamily: '"Cascadia Mono", Consolas, "Courier New", monospace', fontSize: 13, allowProposedApi: true,
      theme: { background: '#0b121a', foreground: '#e4ebf5', cursor: '#9bc2ff', selectionBackground: '#33557f', scrollbarSliderBackground: '#3a4a60b3', scrollbarSliderHoverBackground: '#566b88e6', scrollbarSliderActiveBackground: '#7f9cc4' }
    });
    suppressQueryReplies(t);
    const fit = new FitAddon();
    t.loadAddon(fit);
    // Decorations are what make the addon report a result count (and highlight every match).
    const finderAddon = new SearchAddon({ highlightLimit: HIGHLIGHT_LIMIT });
    t.loadAddon(finderAddon);
    t.open(el);
    emulator = t;
    finder = finderAddon;
    forgetResults();
    copy = null;
    copyLine = null;
    // clearDecorations() reports an empty result set, so a report only counts while the query is searchable:
    // otherwise an empty query would read "No matches" and a bad pattern would lose its validation message.
    const stopResults = finderAddon.onDidChangeResults(({ resultIndex: index, resultCount: count }) => {
      if (!acceptsResults(search)) return;
      resultIndex = index;
      resultCount = count;
    });

    const fail = (e: unknown) => onError(e as TerminalError);
    // The emulator can end up with a cursor row that does not exist (seen as "Cannot set properties of undefined
    // (setting 'isWrapped')" after a resize or a write): every later write then throws too. The cure is a fresh
    // emulator fed the session's history again. At most 3 rebuilds in 10 s, so a bad byte stream cannot loop.
    const broken = (): boolean => {
      try {
        const b = t.buffer.active;
        return !b.getLine(b.baseY + b.cursorY);
      } catch {
        return true;
      }
    };
    const rebuild = (why: unknown): boolean => {
      if (!alive) return false;
      const now = Date.now();
      rebuilds = rebuilds.filter(at => now - at < 10_000);
      if (rebuilds.length >= 3) return false;
      rebuilds.push(now);
      console.warn('terminal emulator rebuilt:', why);
      queueMicrotask(() => { generation += 1; });
      return true;
    };
    const onWindowError = (e: ErrorEvent): void => {
      if (/isWrapped|Cannot set properties of undefined/.test(e.message ?? '') && broken()) rebuild(e.message);
    };
    window.addEventListener('error', onWindowError);
    // Keystrokes keep their order: one request at a time per session.
    const type = (data: string): void => {
      idleReads = 0;
      burstUntil = performance.now() + BURST_MS;
      inputQueue = inputQueue.then(() => terminalClient.input(sessionId, data)).then(wake).catch(fail);
      // Typing into all panes at once (prefix+S): the others get the same keystrokes.
      broadcast.fanOut(sessionId, data, fail);
    };
    t.onData(type);
    typeInto = type;
    t.attachCustomKeyEventHandler(e => {
      if (copy !== null) return copyKeydown(e);
      if (e.type !== 'keydown') return true;
      if (e.ctrlKey && !e.shiftKey && e.key.toLowerCase() === 'c' && t.hasSelection()) {
        void navigator.clipboard?.writeText(t.getSelection()).catch(() => undefined);
        return false;
      }
      if (e.ctrlKey && e.key.toLowerCase() === 'v') return false; // let the browser paste event run
      return true;
    });

    const sendResize = () => {
      if (!alive) return;
      // The emulator and the shell's console get the same clamped size (a sliver of a window used to leave the
      // emulator at 2 columns while the shell believed 20).
      const proposed = fit.proposeDimensions();
      if (!proposed || !Number.isFinite(proposed.cols) || !Number.isFinite(proposed.rows)) return;
      const cols = Math.min(400, Math.max(20, proposed.cols));
      const rows = Math.min(200, Math.max(5, proposed.rows));
      if (cols !== t.cols || rows !== t.rows) {
        try {
          t.resize(cols, rows);
        } catch (e) {
          rebuild(e);
          return;
        }
        if (broken()) { rebuild('cursor row missing after resize'); return; }
      }
      const key = `${cols}x${rows}`;
      if (key === lastSize) return;
      lastSize = key;
      terminalClient.resize(sessionId, cols, rows).catch(fail);
    };
    const observer = new ResizeObserver(() => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(sendResize, 120);
    });
    observer.observe(el);
    sendResize();

    const pump = async (): Promise<void> => {
      if (!alive) return;
      if (document.hidden) {
        pollTimer = setTimeout(pump, 400);
        return;
      }
      reading = true;
      try {
        const r = await terminalClient.read(sessionId, from);
        reading = false;
        if (!alive) return;
        if (r.skipped > 0) onNotice(`${r.skipped} bytes of earlier output were dropped (history limit).`);
        if (r.bytes.length > 0) {
          try {
            t.write(r.bytes);
          } catch (e) {
            // Not advancing `from` would re-feed the same bytes into a broken emulator: rebuild and replay instead.
            if (rebuild(e)) return;
            throw e;
          }
        }
        from = r.next;
        idleReads = r.bytes.length > 0 ? 0 : idleReads + 1;
        const idle = Math.min(70 + idleReads * 10, focusedNow ? 120 : 600);
        const wanted = again;
        again = false;
        pollTimer = setTimeout(pump, wanted ? 0 : r.bytes.length > 0 ? 8 : performance.now() < burstUntil ? BURST_INTERVAL_MS : idle);
      } catch (e) {
        reading = false;
        again = false;
        if (!alive) return;
        fail(e);
        pollTimer = setTimeout(pump, 1500);
      }
    };
    const write = (bytes: Uint8Array, next: number, skipped: number): void => {
      if (skipped > 0) onNotice(`${skipped} bytes of earlier output were dropped (history limit).`);
      if (bytes.length > 0) {
        try {
          t.write(bytes);
        } catch (e) {
          if (rebuild(e)) return;
          fail(e);
        }
      }
      from = next;
      idleReads = bytes.length > 0 ? 0 : idleReads + 1;
    };
    const startPolling = (): void => {
      if (!alive || polling) return;
      polling = true;
      void pump();
    };
    const stopWatching = outputHub.subscribe(sessionId, from, {
      onOutput: (bytes, next, skipped) => { if (alive) write(bytes, next, skipped); },
      onFallback: () => startPolling()
    });
    if (stopWatching === null) startPolling();

    return () => {
      if (typeInto === type) typeInto = undefined;
      stopWatching?.();
      stopResults.dispose();
      if (finder === finderAddon) {
        finder = undefined;
        emulator = undefined;
        forgetResults();
      }
      alive = false;
      clearTimeout(pollTimer);
      clearTimeout(resizeTimer);
      observer.disconnect();
      window.removeEventListener('error', onWindowError);
      t.dispose();
    };
  });

  // The keymap reaches this terminal's search and copy mode through these (prefix+/ and prefix+[).
  $effect(() => {
    const el = wrap;
    if (!el) return;
    // In copy mode nothing reaches the shell, the prefix sent on included.
    return keyControl.registerTerminal(el, { search: openSearch, copyMode: enterCopy, send: text => { if (copy === null) typeInto?.(text); } });
  });
  // Whether it is being logged (the REC mark); asked when shown, kept current by the Terminals view and each toggle.
  $effect(() => { void logStore.refresh(id); });

  // Focus the emulator when the pane becomes the focused one, unless the search bar is what the user is in.
  $effect(() => {
    focusedNow = focused;
    if (focused && !searchOpen) {
      idleReads = 0;
      host?.querySelector<HTMLTextAreaElement>('textarea')?.focus();
    }
  });
</script>

<!-- The padding is on the wrapper, not on the element the emulator lives in: the fit addon measures its parent's
     height as if it had no padding, so padding there made the emulator one row too tall (the bottom line was
     cut off or pushed past the window while output streamed). -->
<div class="pane-wrap" bind:this={wrap} class:focused class:copying={copy !== null} data-terminal={session.id} onpointerdown={onFocus} onfocusin={() => lastFocused.set(session.id, session.label)} role="presentation">
  <div class="pane-host" bind:this={host}></div>
  {#if logStore.on(session.id)}<span class="rec" title={`Everything this terminal prints is being saved to ${logStore.of(session.id)?.path ?? 'a log file'} (passwords included). Prefix then L stops it.`}>REC</span>{/if}
  {#if copyLine}<div class="copy-line" role="status">{copyLine}</div>{/if}
  <!-- The bar overlays the top-right corner of the pane, as tmux's search prompt does in copy mode. It only
       appears on hover/focus so an idle pane stays a bare terminal. -->
  <div class="pane-tools" class:open={searchOpen}>
    {#if searchOpen}
      <!-- Clicking the bar still focuses the pane (the focus effect below does not touch the emulator while the bar
           is open), so the pane the user searches is the pane the keymap will act on. -->
      <div class="search-bar" role="search" aria-label="Search this terminal's scrollback">
        <div class="search-row">
          <input class="search-query" bind:this={queryField} type="text" value={search.query} maxlength={MAX_QUERY_LENGTH} placeholder="Search scrollback" spellcheck="false" autocomplete="off" aria-label="Search text" oninput={onQueryInput} onkeydown={onSearchKey} />
          <button type="button" class="step" aria-label="Previous match" title="Previous match (Shift+Enter)" onclick={() => run(-1)}>↑</button>
          <button type="button" class="step" aria-label="Next match" title="Next match (Enter)" onclick={() => run(1)}>↓</button>
          <button type="button" class="flag" class:on={search.caseSensitive} aria-pressed={search.caseSensitive} aria-label="Match case" title="Match case" onclick={() => flipQuery(toggleCaseSensitive)}>Aa</button>
          <button type="button" class="flag" class:on={search.regex} aria-pressed={search.regex} aria-label="Regular expression" title="Regular expression" onclick={() => flipQuery(toggleRegex)}>.*</button>
          <button type="button" class="shut" aria-label="Close search" title="Close search (Escape)" onclick={closeSearch}>×</button>
        </div>
        <div class="search-result" class:bad={searchMessage !== null} aria-live="polite">{resultLine}</div>
      </div>
    {:else}
      <button type="button" class="search-open" aria-label="Search this terminal's scrollback" title="Search this terminal's scrollback" onclick={openSearch}><svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" focusable="false" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="6.6" cy="6.6" r="4.1" /><path d="M9.7 9.7l4 4" /></svg></button>
    {/if}
  </div>
</div>

<style>
  .pane-wrap { position: relative; width: 100%; height: 100%; min-height: 0; padding: 6px; overflow: hidden; background: #0b121a; }
  .pane-host { width: 100%; height: 100%; min-height: 0; overflow: hidden; }
  /* Hidden until the pane is hovered or holds the caret, so an idle terminal looks untouched. The wrapper spans the
     pane so the bar can size itself against it, but it never takes a click: only the button or the bar does. */
  .pane-tools { position: absolute; top: 6px; left: 6px; right: 6px; z-index: 5; display: flex; justify-content: flex-end; opacity: 0; pointer-events: none; transition: opacity 120ms; }
  .pane-wrap:hover .pane-tools, .pane-wrap:focus-within .pane-tools, .pane-tools.open { opacity: 1; }
  .pane-tools > * { pointer-events: auto; }
  .search-open { display: flex; align-items: center; justify-content: center; width: 20px; height: 20px; padding: 0; color: #a5b3c6; background: #0b121ad9; border: 1px solid #3a4a60; }
  .search-open:hover, .search-open:focus-visible { color: #e4ebf5; border-color: #8abaff; }
  .search-bar { width: min(336px, 100%); padding: 4px; background: #0b121a; border: 1px solid #60738d; box-shadow: 0 12px 32px #0008; }
  .search-row { display: flex; align-items: stretch; gap: 2px; }
  /* Three classes so the global input padding/margin (style.css) loses to the compact bar. */
  .search-bar .search-row input.search-query { flex: 1; display: block; width: auto; margin: 0; padding: 2px 6px; font: 11px ui-monospace, monospace; color: #e4ebf5; background: #151e29; border: 1px solid #3a4a60; }
  .search-bar .search-row input.search-query::placeholder { color: #6b7c92; }
  .search-bar button { padding: 0 6px; font: 11px ui-monospace, monospace; color: #a5b3c6; background: #151e29; border: 1px solid #3a4a60; }
  .search-bar button:hover { color: #e4ebf5; border-color: #8abaff; }
  .search-bar button.on { color: #e5bc6e; border-color: #e5bc6e; }
  .search-bar .shut { padding: 0 5px; font-size: 13px; }
  .search-result { margin-top: 3px; padding: 0 2px; font: 10px ui-monospace, monospace; color: #a5b3c6; overflow-wrap: anywhere; }
  .search-result:empty { display: none; }
  .search-result.bad { color: #e29787; }
  /* Copy mode: an amber edge and a status line, like tmux's mode indicator; the terminal itself stays readable. */
  .pane-wrap.copying { box-shadow: inset 0 0 0 1px #e5bc6e; }
  .copy-line { position: absolute; left: 6px; right: 6px; bottom: 6px; z-index: 5; padding: 2px 6px; font: 10px ui-monospace, monospace; color: #0b121a; background: #e5bc6e; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; pointer-events: none; }
  /* Logging: always visible while on (the design requires an indicator: the file holds whatever was printed). */
  .rec { position: absolute; top: 6px; left: 6px; z-index: 5; padding: 0 5px; font: 700 9px ui-monospace, monospace; letter-spacing: .5px; color: #fff; background: #b8473a; pointer-events: auto; }
</style>
