// The two sidebars (the module list on the left, "Sessions" on the right) can be collapsed the way session windows
// can (full, compact, flat) and, once collapsed, docked to the top or bottom edge like a taskbar. This module is the
// pure part: the state rules and the grid they produce. Remembering the state is in sidebars-store.svelte.ts.

export type Stage = 'full' | 'compact' | 'flat';
export type Dock = 'side' | 'top' | 'bottom';
export interface Bar { readonly stage: Stage; readonly dock: Dock }
export type BarName = 'rail' | 'aside';
export type Bars = Readonly<Record<BarName, Bar>>;

export const defaultBars: Bars = { rail: { stage: 'full', dock: 'side' }, aside: { stage: 'full', dock: 'side' } };

/** Thickness of a collapsed bar, in pixels (width on a side, height when docked top or bottom). */
export const thickness: Readonly<Record<Exclude<Stage, 'full'>, number>> = { compact: 44, flat: 24 };
/** Width of the module list when full. */
export const railWidth = 168;

const stages: readonly Stage[] = ['full', 'compact', 'flat'];
const docks: readonly Dock[] = ['side', 'top', 'bottom'];

/** A full bar is always on its side: only a collapsed bar can be docked to the top or bottom. */
export function normalize(bar: Bar): Bar {
  return bar.stage === 'full' ? { stage: 'full', dock: 'side' } : bar;
}

/** One step toward collapsed: full, compact, flat (and no further). */
export function collapse(bar: Bar): Bar {
  const next = stages[Math.min(stages.indexOf(bar.stage) + 1, stages.length - 1)] ?? 'flat';
  return normalize({ stage: next, dock: bar.dock });
}

/** One step toward full. Reaching full returns the bar to its side. */
export function expand(bar: Bar): Bar {
  const next = stages[Math.max(stages.indexOf(bar.stage) - 1, 0)] ?? 'full';
  return normalize({ stage: next, dock: bar.dock });
}

/** Dock a collapsed bar; a full bar stays where it is. */
export function dockTo(bar: Bar, dock: Dock): Bar {
  return bar.stage === 'full' ? bar : { stage: bar.stage, dock };
}

/**
 * What dropping a dragged bar on `zone` does. A collapsed bar docks there; a full bar dropped on the top or bottom
 * edge collapses to compact first (only a collapsed bar can be docked); a full bar dropped back on its side stays.
 */
export function dropOn(bar: Bar, zone: Dock): Bar {
  if (zone === 'side') return dockTo(bar, 'side');
  return dockTo(bar.stage === 'full' ? collapse(bar) : bar, zone);
}

/** Where a bar being dragged would land: within `edge` px of the top or bottom of the area, or back on its side. */
export function zoneAt(y: number, area: { top: number; bottom: number }, edge = 56): Dock {
  if (y <= area.top + edge) return 'top';
  if (y >= area.bottom - edge) return 'bottom';
  return 'side';
}

/** Read stored bars; anything unexpected becomes the default for that bar. */
export function parseBars(raw: unknown): Bars {
  const one = (v: unknown, fallback: Bar): Bar => {
    if (typeof v !== 'object' || v === null) return fallback;
    const { stage, dock } = v as { stage?: unknown; dock?: unknown };
    if (!stages.includes(stage as Stage) || !docks.includes(dock as Dock)) return fallback;
    return normalize({ stage: stage as Stage, dock: dock as Dock });
  };
  const r = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : {};
  return { rail: one(r.rail, defaultBars.rail), aside: one(r.aside, defaultBars.aside) };
}

export interface Place {
  /** CSS `grid-area` value: row start / column start / row end / column end. */
  readonly area: string;
  /** Whether the bar is laid out as a horizontal strip (docked top or bottom). */
  readonly horizontal: boolean;
}
export interface Grid {
  readonly columns: string;
  readonly rows: string;
  readonly rail: Place;
  readonly aside: Place;
  readonly main: string;
  readonly splitter: string | null;
}

/** Which side the module list is on; the panel (Sessions or Files) is on the other one. */
export type Side = 'left' | 'right';
/** What the panel beside the workspace shows, or null when it is closed (docs/design/sidebar-restructure.md). */
export type PanelName = 'sessions' | 'files';
export interface Arrangement { readonly side: Side; readonly panel: PanelName | null }
export const defaultArrangement: Arrangement = { side: 'left', panel: 'files' };

export function parseArrangement(raw: unknown): Arrangement {
  const r = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : {};
  const side: Side = r.side === 'right' ? 'right' : r.side === 'left' ? 'left' : defaultArrangement.side;
  const panel: PanelName | null = r.panel === 'sessions' || r.panel === 'files' ? r.panel : r.panel === null ? null : defaultArrangement.panel;
  return { side, panel };
}

/** A module-list button for a panel: opens it, switches to it, or (when it is already showing) closes it. */
export function togglePanel(current: PanelName | null, clicked: PanelName): PanelName | null {
  return current === clicked ? null : clicked;
}

/**
 * The shell's grid. Four columns (module list, workspace, splitter, panel; mirrored when the module list is on the right)
 * and one row for the workspace, plus a row above or below for every bar docked there. A bar that is not on its side
 * takes no width in its column; a closed panel takes no room at all.
 */
export function gridFor(bars: Bars, arrangement: Arrangement = defaultArrangement): Grid {
  const rail = normalize(bars.rail);
  const aside = normalize(bars.aside);
  const panelOpen = arrangement.panel !== null;
  const tops: BarName[] = [];
  const bottoms: BarName[] = [];
  for (const [name, bar] of [['rail', rail], ['aside', aside]] as const) {
    if (name === 'aside' && !panelOpen) continue;
    if (bar.dock === 'top') tops.push(name);
    if (bar.dock === 'bottom') bottoms.push(name);
  }
  const main = tops.length + 1;
  const railCol = rail.dock !== 'side' ? '0px' : rail.stage === 'full' ? `${railWidth}px` : `${thickness[rail.stage]}px`;
  const asideCol = !panelOpen || aside.dock !== 'side' ? '0px' : aside.stage === 'full' ? 'minmax(240px,var(--side-width))' : `${thickness[aside.stage]}px`;
  const split = panelOpen && aside.dock === 'side' && aside.stage === 'full' ? '5px' : '0px';
  const mirrored = arrangement.side === 'right';
  // Column of each part, counted from the left: 1 module list, 2 workspace, 3 splitter, 4 panel (or the mirror image).
  const col = (n: 1 | 2 | 3 | 4): number => (mirrored ? 5 - n : n);
  const barOf = (name: BarName): Bar => (name === 'rail' ? rail : aside);
  // A bar docked on an edge is collapsed, so it has a thickness; the fallback only keeps the types honest.
  const rowSize = (name: BarName): string => {
    const stage = barOf(name).stage;
    return `${thickness[stage === 'full' ? 'compact' : stage]}px`;
  };
  const rows = [...tops.map(rowSize), 'minmax(0,1fr)', ...bottoms.map(rowSize)].join(' ');
  const place = (name: BarName, bar: Bar): Place => {
    if (bar.dock === 'side') { const c = col(name === 'rail' ? 1 : 4); return { area: `${main} / ${c} / ${main + 1} / ${c + 1}`, horizontal: false }; }
    const row = bar.dock === 'top' ? tops.indexOf(name) + 1 : main + 1 + bottoms.indexOf(name);
    return { area: `${row} / 1 / ${row + 1} / 5`, horizontal: true };
  };
  const columns = [railCol, 'minmax(300px,1fr)', split, asideCol];
  return {
    columns: (mirrored ? columns.reverse() : columns).join(' '),
    rows,
    rail: place('rail', rail),
    aside: place('aside', aside),
    main: `${main} / ${col(2)} / ${main + 1} / ${col(2) + 1}`,
    splitter: split === '0px' ? null : `${main} / ${col(3)} / ${main + 1} / ${col(3) + 1}`
  };
}
