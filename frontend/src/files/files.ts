// Strictly validated client for the Files panel (docs/design/sidebar-restructure.md): one folder's entries, read-only,
// and "show in File Explorer". Plus the pure helpers the panel uses (joining paths, quoting a path for a terminal).
import { sanitizeError, type TerminalError } from '../terminals/client.js';

export type Kind = 'folder' | 'file' | 'link';
export interface Entry {
  readonly name: string;
  readonly kind: Kind;
  readonly size: number;
  readonly modified_ms: number;
}
export interface Listing {
  readonly path: string;
  readonly entries: readonly Entry[];
  /** Entries that exist but were not returned. */
  readonly more: number;
  /** Hidden or system entries left out. */
  readonly hidden: number;
}
export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

/** The most entries one folder may return (the native cap). */
export const maxEntries = 2000;

const count = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v) && v >= 0;
// A name is one path part: no separators, no control characters, not . or ..
const goodName = (v: unknown): v is string => typeof v === 'string' && v.length > 0 && v.length <= 255 && v !== '.' && v !== '..' && !/[\\/\u0000-\u001f]/.test(v);

/** An absolute drive path of the shape the native side accepts (the terminal start-folder rule, checked loosely here). */
export function isFolderPath(v: unknown): v is string {
  return typeof v === 'string' && v.length <= 240 && /^[A-Za-z]:\\/.test(v) && !/[\u0000-\u001f"<>|*?]/.test(v) && !v.split('\\').some(p => p === '..' || p === '.');
}

export function isEntry(v: unknown): v is Entry {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const o = v as Record<string, unknown>;
  return Object.keys(o).length === 4 && goodName(o.name) && (o.kind === 'folder' || o.kind === 'file' || o.kind === 'link') && count(o.size) && count(o.modified_ms);
}

export function isListing(v: unknown): v is Listing {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const o = v as Record<string, unknown>;
  return Object.keys(o).length === 4 && isFolderPath(o.path) && Array.isArray(o.entries) && o.entries.length <= maxEntries && o.entries.every(isEntry) && count(o.more) && count(o.hidden);
}

/** `C:\a` + `b` = `C:\a\b` (and `C:\` + `b` = `C:\b`). */
export function joinPath(folder: string, name: string): string {
  return folder.endsWith('\\') ? `${folder}${name}` : `${folder}\\${name}`;
}

/** The folder above, or null at a drive root. */
export function parentPath(path: string): string | null {
  const trimmed = path.replace(/\\+$/, '');
  const at = trimmed.lastIndexOf('\\');
  if (at < 0 || /^[A-Za-z]:$/.test(trimmed)) return null;
  const up = trimmed.slice(0, at);
  return /^[A-Za-z]:$/.test(up) ? `${up}\\` : up;
}

/** What "Insert path" types: the path in double quotes (Windows paths cannot contain a quote), never a newline. */
export function quotedPath(path: string): string | null {
  return /["\r\n\u0000-\u001f]/.test(path) ? null : `"${path}"`;
}

/** A pinned file or folder, listed at the top of the Files panel whatever folder is shown. */
export interface Pin { readonly path: string; readonly folder: boolean }
export const maxPins = 50;

/** Stored pins; anything malformed is dropped, duplicates keep the first. */
export function parsePins(raw: unknown): Pin[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: Pin[] = [];
  for (const v of raw) {
    if (typeof v !== 'object' || v === null) continue;
    const { path, folder } = v as { path?: unknown; folder?: unknown };
    if (!isFolderPath(path) || typeof folder !== 'boolean' || seen.has(path.toLowerCase())) continue;
    seen.add(path.toLowerCase());
    out.push({ path, folder });
    if (out.length >= maxPins) break;
  }
  return out;
}

/** Pin a path, or unpin it if it is already pinned (Windows paths compare ignoring case). Refused past the cap. */
export function togglePin(pins: readonly Pin[], pin: Pin): Pin[] {
  const key = pin.path.toLowerCase();
  if (pins.some(p => p.path.toLowerCase() === key)) return pins.filter(p => p.path.toLowerCase() !== key);
  return pins.length >= maxPins ? [...pins] : [...pins, pin];
}

export function isPinned(pins: readonly Pin[], path: string): boolean {
  const key = path.toLowerCase();
  return pins.some(p => p.path.toLowerCase() === key);
}

/** The last part of a path (the drive itself for a root). */
export function baseName(path: string): string {
  const trimmed = path.replace(/\\+$/, '');
  const at = trimmed.lastIndexOf('\\');
  return at < 0 ? trimmed : trimmed.slice(at + 1) || trimmed;
}

/** 1.2 KB, 3.4 MB: sizes for the tree. */
export function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

export function createFilesClient(call: Invoke) {
  const ask = async (command: string, args?: Record<string, unknown>): Promise<unknown> => {
    try {
      return await call(command, args);
    } catch (e) {
      throw sanitizeError(e);
    }
  };
  return {
    list: async (path: string, showHidden: boolean): Promise<Listing> => {
      if (!isFolderPath(path)) throw { code: 'files_invalid', message: 'Use an absolute folder path such as C:\\Users\\you\\project.' } satisfies TerminalError;
      const reply = await ask('files_list', { path, showHidden });
      if (!isListing(reply) || reply.path !== path) throw { code: 'invalid_response', message: 'The folder listing could not be validated.' } satisfies TerminalError;
      return reply;
    },
    home: async (): Promise<string> => {
      const reply = await ask('files_home');
      if (!isFolderPath(reply)) throw { code: 'invalid_response', message: 'The home folder could not be validated.' } satisfies TerminalError;
      return reply;
    },
    reveal: async (path: string): Promise<void> => {
      if (!isFolderPath(path)) throw { code: 'files_invalid', message: 'Invalid path.' } satisfies TerminalError;
      await ask('files_reveal', { path });
    }
  };
}
