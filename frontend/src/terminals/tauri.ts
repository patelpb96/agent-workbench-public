// Tauri wiring kept apart from the pure, unit-tested client.
import { invoke, isTauri } from '@tauri-apps/api/core';
import { createAgentClient } from './agents.js';
import { createTerminalClient } from './client.js';
import { createOutputHub } from './hub.js';

/** The remembered pane arrangement from the app data folder (null when none is saved). */
export async function loadSavedPanes(): Promise<unknown> {
  if (!isTauri()) return null;
  try { return await invoke('terminal_layout_load'); } catch { return null; }
}
/** Fire-and-forget: the arrangement is view state, so a failed save only means it is not remembered. */
export function savePanes(state: unknown): void {
  if (isTauri()) void invoke('terminal_layout_save', { layout: state }).catch(() => undefined);
}

/** The remembered floating terminal windows and session links (null when none is saved). */
export async function loadSavedWindows(): Promise<unknown> {
  if (!isTauri()) return null;
  try { return await invoke('terminal_windows_load'); } catch { return null; }
}
export function saveWindows(state: unknown): void {
  if (isTauri()) void invoke('terminal_windows_save', { windows: state }).catch(() => undefined);
}

export const terminalAvailable = (): boolean => isTauri();
export const terminalClient = createTerminalClient((command, args) =>
  isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'Terminals are available in the desktop app.' }));
/** One watcher for every terminal pane (see hub.ts); panes poll instead when it is off. */
export const outputHub = createOutputHub({
  watch: (cursors, waitMs) => terminalClient.watch(cursors, waitMs),
  hidden: () => document.hidden,
  sleep: ms => new Promise(resolve => setTimeout(resolve, ms)),
  now: () => Date.now()
});
export const agentClient = createAgentClient((command, args) =>
  isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'Agents can be launched in the desktop app.' }));
