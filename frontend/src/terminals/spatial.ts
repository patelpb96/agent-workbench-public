// "Which window is next in that direction" for free-form windows (the Canvas), where windows can sit anywhere and
// overlap: pure geometry, used by prefix+arrow there. The tmux panes have their own exact version in panes.ts.

export interface Box { readonly x: number; readonly y: number; readonly w: number; readonly h: number }
export type Direction = 'left' | 'right' | 'up' | 'down';

const STEP: Readonly<Record<Direction, readonly [number, number]>> = { left: [-1, 0], right: [1, 0], up: [0, -1], down: [0, 1] };

/** The index in `others` of the window nearest to `from` in `dir`, or -1. Only windows whose centre lies on that side
 *  count; distance along the direction counts once and sideways distance twice, so a window straight ahead wins over a
 *  closer one far off to the side. Ties go to the earlier window (stable). */
export function nearestInDirection(from: Box, others: readonly Box[], dir: Direction): number {
  const [dx, dy] = STEP[dir];
  const fx = from.x + from.w / 2, fy = from.y + from.h / 2;
  let best = -1, bestScore = Infinity;
  others.forEach((b, i) => {
    const x = b.x + b.w / 2, y = b.y + b.h / 2;
    const along = (x - fx) * dx + (y - fy) * dy;
    if (along <= 0.5) return;
    const across = Math.abs((x - fx) * dy) + Math.abs((y - fy) * dx);
    const score = along + 2 * across;
    if (score < bestScore) { best = i; bestScore = score; }
  });
  return best;
}

/** Tops this close count as one row (windows placed by hand are rarely aligned to the pixel). */
export const ROW_BAND = 24;

/** Reading order (rows top to bottom, then left to right, by top-left corner): the order prefix+o cycles in. Windows
 *  join the row of the top-most window above them within ROW_BAND, so a hand-placed row reads left to right. */
export function readingOrder(boxes: readonly Box[]): number[] {
  const byTop = boxes.map((b, i) => i).sort((a, b) => boxes[a].y - boxes[b].y || a - b);
  const row = new Array<number>(boxes.length);
  let current = -1, rowTop = -Infinity;
  for (const i of byTop) {
    if (boxes[i].y - rowTop > ROW_BAND) { current++; rowTop = boxes[i].y; }
    row[i] = current;
  }
  return boxes.map((b, i) => i).sort((a, b) => row[a] - row[b] || boxes[a].x - boxes[b].x || a - b);
}
