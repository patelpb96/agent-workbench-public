// What the Inspector is showing, kept at module level so it survives the view being unmounted when another rail
// item is chosen: tiles or the map, and which session is selected (not saved between runs), and how tall the map is
// (dragged with the bar under it; remembered in this window's storage).
export type InspectorMode = 'tiles' | 'map';

const MAP_HEIGHT_KEY = 'aw.inspector.map-height';
export const MAP_HEIGHT_DEFAULT = 320;
export const MAP_HEIGHT_MIN = 120;

let mode = $state<InspectorMode>('tiles');
let selected = $state<string | null>(null);
let mapHeight = $state(readMapHeight());

function readMapHeight(): number {
  try {
    const n = Number(localStorage.getItem(MAP_HEIGHT_KEY));
    return Number.isFinite(n) && n >= MAP_HEIGHT_MIN && n <= 4000 ? Math.round(n) : MAP_HEIGHT_DEFAULT;
  } catch {
    return MAP_HEIGHT_DEFAULT;
  }
}

export const inspectorView = {
  get mode(): InspectorMode { return mode; },
  set mode(value: InspectorMode) { mode = value; },
  get selected(): string | null { return selected; },
  set selected(value: string | null) { selected = value; },
  /** The map's height in px (the view keeps it within the room it has). */
  get mapHeight(): number { return mapHeight; },
  set mapHeight(value: number) {
    mapHeight = Math.max(MAP_HEIGHT_MIN, Math.round(value));
    try { localStorage.setItem(MAP_HEIGHT_KEY, String(mapHeight)); } catch { /* not remembered */ }
  }
};
