// Copy mode (tmux prefix+[): move a cursor through the scrollback with the keyboard, mark a selection, copy it.
// Pure: positions and key handling only; TerminalPane.svelte draws the cursor/selection with the emulator's own
// selection and scrolls it into view. Rows are absolute buffer lines (0 = the oldest line kept).

import type { KeyInput } from './keymap.js';

export interface Point { readonly row: number; readonly col: number }
export interface CopyState { readonly cursor: Point; readonly anchor: Point | null }
/** The buffer the cursor moves in: `lines` in total, `cols` wide, `rows` visible at once. */
export interface Extent { readonly lines: number; readonly cols: number; readonly rows: number }

export type CopyEffect = 'none' | 'copy' | 'exit' | 'search' | 'next-match' | 'previous-match';
export interface CopyStep { readonly state: CopyState; readonly effect: CopyEffect }

const clamp = (n: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, n));

/** Enter copy mode with the cursor where the terminal's cursor is. */
export function startCopy(at: Point, extent: Extent): CopyState {
  return { cursor: within(at, extent), anchor: null };
}

export function within(p: Point, e: Extent): Point {
  return { row: clamp(Math.trunc(p.row), 0, Math.max(0, e.lines - 1)), col: clamp(Math.trunc(p.col), 0, Math.max(0, e.cols - 1)) };
}

/** One key in copy mode. Every key is the mode's: nothing reaches the shell while it is on. */
export function copyKey(s: CopyState, k: KeyInput, e: Extent): CopyStep {
  const move = (row: number, col: number): CopyStep => ({ state: { ...s, cursor: within({ row, col }, e) }, effect: 'none' });
  const { row, col } = s.cursor;
  const page = Math.max(1, e.rows - 1);
  if (k.ctrlKey && (k.key === 'u' || k.key === 'U')) return move(row - Math.max(1, Math.floor(e.rows / 2)), col);
  if (k.ctrlKey && (k.key === 'd' || k.key === 'D')) return move(row + Math.max(1, Math.floor(e.rows / 2)), col);
  if (k.ctrlKey || k.altKey || k.metaKey) return { state: s, effect: 'none' };
  switch (k.key) {
    case 'ArrowLeft': case 'h': return move(row, col - 1);
    case 'ArrowRight': case 'l': return move(row, col + 1);
    case 'ArrowUp': case 'k': return move(row - 1, col);
    case 'ArrowDown': case 'j': return move(row + 1, col);
    case 'PageUp': return move(row - page, col);
    case 'PageDown': return move(row + page, col);
    case 'Home': case '0': return move(row, 0);
    case 'End': case '$': return move(row, e.cols - 1);
    case 'g': return move(0, 0);
    case 'G': return move(e.lines - 1, 0);
    case ' ': case 'v': return { state: { ...s, anchor: s.anchor === null ? s.cursor : null }, effect: 'none' };
    case 'Enter': case 'y': return { state: s, effect: s.anchor === null ? 'none' : 'copy' };
    case 'Escape': case 'q': return { state: s, effect: 'exit' };
    case '/': return { state: s, effect: 'search' };
    case 'n': return { state: s, effect: 'next-match' };
    case 'N': return { state: s, effect: 'previous-match' };
    default: return { state: s, effect: 'none' };
  }
}

/** What to highlight: the selection from anchor to cursor (both ends included, in reading order), or just the cursor
 *  cell. As the emulator's `select(col, row, length)` wants it: a run of `length` cells that wraps at `cols`. */
export function highlight(s: CopyState, cols: number): { readonly col: number; readonly row: number; readonly length: number } {
  const a = s.anchor ?? s.cursor;
  const b = s.cursor;
  const [first, last] = a.row < b.row || (a.row === b.row && a.col <= b.col) ? [a, b] : [b, a];
  return { col: first.col, row: first.row, length: (last.row - first.row) * cols + (last.col - first.col) + 1 };
}

/** The first visible line that keeps `row` on screen, moving the view as little as possible. */
export function scrollFor(row: number, viewportTop: number, rows: number): number {
  if (row < viewportTop) return row;
  if (row >= viewportTop + rows) return row - rows + 1;
  return viewportTop;
}

/** The status line shown while copy mode is on. */
export function copyStatus(s: CopyState, e: Extent): string {
  const where = `line ${s.cursor.row + 1} of ${e.lines}, column ${s.cursor.col + 1}`;
  return s.anchor === null
    ? `COPY · ${where} · arrows or h j k l move · Space or v starts a selection · / search · q leaves`
    : `COPY · selecting · ${where} · Enter or y copies · Space or v cancels the selection · q leaves`;
}
