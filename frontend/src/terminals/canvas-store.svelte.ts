// Shared, reactive state of the free-form terminals on the Canvas (floating windows + session links)
// and the terminal list they need. One instance serves the canvas, the session panels and the Terminals
// tab (which must not also show a terminal that is on the canvas). Persisted through the native app-data file.
import type { TerminalInfo } from './client.js';
import { defaultState, parse, reconcile, type CanvasTerminals } from './floating.js';
import { loadSavedWindows, saveWindows, terminalClient } from './tauri.js';

let state = $state.raw<CanvasTerminals>(defaultState());
let terminals = $state.raw<TerminalInfo[]>([]);
let touched = false;
let listed = $state(false);
let lastSaved: CanvasTerminals | null = null;

export const canvasTerminals = {
  get state(): CanvasTerminals { return state; },
  get terminals(): readonly TerminalInfo[] { return terminals; },
  /** True once a terminal listing has succeeded (so a missing terminal really is missing). */
  get listed(): boolean { return listed; },
  /** Transient change while dragging or resizing: shown, not saved. */
  preview(next: CanvasTerminals): void { state = next; },
  /** A finished change: shown and saved. */
  commit(next: CanvasTerminals): void {
    touched = true;
    state = next;
    if (next === lastSaved) return; // nothing new to write (e.g. focusing the window that is already on top)
    lastSaved = next;
    saveWindows(next);
  },
  /** Read the saved arrangement once; anything the user did before it arrived wins. */
  async load(): Promise<void> {
    const saved = await loadSavedWindows();
    if (!touched && saved !== null && saved !== undefined) state = parse(saved);
  },
  /** Refresh the terminal list; only after a successful listing are vanished terminals dropped from the canvas. */
  async refresh(): Promise<void> {
    try {
      terminals = await terminalClient.list();
      listed = true;
      const next = reconcile(state, new Set(terminals.map(t => t.id)));
      if (next !== state) this.commit(next);
    } catch {
      /* the service is unreachable right now: keep what is shown */
    }
  }
};
