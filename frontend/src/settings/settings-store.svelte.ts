// Shared, reactive copy of the user settings. Loaded once at start; every change is validated, saved natively
// and replaced by what the native side actually stored (so the UI never shows something that was not saved).
import { invoke, isTauri } from '@tauri-apps/api/core';
import { createSettingsClient, defaultSettings, type Settings } from './settings.js';
import type { TerminalError } from '../terminals/client.js';

const client = createSettingsClient((command, args) =>
  isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'Settings are saved in the desktop app.' }));

let current = $state.raw<Settings>(defaultSettings);
let loaded = $state(false);
let saving = $state(false);
let problem = $state.raw<TerminalError | null>(null);

export const settingsStore = {
  get value(): Settings { return current; },
  get loaded(): boolean { return loaded; },
  get saving(): boolean { return saving; },
  get problem(): TerminalError | null { return problem; },

  async load(): Promise<void> {
    try {
      current = await client.load();
    } catch {
      /* not available (browser preview): keep the defaults */
    } finally {
      loaded = true;
    }
  },

  /** Apply a change. Resolves true when it was saved; false (with `problem` set) when it was refused. */
  async update(change: (s: Settings) => Settings): Promise<boolean> {
    const next = change(current);
    saving = true;
    try {
      current = await client.save(next);
      problem = null;
      return true;
    } catch (e) {
      problem = e as TerminalError;
      return false;
    } finally {
      saving = false;
    }
  },
  /** The chosen folder, or null when cancelled or unavailable (the reason is thrown as a TerminalError). */
  pickFolder: (start: string): Promise<string | null> => client.pickFolder(start),
  clearProblem(): void { problem = null; }
};
