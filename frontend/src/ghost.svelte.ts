// See-through mode for the Canvas (docs/design/overlay-harness.md). The mode itself lives in the native layer
// (so a stuck page can never trap the window); this is the page's view of it and its way to ask for a toggle when the
// global hotkey could not be registered.
import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

class GhostStore {
  on = $state(false);
  /** False when another program owns the global hotkey: the page then handles the key itself while it has focus. */
  registered = $state(true);
  hotkey = $state('Ctrl+Alt+G');

  async init(): Promise<void> {
    if (!isTauri()) return;
    try {
      const info = await invoke<{ on?: unknown; registered?: unknown; hotkey?: unknown }>('ghost_info');
      if (typeof info.on === 'boolean') this.on = info.on;
      if (typeof info.registered === 'boolean') this.registered = info.registered;
      if (typeof info.hotkey === 'string' && info.hotkey.length <= 24) this.hotkey = info.hotkey;
    } catch {
      // Older native layer: the mode is simply unavailable.
    }
    try {
      await listen<{ on?: unknown }>('ghost-changed', event => {
        if (typeof event.payload?.on === 'boolean') this.on = event.payload.on;
      });
    } catch {
      // No event channel (a plain browser).
    }
  }

  /** Ctrl+Alt+G pressed in the page while the global hotkey is not registered. */
  async toggle(): Promise<void> {
    try {
      const now = await invoke<unknown>('ghost_toggle');
      if (typeof now === 'boolean') this.on = now;
    } catch {
      // Nothing changed.
    }
  }

  /** Tell the native layer whether the mode applies now and where the interactive areas are (physical pixels). */
  report(effective: boolean, whole: boolean, rects: number[][]): void {
    if (!isTauri()) return;
    void invoke('ghost_report', { effective, whole, rects }).catch(() => undefined);
  }
}

export const ghost = new GhostStore();

/** What stays clickable while see-through: everything except bare background. On the canvas that is its panels and
 *  windows; on the other tabs each view's own parts (the gaps between them, and the view's backdrop, let clicks through). */
export const HIT_SELECTOR = [
  '.titlebar-hot, .titlebar, nav.rail, aside, .statusbar, .splitter, .float-win, section.agent, section.nest, .notice, .canvas-empty',
  // Drop-down lists can reach past the part they belong to (menus and dialogs make the whole window clickable instead).
  '[role="listbox"]',
  '.terminals > :not(.terminal-body), .terminal-body > :not(.panes), .panes > *',
  '.inspector > :not(.ins-body), .ins-body > *',
  '.run-board > *, .usage-overview > *, .event-timeline > *, .workspace-diagnostics > *'
].join(', ');

export function hitRects(root: ParentNode = document): { whole: boolean; rects: number[][] } {
  const scale = window.devicePixelRatio || 1;
  const rects: number[][] = [];
  // The window's own resize border must stay grabbable or the window cannot be resized while it is see-through.
  const edge = 6;
  const w = window.innerWidth;
  const h = window.innerHeight;
  for (const [x, y, width, height] of [[0, 0, w, edge], [0, h - edge, w, edge], [0, 0, edge, h], [w - edge, 0, edge, h]]) {
    rects.push([Math.round(x * scale), Math.round(y * scale), Math.round(width * scale), Math.round(height * scale)]);
  }
  for (const el of root.querySelectorAll<HTMLElement>(HIT_SELECTOR)) {
    // A title bar that has slid away is not there (its box is above the window); one that is showing counts as its full box,
    // not whatever stage its slide animation is at.
    if (el.classList.contains('titlebar')) {
      if (el.classList.contains('autohide') && !el.classList.contains('shown') && !el.matches(':focus-within')) continue;
      rects.push([0, 0, Math.round(w * scale), Math.round(el.offsetHeight * scale)]);
      continue;
    }
    // The canvas stays in the page under the other tabs, hidden: its panels are not there to click.
    if (el.closest('.background-hidden')) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    rects.push([Math.round(r.left * scale), Math.round(r.top * scale), Math.round(r.width * scale), Math.round(r.height * scale)]);
    if (rects.length >= 250) break;
  }
  // A dialog or menu takes the whole window, however it is laid out.
  const whole = root.querySelector('[role="dialog"], [role="alertdialog"], [role="menu"]') !== null;
  return { whole, rects };
}
