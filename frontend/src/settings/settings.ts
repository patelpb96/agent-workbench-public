// Strictly validated client for the user settings behind the Properties window. The webview never names a
// file: native code reads and writes the one settings file in the app data folder.
import { folderProblem } from '../terminals/agents.js';
import { sanitizeError, type TerminalError } from '../terminals/client.js';
import { isPrefixSetting, type Prefix } from '../terminals/keymap.js';

export interface Settings {
  readonly version: 1;
  readonly terminals: { readonly start_folder: string; readonly extended_history: boolean };
  readonly agents: { readonly launch_folder: string };
  /** How solid the two sidebars are over the background, in percent (100 = solid). */
  readonly appearance: { readonly sidebar1_opacity: number; readonly sidebar2_opacity: number };
  /** How the window opens: title bar behaviour and the canvas background, remembered between launches. */
  readonly window: WindowSettings;
  /** tmux-style keyboard control in terminals: the prefix key, or off (no key is taken from the terminals). */
  readonly keys: { readonly prefix: Prefix };
}
export interface WindowSettings {
  /** The title bar slides away until the cursor rests on the top edge (true), or always shows (false). */
  readonly titlebar_autohide: boolean;
  /** Only these two are remembered; a chosen image or video has to be picked again. */
  readonly canvas_mode: 'solid' | 'transparent';
  readonly canvas_color: string;
  readonly canvas_alpha: number;
  /** The see-through, click-through canvas (Ctrl+Alt+G) was on when the app was last used. */
  readonly see_through: boolean;
}

export const defaultWindow: WindowSettings = { titlebar_autohide: true, canvas_mode: 'solid', canvas_color: '#0c1118', canvas_alpha: 100, see_through: false };

export const defaultSettings: Settings = {
  version: 1,
  terminals: { start_folder: '', extended_history: false },
  agents: { launch_folder: '' },
  appearance: { sidebar1_opacity: 100, sidebar2_opacity: 100 },
  window: defaultWindow,
  keys: { prefix: 'ctrl+b' }
};

export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

function record(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function percent(v: unknown): boolean {
  return typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 100;
}
function onlyKeys(v: Record<string, unknown>, keys: readonly string[]): boolean {
  const have = Object.keys(v);
  return have.length === keys.length && keys.every(k => have.includes(k));
}

/** A terminal start folder: empty (the user's folder) or an absolute drive path without `..` parts or characters Windows forbids. */
export function startFolderProblem(folder: string): string | null {
  const text = folder.trim();
  if (text === '') return null;
  if (text.length > 240) return 'The folder path is too long.';
  if (!/^[A-Za-z]:\\/.test(text)) return 'Use an absolute path such as C:\\Users\\you\\project, or leave it empty for your user folder.';
  if (/[\u0000-\u001f\u007f-\u009f"<>|*?]/.test(text)) return 'The folder path contains a character that is not allowed.';
  if (text.split('\\').some(part => part === '..' || part === '.')) return 'Use a full path without . or .. parts.';
  return null;
}
/** The folder the Launch agent row starts in: empty, or valid for typing into PowerShell (stricter: no quotes). */
export function launchFolderProblem(folder: string): string | null {
  return folder.trim() === '' ? null : folderProblem(folder);
}
export function settingsProblem(s: Settings): string | null {
  return startFolderProblem(s.terminals.start_folder) ?? launchFolderProblem(s.agents.launch_folder);
}

export function isWindowSettings(v: unknown): v is WindowSettings {
  return record(v) && onlyKeys(v, ['titlebar_autohide', 'canvas_mode', 'canvas_color', 'canvas_alpha', 'see_through']) &&
    typeof v.titlebar_autohide === 'boolean' && (v.canvas_mode === 'solid' || v.canvas_mode === 'transparent') &&
    typeof v.canvas_color === 'string' && /^#[0-9a-fA-F]{6}$/.test(v.canvas_color) && percent(v.canvas_alpha) && typeof v.see_through === 'boolean';
}

export function isSettings(v: unknown): v is Settings {
  return record(v) && onlyKeys(v, ['version', 'terminals', 'agents', 'appearance', 'window', 'keys']) && v.version === 1 && isWindowSettings(v.window) &&
    record(v.keys) && onlyKeys(v.keys, ['prefix']) && isPrefixSetting(v.keys.prefix) &&
    record(v.appearance) && onlyKeys(v.appearance, ['sidebar1_opacity', 'sidebar2_opacity']) && percent(v.appearance.sidebar1_opacity) && percent(v.appearance.sidebar2_opacity) &&
    record(v.terminals) && onlyKeys(v.terminals, ['start_folder', 'extended_history']) &&
    typeof v.terminals.start_folder === 'string' && v.terminals.start_folder.length <= 240 && typeof v.terminals.extended_history === 'boolean' &&
    record(v.agents) && onlyKeys(v.agents, ['launch_folder']) && typeof v.agents.launch_folder === 'string' && v.agents.launch_folder.length <= 240;
}

export function createSettingsClient(call: Invoke) {
  const wrap = async (command: string, args: Record<string, unknown> | undefined): Promise<Settings> => {
    let reply: unknown;
    try {
      reply = await call(command, args);
    } catch (e) {
      throw sanitizeError(e);
    }
    if (!isSettings(reply)) throw { code: 'invalid_response', message: 'The settings reply could not be validated.' } satisfies TerminalError;
    return reply;
  };
  return {
    load: () => wrap('settings_load', undefined),
    /** Opens the Windows folder picker (starting at `start` when it exists). Resolves the chosen path, or null if cancelled. */
    pickFolder: async (start: string): Promise<string | null> => {
      let reply: unknown;
      try {
        reply = await call('pick_folder', { start: start.trim() === '' ? null : start.trim() });
      } catch (e) {
        throw sanitizeError(e);
      }
      if (reply === null) return null;
      if (typeof reply !== 'string' || reply.length === 0 || reply.length > 1024) throw { code: 'invalid_response', message: 'The folder reply could not be validated.' } satisfies TerminalError;
      return reply;
    },
    /** Saves after checking the values here too; resolves with what was actually stored. */
    save: (settings: Settings): Promise<Settings> => {
      const problem = settingsProblem(settings);
      if (problem !== null) return Promise.reject({ code: 'settings_invalid', message: problem } satisfies TerminalError);
      return wrap('settings_save', { settings });
    }
  };
}
