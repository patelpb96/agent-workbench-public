// Strictly validated client for "where is everything": the places Agent Workbench keeps things on this machine
// (only those that exist). Display and "show in Explorer" only; the webview names a place by id, never by path.
import { sanitizeError, type TerminalError } from '../terminals/client.js';

export interface Location {
  readonly id: string;
  readonly label: string;
  readonly path: string;
  readonly detail: string;
  /** A file (shown selected in Explorer) rather than a folder. */
  readonly file: boolean;
}
export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const idPattern = /^[a-z][a-z_]{0,31}$/;

export function isLocation(v: unknown): v is Location {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const o = v as Record<string, unknown>;
  return Object.keys(o).length === 5 && typeof o.id === 'string' && idPattern.test(o.id) && typeof o.label === 'string' && o.label.length > 0 && o.label.length <= 100 &&
    typeof o.path === 'string' && o.path.length > 0 && o.path.length <= 1024 && typeof o.detail === 'string' && o.detail.length <= 120 && typeof o.file === 'boolean';
}

export function createLocationsClient(call: Invoke) {
  return {
    list: async (): Promise<Location[]> => {
      let reply: unknown;
      try {
        reply = await call('locations_list');
      } catch (e) {
        throw sanitizeError(e);
      }
      // Ids are unique; anything malformed makes the whole list untrusted.
      if (!Array.isArray(reply) || reply.length > 32 || !reply.every(isLocation) || new Set(reply.map(l => l.id)).size !== reply.length) {
        throw { code: 'invalid_response', message: 'The list of places could not be validated.' } satisfies TerminalError;
      }
      return reply;
    },
    open: async (id: string): Promise<void> => {
      if (!idPattern.test(id)) throw { code: 'locations_failed', message: 'Invalid place.' } satisfies TerminalError;
      try {
        await call('locations_open', { id });
      } catch (e) {
        throw sanitizeError(e);
      }
    }
  };
}
