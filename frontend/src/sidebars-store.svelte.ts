// The sidebars' stage and dock, which side the module list is on and which panel is open, remembered in this window's
// browser storage (a convenience: if storage is unavailable the bars simply start in their default places).
import { collapse, defaultArrangement, defaultBars, dockTo, dropOn, expand, parseArrangement, parseBars, togglePanel, type Arrangement, type BarName, type Bars, type Dock, type PanelName } from './sidebars.js';

const KEY = 'aw.sidebars';
const ARRANGEMENT_KEY = 'aw.arrangement';

function read<T>(key: string, parse: (raw: unknown) => T, fallback: T): T {
  try {
    const raw = typeof localStorage === 'undefined' ? null : localStorage.getItem(key);
    return raw === null ? fallback : parse(JSON.parse(raw));
  } catch {
    return fallback;
  }
}
function write(key: string, value: unknown): void {
  try {
    if (typeof localStorage !== 'undefined') localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Not remembered; the layout still changes.
  }
}

class SidebarStore {
  bars = $state.raw<Bars>(read(KEY, parseBars, defaultBars));
  arrangement = $state.raw<Arrangement>(read(ARRANGEMENT_KEY, parseArrangement, defaultArrangement));

  private set(next: Bars): void {
    this.bars = next;
    write(KEY, next);
  }
  private arrange(next: Arrangement): void {
    this.arrangement = next;
    write(ARRANGEMENT_KEY, next);
  }
  collapse(name: BarName): void {
    this.set({ ...this.bars, [name]: collapse(this.bars[name]) });
  }
  expand(name: BarName): void {
    this.set({ ...this.bars, [name]: expand(this.bars[name]) });
  }
  dock(name: BarName, dock: Dock): void {
    this.set({ ...this.bars, [name]: dockTo(this.bars[name], dock) });
  }
  /** A bar was dragged and dropped on `zone`. */
  drop(name: BarName, zone: Dock): void {
    this.set({ ...this.bars, [name]: dropOn(this.bars[name], zone) });
  }
  /** Put the module list on the other side (the panel moves to the side it left). */
  swap(): void {
    this.arrange({ ...this.arrangement, side: this.arrangement.side === 'left' ? 'right' : 'left' });
  }
  /** A panel button in the module list: open it, switch to it, or close it if it is already showing. */
  togglePanel(name: PanelName): void {
    this.arrange({ ...this.arrangement, panel: togglePanel(this.arrangement.panel, name) });
  }
  openPanel(name: PanelName): void {
    if (this.arrangement.panel !== name) this.arrange({ ...this.arrangement, panel: name });
  }
}

export const sidebars = new SidebarStore();
