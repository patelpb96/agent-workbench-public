// Pure pane-arrangement logic for the terminals view (no DOM, no Tauri), modelled on tmux: the
// window is a binary tree of splits whose leaves are panes, with no fixed small cap (tmux limits a
// window only by the room left on screen; here by MAX_LEAVES and a minimum pane size the view enforces).
// One session is shown in at most one pane, so every pane owns its own PTY size and two panes never
// fight over it.

export type Direction = 'left' | 'right' | 'up' | 'down';
/** `row`: children side by side (a left, b right). `column`: stacked (a on top, b below). */
export type SplitDir = 'row' | 'column';

export interface Leaf { readonly kind: 'leaf'; readonly id: number; readonly session: string | null }
export interface Split {
  readonly kind: 'split';
  readonly id: number;
  readonly dir: SplitDir;
  /** Share of the space given to `a`. */
  readonly ratio: number;
  readonly a: Node;
  readonly b: Node;
}
export type Node = Leaf | Split;

export interface PaneState {
  readonly version: 2;
  readonly root: Node;
  /** Id of the focused leaf. */
  readonly focused: number;
  /** Next unused node id. */
  readonly nextId: number;
}

export const MAX_LEAVES = 64;
export const MAX_DEPTH = MAX_LEAVES - 1; // a chain of splits can never exceed the leaf cap anyway
export const MIN_RATIO = 0.15;
export const MAX_RATIO = 0.85;
export const DIVIDER_PX = 6;
/** The view refuses a split that would leave a pane smaller than this (tmux: "no space for new pane"). */
export const MIN_PANE_WIDTH_PX = 260;
export const MIN_PANE_HEIGHT_PX = 110;

const idPattern = /^[0-9a-f]{8}$/;

export function defaultState(): PaneState {
  return { version: 2, root: { kind: 'leaf', id: 1, session: null }, focused: 1, nextId: 2 };
}

export function clampRatio(value: number): number {
  if (!Number.isFinite(value)) return 0.5;
  return Math.min(MAX_RATIO, Math.max(MIN_RATIO, value));
}

/** The divider position under the pointer: `pos` and `start` are in px along the axis, `size` the container length. */
export function ratioFromPointer(pos: number, start: number, size: number): number {
  return size > 0 ? clampRatio((pos - start) / size) : 0.5;
}

// ---- reading the tree --------------------------------------------------------------------

/** Leaves left-to-right / top-to-bottom (the order Alt+1..9 count in). */
export function leaves(node: Node): Leaf[] {
  return node.kind === 'leaf' ? [node] : [...leaves(node.a), ...leaves(node.b)];
}
export function leafCount(state: PaneState): number { return leaves(state.root).length; }
export function leafById(state: PaneState, id: number): Leaf | undefined { return leaves(state.root).find(l => l.id === id); }
export function focusedSession(state: PaneState): string | null { return leafById(state, state.focused)?.session ?? null; }
export function leafOf(state: PaneState, session: string): number | null {
  return leaves(state.root).find(l => l.session === session)?.id ?? null;
}
export function depth(node: Node): number { return node.kind === 'leaf' ? 0 : 1 + Math.max(depth(node.a), depth(node.b)); }

/** Normalised rectangles (0..1) of every leaf; used for spatial focus movement. */
export interface Rect { readonly x: number; readonly y: number; readonly w: number; readonly h: number }
export function rects(node: Node, at: Rect = { x: 0, y: 0, w: 1, h: 1 }, out: Map<number, Rect> = new Map()): Map<number, Rect> {
  if (node.kind === 'leaf') { out.set(node.id, at); return out; }
  if (node.dir === 'row') {
    const aw = at.w * node.ratio;
    rects(node.a, { ...at, w: aw }, out);
    rects(node.b, { x: at.x + aw, y: at.y, w: at.w - aw, h: at.h }, out);
  } else {
    const ah = at.h * node.ratio;
    rects(node.a, { ...at, h: ah }, out);
    rects(node.b, { x: at.x, y: at.y + ah, w: at.w, h: at.h - ah }, out);
  }
  return out;
}

/** Every split with the normalised region it divides; the view draws one divider per entry. */
export function splitRegions(node: Node, at: Rect = { x: 0, y: 0, w: 1, h: 1 }, out: { split: Split; at: Rect }[] = []): { split: Split; at: Rect }[] {
  if (node.kind === 'leaf') return out;
  out.push({ split: node, at });
  if (node.dir === 'row') {
    const aw = at.w * node.ratio;
    splitRegions(node.a, { ...at, w: aw }, out);
    splitRegions(node.b, { x: at.x + aw, y: at.y, w: at.w - aw, h: at.h }, out);
  } else {
    const ah = at.h * node.ratio;
    splitRegions(node.a, { ...at, h: ah }, out);
    splitRegions(node.b, { x: at.x, y: at.y + ah, w: at.w, h: at.h - ah }, out);
  }
  return out;
}

/** The ratio a pointer at normalised window position (`px`, `py`, 0..1) means for a split over `at`. */
export function ratioInRegion(px: number, py: number, dir: SplitDir, at: Rect): number {
  const size = dir === 'row' ? at.w : at.h;
  const pos = dir === 'row' ? px - at.x : py - at.y;
  return size > 0 ? clampRatio(pos / size) : 0.5;
}

const EPS = 1e-9;
/** The pane next to `from` in `dir`, or `from` itself at an edge. */
export function neighbor(state: PaneState, from: number, dir: Direction): number {
  const map = rects(state.root);
  const me = map.get(from);
  if (!me) return from;
  let best = from;
  let bestGap = Infinity;
  let bestCenter = Infinity;
  for (const [id, r] of map) {
    if (id === from) continue;
    // Strictly on that side, and overlapping `me` along the other axis.
    const gap = dir === 'right' ? r.x - (me.x + me.w) : dir === 'left' ? me.x - (r.x + r.w)
      : dir === 'down' ? r.y - (me.y + me.h) : me.y - (r.y + r.h);
    if (gap < -EPS) continue;
    const overlap = dir === 'left' || dir === 'right'
      ? Math.min(me.y + me.h, r.y + r.h) - Math.max(me.y, r.y)
      : Math.min(me.x + me.w, r.x + r.w) - Math.max(me.x, r.x);
    if (overlap <= EPS) continue;
    const center = dir === 'left' || dir === 'right'
      ? Math.abs((r.y + r.h / 2) - (me.y + me.h / 2)) : Math.abs((r.x + r.w / 2) - (me.x + me.w / 2));
    if (gap < bestGap - EPS || (Math.abs(gap - bestGap) <= EPS && center < bestCenter)) { best = id; bestGap = gap; bestCenter = center; }
  }
  return best;
}

// ---- changing the tree -------------------------------------------------------------------

function mapNode(node: Node, f: (n: Node) => Node | null): Node {
  const replaced = f(node);
  if (replaced) return replaced;
  return node.kind === 'leaf' ? node : { ...node, a: mapNode(node.a, f), b: mapNode(node.b, f) };
}

/** A terminal id taken from a drag payload; anything that is not a session id is refused. */
export function droppedSession(payload: string | null | undefined): string | null {
  return typeof payload === 'string' && idPattern.test(payload) ? payload : null;
}

/** Show `session` in leaf `leafId`; if it was in another leaf it moves (never shown twice). */
export function assign(state: PaneState, leafId: number, session: string): PaneState {
  if (!leafById(state, leafId)) return state;
  const root = mapNode(state.root, n => n.kind === 'leaf'
    ? { ...n, session: n.id === leafId ? session : n.session === session ? null : n.session }
    : null);
  return { ...state, root, focused: leafId };
}

/** Empty a pane without removing it. */
export function clear(state: PaneState, leafId: number): PaneState {
  const target = leafById(state, leafId);
  if (!target || target.session === null) return state;
  return { ...state, root: mapNode(state.root, n => n.kind === 'leaf' && n.id === leafId ? { ...n, session: null } : null) };
}

export function focus(state: PaneState, leafId: number): PaneState {
  return leafId !== state.focused && leafById(state, leafId) ? { ...state, focused: leafId } : state;
}
export function focusDirection(state: PaneState, dir: Direction): PaneState {
  return focus(state, neighbor(state, state.focused, dir));
}
/** Focus the n-th pane (0-based, reading order); out of range changes nothing. */
export function focusIndex(state: PaneState, index: number): PaneState {
  const target = leaves(state.root)[index];
  return target ? focus(state, target.id) : state;
}

/** Split a pane: the original keeps the first half, an empty pane takes the second and gets focus. */
export function split(state: PaneState, leafId: number, dir: SplitDir): PaneState {
  if (!leafById(state, leafId) || leafCount(state) >= MAX_LEAVES) return state;
  const fresh: Leaf = { kind: 'leaf', id: state.nextId + 1, session: null };
  const root = mapNode(state.root, n => n.kind === 'leaf' && n.id === leafId
    ? { kind: 'split', id: state.nextId, dir, ratio: 0.5, a: n, b: fresh } : null);
  if (depth(root) > MAX_DEPTH) return state;
  return { ...state, root, focused: fresh.id, nextId: state.nextId + 2 };
}

/** Remove a pane; its sibling takes the space. The last pane is only emptied. Never ends a terminal. */
export function close(state: PaneState, leafId: number): PaneState {
  if (!leafById(state, leafId)) return state;
  if (state.root.kind === 'leaf') return clear(state, leafId);
  const strip = (n: Node): Node => {
    if (n.kind === 'leaf') return n;
    if (n.a.kind === 'leaf' && n.a.id === leafId) return n.b;
    if (n.b.kind === 'leaf' && n.b.id === leafId) return n.a;
    return { ...n, a: strip(n.a), b: strip(n.b) };
  };
  const root = strip(state.root);
  const stillThere = leaves(root).some(l => l.id === state.focused);
  return { ...state, root, focused: stillThere ? state.focused : leaves(root)[0].id };
}

export function setRatio(state: PaneState, splitId: number, value: number): PaneState {
  const r = clampRatio(value);
  return { ...state, root: mapNode(state.root, n => n.kind === 'split' && n.id === splitId ? { ...n, ratio: r } : null) };
}

/** The splits from the root down to leaf `leafId`, each with the side (`a` or `b`) the leaf is on. */
function pathTo(node: Node, leafId: number): { split: Split; side: 'a' | 'b' }[] | null {
  if (node.kind === 'leaf') return node.id === leafId ? [] : null;
  const inA = pathTo(node.a, leafId);
  if (inA) return [{ split: node, side: 'a' }, ...inA];
  const inB = pathTo(node.b, leafId);
  return inB ? [{ split: node, side: 'b' }, ...inB] : null;
}

/** Keyboard resize (tmux prefix+Ctrl+arrow): move the focused pane's nearest divider on the `dir` side by `step`;
 *  a pane with no divider on that side moves the one on its other side the same way (at a window edge the pane can
 *  only shrink). A pane with no divider of that orientation changes nothing. */
export function resizeFocused(state: PaneState, dir: Direction, step = 0.05): PaneState {
  const path = pathTo(state.root, state.focused);
  if (!path) return state;
  const orientation: SplitDir = dir === 'left' || dir === 'right' ? 'row' : 'column';
  const towardB = dir === 'right' || dir === 'down';
  // Innermost first: the nearest divider is the deepest split of that orientation.
  const candidates = path.filter(p => p.split.dir === orientation).reverse();
  const pick = candidates.find(p => p.side === (towardB ? 'a' : 'b')) ?? candidates[0];
  return pick ? setRatio(state, pick.split.id, pick.split.ratio + (towardB ? step : -step)) : state;
}

/** tmux's zoom (prefix+z): while a pane is zoomed it alone fills the view; the arrangement itself is unchanged. */
export function zoomedRects(all: ReadonlyMap<number, Rect>, zoomed: number | null): Map<number, Rect> {
  if (zoomed === null || !all.has(zoomed)) return new Map(all);
  return new Map([[zoomed, { x: 0, y: 0, w: 1, h: 1 }]]);
}

/** Drop sessions that no longer exist (panes stay, empty). */
export function reconcile(state: PaneState, existing: ReadonlySet<string>): PaneState {
  if (leaves(state.root).every(l => l.session === null || existing.has(l.session))) return state;
  return { ...state, root: mapNode(state.root, n => n.kind === 'leaf' && n.session !== null && !existing.has(n.session) ? { ...n, session: null } : null) };
}

/** Where a newly created session should go: the focused pane if empty, else the first empty one, else the focused pane. */
export function leafForNew(state: PaneState): number {
  const all = leaves(state.root);
  if (all.find(l => l.id === state.focused)?.session === null) return state.focused;
  return all.find(l => l.session === null)?.id ?? state.focused;
}

/** tmux "tiled": one pane per session in a balanced grid (columns ~ sqrt(n)); the first stays focused. Panes beyond MAX_LEAVES are left out. */
export function tile(state: PaneState, sessions: readonly string[]): PaneState {
  const ids = sessions.slice(0, MAX_LEAVES);
  if (ids.length === 0) return state;
  let next = 1;
  const grid = (items: readonly string[], dir: SplitDir): Node => {
    if (items.length === 1) return { kind: 'leaf', id: next++, session: items[0] };
    const half = Math.ceil(items.length / 2);
    const id = next++;
    return { kind: 'split', id, dir, ratio: half / items.length, a: grid(items.slice(0, half), dir), b: grid(items.slice(half), dir) };
  };
  const columns = Math.ceil(Math.sqrt(ids.length));
  // Spread the terminals over the rows as evenly as possible (row sizes differ by at most one).
  const rowCount = Math.ceil(ids.length / columns);
  const rows: string[][] = [];
  for (let r = 0, at = 0; r < rowCount; r++) {
    const size = Math.floor(ids.length / rowCount) + (r < ids.length % rowCount ? 1 : 0);
    rows.push(ids.slice(at, at + size));
    at += size;
  }
  const stack = (list: string[][]): Node => {
    if (list.length === 1) return grid(list[0], 'row');
    const half = Math.ceil(list.length / 2);
    const id = next++;
    // Rows are equally tall: weight each half by its row count.
    return { kind: 'split', id, dir: 'column', ratio: half / list.length, a: stack(list.slice(0, half)), b: stack(list.slice(half)) };
  };
  const root = stack(rows);
  const firstLeaf = leaves(root)[0];
  return { version: 2, root, focused: firstLeaf.id, nextId: next };
}

// ---- layout presets (tmux's prefix+Space cycle) -------------------------------------------

/** The five window layouts tmux offers. */
export type Preset = 'even-horizontal' | 'even-vertical' | 'main-horizontal' | 'main-vertical' | 'tiled';
/** tmux's cycle order (a new window starts at `even-horizontal`). */
export const PRESETS: readonly Preset[] = ['even-horizontal', 'even-vertical', 'main-horizontal', 'main-vertical', 'tiled'];
/** Share of the window the main pane of the `main-*` layouts gets. */
export const MAIN_RATIO = 0.6;

interface Alloc { next: number }

/** A balanced tree over `items` along `dir` with equal sizes: a split of k puts ceil(k/2) in `a`. */
function evenTree(items: readonly string[], dir: SplitDir, ids: Alloc): Node {
  if (items.length === 1) return { kind: 'leaf', id: ids.next++, session: items[0] };
  const half = Math.ceil(items.length / 2);
  const id = ids.next++;
  return {
    kind: 'split', id, dir, ratio: half / items.length,
    a: evenTree(items.slice(0, half), dir, ids),
    b: evenTree(items.slice(half), dir, ids)
  };
}

/** A state for a freshly built tree, focused on the leaf showing `session` (else the first leaf). */
function presetState(root: Node, nextId: number, session: string | null): PaneState {
  const order = leaves(root);
  const focused = (session === null ? undefined : order.find(l => l.session === session)) ?? order[0];
  return { version: 2, root, focused: focused.id, nextId };
}

/**
 * tmux's <prefix>+Space layouts applied to the panes that currently hold a session: every session is
 * kept exactly once, in the order it reads today, and empty panes are dropped (a window with no
 * session at all is left untouched). The focused session stays focused, so the leaf ids are rebuilt.
 */
export function applyPreset(state: PaneState, preset: Preset): PaneState {
  if (!PRESETS.includes(preset)) return state;
  const all = leaves(state.root);
  const want = all.find(l => l.id === state.focused)?.session ?? null;
  const sessions = all.map(l => l.session).filter((s): s is string => s !== null).slice(0, MAX_LEAVES);
  if (sessions.length === 0) return state;
  if (preset === 'tiled') {
    const out = tile(state, sessions);
    const target = want === null ? null : leafOf(out, want);
    return target === null ? out : focus(out, target);
  }
  const ids: Alloc = { next: 1 };
  const main = want !== null && sessions.includes(want) ? want : sessions[0];
  const rest = sessions.filter(s => s !== main);
  let root: Node;
  if (preset === 'even-horizontal' || preset === 'even-vertical') {
    root = evenTree(sessions, preset === 'even-horizontal' ? 'row' : 'column', ids);
  } else if (rest.length === 0) {
    root = { kind: 'leaf', id: ids.next++, session: main };
  } else {
    // The main pane takes the left (vertical) or top (horizontal) side; the others share the rest evenly.
    const vertical = preset === 'main-vertical';
    root = {
      kind: 'split', id: ids.next++, dir: vertical ? 'row' : 'column', ratio: MAIN_RATIO,
      a: { kind: 'leaf', id: ids.next++, session: main },
      b: evenTree(rest, vertical ? 'column' : 'row', ids)
    };
  }
  return presetState(root, ids.next, want);
}

/** The preset <prefix>+Space moves to: forward through `PRESETS`, wrapping, `null` (or junk) starting at the first. */
export function nextPreset(current: Preset | null): Preset {
  const at = current === null ? -1 : PRESETS.indexOf(current);
  return PRESETS[(at + 1) % PRESETS.length];
}

// ---- storage -----------------------------------------------------------------------------

function parseNode(value: unknown, seen: { ids: Set<number>; sessions: Set<string>; leaves: number }, level: number): Node | null {
  if (typeof value !== 'object' || value === null || level > MAX_DEPTH) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.id !== 'number' || !Number.isInteger(v.id) || v.id < 1 || seen.ids.has(v.id)) return null;
  seen.ids.add(v.id);
  if (v.kind === 'leaf') {
    if (++seen.leaves > MAX_LEAVES) return null;
    let session: string | null = null;
    if (typeof v.session === 'string' && idPattern.test(v.session) && !seen.sessions.has(v.session)) { session = v.session; seen.sessions.add(session); }
    return { kind: 'leaf', id: v.id, session };
  }
  if (v.kind !== 'split' || (v.dir !== 'row' && v.dir !== 'column')) return null;
  const a = parseNode(v.a, seen, level + 1);
  const b = a ? parseNode(v.b, seen, level + 1) : null;
  if (!a || !b) return null;
  return { kind: 'split', id: v.id, dir: v.dir, ratio: typeof v.ratio === 'number' ? clampRatio(v.ratio) : 0.5, a, b };
}

/** Pane arrangement saved by builds with a fixed 1/2/2/4-pane layout (slots + two ratios). */
function fromLegacy(v: Record<string, unknown>): PaneState {
  const layouts = { single: 1, columns: 2, rows: 2, grid: 4 } as const;
  const kind = v.layout as keyof typeof layouts;
  if (!(kind in layouts) || !Array.isArray(v.slots) || v.slots.length !== 4) return defaultState();
  const sessions = v.slots.map(s => (typeof s === 'string' && idPattern.test(s) ? s : null));
  const unique = sessions.map((s, i) => (s !== null && sessions.indexOf(s) !== i ? null : s));
  const ratio = (x: unknown) => (typeof x === 'number' ? clampRatio(x) : 0.5);
  const col = ratio(v.colRatio), row = ratio(v.rowRatio);
  let n = 0;
  const leaf = (i: number): Leaf => ({ kind: 'leaf', id: ++n, session: unique[i] });
  const split = (dir: SplitDir, r: number, a: Node, b: Node): Node => ({ kind: 'split', id: ++n, dir, ratio: r, a, b });
  let root: Node;
  if (kind === 'single') root = leaf(0);
  else if (kind === 'columns') root = split('row', col, leaf(0), leaf(1));
  else if (kind === 'rows') root = split('column', row, leaf(0), leaf(1));
  else root = split('column', row, split('row', col, leaf(0), leaf(1)), split('row', col, leaf(2), leaf(3)));
  const order = leaves(root);
  const wanted = typeof v.focused === 'number' && Number.isInteger(v.focused) ? v.focused : 0;
  // Legacy leaves were numbered in reading order, so the legacy focus index is the n-th leaf.
  const focused = order[Math.max(0, Math.min(wanted, order.length - 1))].id;
  return { version: 2, root, focused, nextId: n + 1 };
}

/** Validate untrusted stored state (native file or localStorage); anything odd yields the default. Accepts the older fixed-layout format. */
export function parse(value: unknown): PaneState {
  if (typeof value !== 'object' || value === null) return defaultState();
  const v = value as Record<string, unknown>;
  if (v.version !== 2) return fromLegacy(v);
  const seen = { ids: new Set<number>(), sessions: new Set<string>(), leaves: 0 };
  const root = parseNode(v.root, seen, 0);
  if (!root) return defaultState();
  const all = leaves(root);
  const focused = typeof v.focused === 'number' && all.some(l => l.id === v.focused) ? v.focused : all[0].id;
  return { version: 2, root, focused, nextId: Math.max(...seen.ids) + 1 };
}
