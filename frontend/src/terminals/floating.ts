// Pure logic for terminals that are NOT in the tmux-style grid: floating windows on the Canvas (free
// position, size and overlap, so any shape such as a star or cross is possible) and links between a
// session panel and a terminal (the panel can flip between its transcript and that terminal).
// A terminal is shown in at most one place at a time (grid pane, floating window, flipped panel); this module
// owns the canvas side of that rule via `occupied`, the grid side lives in panes.ts.

export interface Box { readonly x: number; readonly y: number; readonly w: number; readonly h: number }
/** A minimized window: a bar (title only) or a small block. Absent means full size. */
export type Stage = 'compact' | 'block';
export interface Win extends Box {
  readonly terminal: string;
  /** Stacking order; higher is on top. */
  readonly z: number;
  /** Minimized like a session panel; `w` and `h` keep the full size to restore. Left out while full. */
  readonly stage?: Stage;
}
/** A window that only WATCHES an existing agent session (its Activity view), with no terminal. */
export interface Watch extends Box {
  /** Opaque inspector source id (16 hex); never a path. */
  readonly source: string;
  readonly agent: 'claude' | 'pi' | 'codex';
  readonly label: string;
  readonly z: number;
}
export interface Link {
  readonly terminal: string;
  /** True while the session's panel shows the terminal instead of its transcript. */
  readonly flipped: boolean;
}
export interface CanvasTerminals {
  readonly version: 2;
  readonly windows: readonly Win[];
  readonly watches: readonly Watch[];
  /** Session id (journal UUID) -> its linked terminal. */
  readonly links: Readonly<Record<string, Link>>;
  /** Terminal that last had the user's attention (keyboard focus). */
  readonly active: string | null;
}
export interface Size { readonly w: number; readonly h: number }

export const MAX_WINDOWS = 32;
export const MAX_LINKS = 16;
export const MAX_WATCHES = 16;
export const MIN_W = 240;
export const MIN_H = 140;
export const DEFAULT_W = 560;
export const DEFAULT_H = 340;
/** How much of a window must stay inside the canvas so its title bar can always be grabbed. */
export const KEEP_VISIBLE = 80;
const CASCADE = 28;
const MAX_COORD = 100_000;

const terminalId = /^[0-9a-f]{8}$/;
const sourceId = /^[0-9a-f]{16}$/;
const sessionId = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

export function defaultState(): CanvasTerminals {
  return { version: 2, windows: [], watches: [], links: {}, active: null };
}

const finite = (n: number, fallback: number): number => (Number.isFinite(n) ? n : fallback);

/** Keep a window usable inside a canvas: at least MIN size (or the canvas, if smaller) and its title bar reachable. */
export function clampWin<T extends Box>(win: T, canvas: Size): T {
  const w = Math.round(Math.min(Math.max(finite(win.w, DEFAULT_W), MIN_W), Math.max(canvas.w, MIN_W)));
  const h = Math.round(Math.min(Math.max(finite(win.h, DEFAULT_H), MIN_H), Math.max(canvas.h, MIN_H)));
  const x = Math.round(Math.min(Math.max(finite(win.x, 0), KEEP_VISIBLE - w), Math.max(canvas.w - KEEP_VISIBLE, 0)));
  const y = Math.round(Math.min(Math.max(finite(win.y, 0), 0), Math.max(canvas.h - 40, 0)));
  return { ...win, x, y, w, h };
}

/** Size of a minimized window: a bar is the title row (at most 320 wide), a block is a small square. */
export const BAR_W = 320;
export const BAR_H = 28;
export const BLOCK_W = 72;
export const BLOCK_H = 48;
function stageSize(win: Win): Size {
  return win.stage === 'block' ? { w: BLOCK_W, h: BLOCK_H } : { w: Math.min(BAR_W, Math.max(finite(win.w, DEFAULT_W), MIN_W)), h: BAR_H };
}
/** A minimized window stays wholly inside the canvas (so bars can line up along any edge); its full size is kept. */
function clampMinimized(win: Win, canvas: Size): Win {
  const shown = stageSize(win);
  const x = Math.round(Math.min(Math.max(finite(win.x, 0), 0), Math.max(canvas.w - shown.w, 0)));
  const y = Math.round(Math.min(Math.max(finite(win.y, 0), 0), Math.max(canvas.h - shown.h, 0)));
  return { ...win, x, y };
}
/** Where and how big a window is drawn: full windows as `clampWin`, minimized ones at their bar or block size. */
export function shownBox(win: Win, canvas: Size): Box {
  if (!win.stage) return clampWin(win, canvas);
  const { x, y } = clampMinimized(win, canvas);
  return { x, y, ...stageSize(win) };
}

export function windowOf(state: CanvasTerminals, terminal: string): Win | undefined {
  return state.windows.find(win => win.terminal === terminal);
}
export function isFloating(state: CanvasTerminals, terminal: string): boolean {
  return windowOf(state, terminal) !== undefined;
}
export function linkOf(state: CanvasTerminals, session: string): Link | undefined {
  return state.links[session];
}
/** Terminals shown on the canvas right now (floating, or flipped into a session panel). */
export function occupied(state: CanvasTerminals): Set<string> {
  const out = new Set(state.windows.map(win => win.terminal));
  for (const link of Object.values(state.links)) if (link.flipped) out.add(link.terminal);
  return out;
}
/** Windows bottom-to-top, the order to render them in. */
export function stacked(state: CanvasTerminals): Win[] {
  return [...state.windows].sort((a, b) => a.z - b.z);
}

function topZ(state: CanvasTerminals): number {
  return Math.max(0, ...state.windows.map(win => win.z), ...state.watches.map(watch => watch.z));
}
function unflip(links: CanvasTerminals['links'], terminal: string): CanvasTerminals['links'] {
  return Object.fromEntries(Object.entries(links).map(([s, l]) => [s, l.terminal === terminal && l.flipped ? { ...l, flipped: false } : l]));
}

/** Open a terminal in a floating window (cascaded from the top-left, on top). It leaves any flipped panel. Refused at MAX_WINDOWS. */
export function floatTerminal(state: CanvasTerminals, terminal: string, canvas: Size): CanvasTerminals {
  if (!terminalId.test(terminal)) return state;
  // Asking for a window that is already open (and maybe minimized) brings it back full size, on top.
  if (isFloating(state, terminal)) return raise(restoreWindow(state, terminal), terminal);
  if (state.windows.length >= MAX_WINDOWS) return state;
  const offset = (state.windows.length % 10) * CASCADE;
  const win = clampWin({ terminal, x: 24 + offset, y: 48 + offset, w: DEFAULT_W, h: DEFAULT_H, z: topZ(state) + 1 }, canvas);
  return { ...state, windows: [...state.windows, win], links: unflip(state.links, terminal), active: terminal };
}

/** Close a floating window (the terminal keeps running; it simply is not shown here any more). */
export function dock(state: CanvasTerminals, terminal: string): CanvasTerminals {
  if (!isFloating(state, terminal)) return state;
  return { ...state, windows: state.windows.filter(win => win.terminal !== terminal), active: state.active === terminal ? null : state.active };
}

function update(state: CanvasTerminals, terminal: string, f: (win: Win) => Win): CanvasTerminals {
  const current = windowOf(state, terminal);
  if (!current) return state;
  const next = f(current);
  return next === current ? state : { ...state, windows: state.windows.map(win => (win.terminal === terminal ? next : win)) };
}
export function moveWindow(state: CanvasTerminals, terminal: string, x: number, y: number, canvas: Size): CanvasTerminals {
  return update(state, terminal, win => (win.stage ? clampMinimized({ ...win, x, y }, canvas) : clampWin({ ...win, x, y }, canvas)));
}
/** A minimized window is not resized (its full size waits to be restored). */
export function resizeWindow(state: CanvasTerminals, terminal: string, w: number, h: number, canvas: Size): CanvasTerminals {
  return update(state, terminal, win => (win.stage ? win : clampWin({ ...win, w, h }, canvas)));
}
/** Minimize like a session panel: full, then a bar, then a block, then full again (at the size it had). */
export function cycleWindow(state: CanvasTerminals, terminal: string, canvas: Size): CanvasTerminals {
  return update(state, terminal, win => {
    const { stage: _, ...full } = win;
    if (win.stage === 'block') return clampWin(full, canvas);
    return clampMinimized({ ...full, stage: win.stage === 'compact' ? 'block' : 'compact' }, canvas);
  });
}
/** Back to full size (no change when it already is). */
export function restoreWindow(state: CanvasTerminals, terminal: string): CanvasTerminals {
  return update(state, terminal, win => {
    if (!win.stage) return win;
    const { stage: _, ...full } = win;
    return full;
  });
}
/** Bring a window to the front and mark it active. */
export function raise(state: CanvasTerminals, terminal: string): CanvasTerminals {
  const win = windowOf(state, terminal);
  if (!win) return state;
  const top = topZ(state);
  if (win.z === top && state.active === terminal) return state;
  return { ...state, windows: state.windows.map(w => (w.terminal === terminal && w.z !== top ? { ...w, z: top + 1 } : w)), active: terminal };
}

// ---- watch windows (Activity of an existing agent session, no terminal) ---------------------

export function watchOf(state: CanvasTerminals, source: string): Watch | undefined {
  return state.watches.find(watch => watch.source === source);
}
export function isWatching(state: CanvasTerminals, source: string): boolean {
  return watchOf(state, source) !== undefined;
}
/** A label that is safe to show: printable, bounded. */
function cleanLabel(label: string): string {
  return Array.from(label).filter(c => !/[\u0000-\u001f\u007f-\u009f]/.test(c)).join('').trim().slice(0, 120);
}
/** Open an existing agent session's Activity in a floating window (cascaded, on top). Refused at MAX_WATCHES. */
export function floatWatch(state: CanvasTerminals, source: string, agent: Watch['agent'], label: string, canvas: Size): CanvasTerminals {
  if (!sourceId.test(source) || !['claude', 'pi', 'codex'].includes(agent)) return state;
  if (isWatching(state, source)) return raiseWatch(state, source);
  if (state.watches.length >= MAX_WATCHES) return state;
  const offset = ((state.watches.length + state.windows.length) % 10) * CASCADE;
  const watch = clampWin<Watch>({ source, agent, label: cleanLabel(label) || agent, x: 60 + offset, y: 60 + offset, w: 520, h: 380, z: topZ(state) + 1 }, canvas);
  return { ...state, watches: [...state.watches, watch] };
}
/**
 * Show a terminal that was just opened from a watched session in the watch window's place: same position,
 * size and stacking, so the window turns into the terminal instead of a second window appearing. Without a
 * watch window for `source` it opens like any floating terminal. Refused (state unchanged) at MAX_WINDOWS.
 */
export function openFromWatch(state: CanvasTerminals, source: string, terminal: string, canvas: Size): CanvasTerminals {
  if (!terminalId.test(terminal) || !sourceId.test(source)) return state;
  const watch = watchOf(state, source);
  if (!watch) return floatTerminal(state, terminal, canvas);
  if (isFloating(state, terminal)) return closeWatch(raise(state, terminal), source);
  if (state.windows.length >= MAX_WINDOWS) return state;
  const win = clampWin({ terminal, x: watch.x, y: watch.y, w: watch.w, h: watch.h, z: watch.z }, canvas);
  return { ...state, windows: [...state.windows, win], watches: state.watches.filter(w => w.source !== source), links: unflip(state.links, terminal), active: terminal };
}

export function closeWatch(state: CanvasTerminals, source: string): CanvasTerminals {
  return isWatching(state, source) ? { ...state, watches: state.watches.filter(w => w.source !== source) } : state;
}
function updateWatch(state: CanvasTerminals, source: string, f: (w: Watch) => Watch): CanvasTerminals {
  const current = watchOf(state, source);
  if (!current) return state;
  const next = f(current);
  return next === current ? state : { ...state, watches: state.watches.map(w => (w.source === source ? next : w)) };
}
export function moveWatch(state: CanvasTerminals, source: string, x: number, y: number, canvas: Size): CanvasTerminals {
  return updateWatch(state, source, w => clampWin({ ...w, x, y }, canvas));
}
export function resizeWatch(state: CanvasTerminals, source: string, w: number, h: number, canvas: Size): CanvasTerminals {
  return updateWatch(state, source, win => clampWin({ ...win, w, h }, canvas));
}
export function raiseWatch(state: CanvasTerminals, source: string): CanvasTerminals {
  const watch = watchOf(state, source);
  if (!watch) return state;
  const top = topZ(state);
  return watch.z === top ? state : { ...state, watches: state.watches.map(w => (w.source === source ? { ...w, z: top + 1 } : w)) };
}

// ---- one set of entry points for either kind of window (terminal ids are 8 hex, source ids 16 hex) ----

export type Item = { readonly kind: 'terminal'; readonly win: Win } | { readonly kind: 'watch'; readonly watch: Watch };
export function itemOf(state: CanvasTerminals, key: string): Item | undefined {
  const watch = sourceId.test(key) ? watchOf(state, key) : undefined;
  if (watch) return { kind: 'watch', watch };
  const win = terminalId.test(key) ? windowOf(state, key) : undefined;
  return win ? { kind: 'terminal', win } : undefined;
}
export function moveItem(state: CanvasTerminals, key: string, x: number, y: number, canvas: Size): CanvasTerminals {
  return sourceId.test(key) ? moveWatch(state, key, x, y, canvas) : moveWindow(state, key, x, y, canvas);
}
export function resizeItem(state: CanvasTerminals, key: string, w: number, h: number, canvas: Size): CanvasTerminals {
  return sourceId.test(key) ? resizeWatch(state, key, w, h, canvas) : resizeWindow(state, key, w, h, canvas);
}
export function raiseItem(state: CanvasTerminals, key: string): CanvasTerminals {
  return sourceId.test(key) ? raiseWatch(state, key) : raise(state, key);
}

/** Attach a terminal to a session (one terminal per session and one session per terminal; the previous link is replaced). */
export function link(state: CanvasTerminals, session: string, terminal: string): CanvasTerminals {
  if (!sessionId.test(session) || !terminalId.test(terminal)) return state;
  if (state.links[session]?.terminal === terminal) return state;
  const links: Record<string, Link> = {};
  for (const [s, l] of Object.entries(state.links)) if (s !== session && l.terminal !== terminal) links[s] = l;
  if (!(session in state.links) && Object.keys(links).length >= MAX_LINKS) return state;
  links[session] = { terminal, flipped: false };
  return { ...state, links };
}
export function unlink(state: CanvasTerminals, session: string): CanvasTerminals {
  if (!(session in state.links)) return state;
  return { ...state, links: Object.fromEntries(Object.entries(state.links).filter(([s]) => s !== session)) };
}
/** Show the linked terminal in the session's panel (on) or the transcript again (off). Turning it on takes the terminal out of a floating window. */
export function flip(state: CanvasTerminals, session: string, on: boolean): CanvasTerminals {
  const current = state.links[session];
  if (!current || current.flipped === on) return state;
  const windows = on ? state.windows.filter(win => win.terminal !== current.terminal) : state.windows;
  return { ...state, windows, links: { ...state.links, [session]: { ...current, flipped: on } }, active: on ? current.terminal : state.active };
}
/** Take a terminal off the canvas entirely (used when the tmux grid claims it). Links stay; only the display goes. */
export function release(state: CanvasTerminals, terminal: string): CanvasTerminals {
  if (!occupied(state).has(terminal)) return state;
  const undocked = dock(state, terminal);
  return { ...undocked, links: unflip(undocked.links, terminal) };
}

/** Drop windows and links whose terminal or session no longer exists. Call only after a successful listing. */
export function reconcile(state: CanvasTerminals, terminals: ReadonlySet<string>, sessions?: ReadonlySet<string>): CanvasTerminals {
  const windows = state.windows.filter(win => terminals.has(win.terminal));
  const links = Object.fromEntries(Object.entries(state.links).filter(([s, l]) => terminals.has(l.terminal) && (sessions === undefined || sessions.has(s))));
  const same = windows.length === state.windows.length && Object.keys(links).length === Object.keys(state.links).length;
  const active = state.active !== null && !terminals.has(state.active) ? null : state.active;
  return same && active === state.active ? state : { ...state, windows, links, active };
}

/** Validate untrusted stored state; anything odd is dropped (never trusted), duplicates repaired. */
export function parse(value: unknown): CanvasTerminals {
  if (typeof value !== 'object' || value === null) return defaultState();
  const v = value as Record<string, unknown>;
  // Version 1 (before watch windows) is read as "no watches".
  if (v.version !== 1 && v.version !== 2) return defaultState();
  const windows: Win[] = [];
  const seen = new Set<string>();
  if (Array.isArray(v.windows)) {
    for (const raw of v.windows.slice(0, MAX_WINDOWS)) {
      if (typeof raw !== 'object' || raw === null) continue;
      const r = raw as Record<string, unknown>;
      const num = (n: unknown): n is number => typeof n === 'number' && Number.isFinite(n) && Math.abs(n) <= MAX_COORD;
      if (typeof r.terminal !== 'string' || !terminalId.test(r.terminal) || seen.has(r.terminal)) continue;
      if (!num(r.x) || !num(r.y) || !num(r.w) || !num(r.h) || !num(r.z) || r.w < 1 || r.h < 1) continue;
      if (r.stage !== undefined && r.stage !== 'compact' && r.stage !== 'block') continue;
      seen.add(r.terminal);
      const win: Win = { terminal: r.terminal, x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.w), h: Math.round(r.h), z: Math.max(0, Math.round(r.z)) };
      windows.push(r.stage === undefined ? win : { ...win, stage: r.stage });
    }
  }
  const num = (n: unknown): n is number => typeof n === 'number' && Number.isFinite(n) && Math.abs(n) <= MAX_COORD;
  const watches: Watch[] = [];
  const watched = new Set<string>();
  if (v.version === 2 && Array.isArray(v.watches)) {
    for (const raw of v.watches.slice(0, MAX_WATCHES)) {
      if (typeof raw !== 'object' || raw === null) continue;
      const r = raw as Record<string, unknown>;
      if (typeof r.source !== 'string' || !sourceId.test(r.source) || watched.has(r.source)) continue;
      if (r.agent !== 'claude' && r.agent !== 'pi' && r.agent !== 'codex') continue;
      if (typeof r.label !== 'string' || !num(r.x) || !num(r.y) || !num(r.w) || !num(r.h) || !num(r.z) || r.w < 1 || r.h < 1) continue;
      watched.add(r.source);
      watches.push({ source: r.source, agent: r.agent, label: cleanLabel(r.label) || r.agent, x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.w), h: Math.round(r.h), z: Math.max(0, Math.round(r.z)) });
    }
  }
  // Renumber stacking densely over BOTH kinds of window, so z never grows without bound and stays one order.
  const everyone = [...windows.map(w => ({ kind: 't' as const, z: w.z, w })), ...watches.map(w => ({ kind: 'w' as const, z: w.z, w }))].sort((a, b) => a.z - b.z);
  const rank = new Map(everyone.map((e, i) => [e.w as object, i + 1]));
  const ordered = [...windows].sort((a, b) => a.z - b.z).map(win => ({ ...win, z: rank.get(win) ?? 1 }));
  const orderedWatches = watches.map(w => ({ ...w, z: rank.get(w) ?? 1 }));
  const links: Record<string, Link> = {};
  const linked = new Set<string>();
  if (typeof v.links === 'object' && v.links !== null && !Array.isArray(v.links)) {
    for (const [s, raw] of Object.entries(v.links as Record<string, unknown>).slice(0, MAX_LINKS)) {
      if (!sessionId.test(s) || typeof raw !== 'object' || raw === null) continue;
      const r = raw as Record<string, unknown>;
      if (typeof r.terminal !== 'string' || !terminalId.test(r.terminal) || linked.has(r.terminal) || typeof r.flipped !== 'boolean') continue;
      linked.add(r.terminal);
      // Shown at most once: a terminal both floating and flipped stays floating.
      links[s] = { terminal: r.terminal, flipped: r.flipped && !seen.has(r.terminal) };
    }
  }
  const active = typeof v.active === 'string' && terminalId.test(v.active) ? v.active : null;
  return { version: 2, windows: ordered, watches: orderedWatches, links, active };
}
