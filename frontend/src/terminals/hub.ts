// One watcher for all terminal panes. Instead of every pane polling the service on its own timer, the hub keeps a
// single long-running "watch" going for every subscribed terminal; the service answers as soon as any of them has
// output, so output arrives when it is produced and an idle window costs one call a second rather than a call per
// pane per tick. If watching does not work (an older service, repeated failures) subscribers are told to fall back
// to polling, which is what panes did before and still can.
import type { TerminalError, TerminalOutput } from './client.js';

export interface WatchedOutput extends TerminalOutput {
  readonly session: string;
  /** The cursor this output starts at: only the subscriber that asked from here may apply it. */
  readonly from: number;
}
export interface WatchReply {
  readonly items: readonly WatchedOutput[];
  /** Sessions that no longer exist. */
  readonly gone: readonly string[];
}
export interface HubDeps {
  watch(cursors: readonly { session: string; from: number }[], waitMs: number): Promise<WatchReply>;
  hidden(): boolean;
  sleep(ms: number): Promise<void>;
  now(): number;
}
export interface Subscriber {
  onOutput(bytes: Uint8Array, next: number, skipped: number): void;
  /** Watching stopped for this subscriber: poll from the cursor it last applied. */
  onFallback(reason: 'gone' | 'unavailable'): void;
}

const MAX_CURSORS = 64;
const WAIT_MS = 1000;
/** After this many failures in a row watching is switched off for a while and subscribers poll. */
const MAX_FAILURES = 3;
const RETRY_AFTER_MS = 30_000;

interface Entry {
  readonly session: string;
  from: number;
  readonly sub: Subscriber;
}

export function createOutputHub(deps: HubDeps) {
  const entries = new Map<number, Entry>();
  let nextId = 1;
  let current = 0;
  let disabledUntil = 0;
  let failures = 0;
  let kick: ReturnType<typeof setTimeout> | undefined;

  const cursors = (): { session: string; from: number }[] => {
    const seen = new Set<string>();
    const out: { session: string; from: number }[] = [];
    for (const e of entries.values()) {
      const key = `${e.session}@${e.from}`;
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({ session: e.session, from: e.from });
      if (out.length === MAX_CURSORS) break;
    }
    return out;
  };

  const disable = (): void => {
    disabledUntil = deps.now() + RETRY_AFTER_MS;
    const all = [...entries.values()];
    entries.clear();
    current++;
    for (const e of all) e.sub.onFallback('unavailable');
  };

  const apply = (reply: WatchReply): void => {
    for (const item of reply.items) {
      for (const e of [...entries.values()]) {
        if (e.session === item.session && e.from === item.from) {
          e.from = item.next;
          e.sub.onOutput(item.bytes, item.next, item.skipped);
        }
      }
    }
    for (const session of reply.gone) {
      for (const [id, e] of [...entries]) {
        if (e.session === session) {
          entries.delete(id);
          e.sub.onFallback('gone');
        }
      }
    }
  };

  async function loop(id: number): Promise<void> {
    while (id === current && entries.size > 0) {
      if (deps.hidden()) {
        await deps.sleep(400);
        continue;
      }
      let reply: WatchReply;
      try {
        reply = await deps.watch(cursors(), WAIT_MS);
        failures = 0;
      } catch (e) {
        if ((e as Partial<TerminalError>)?.code === 'watch_busy') {
          await deps.sleep(100);
          continue;
        }
        failures++;
        if (failures >= MAX_FAILURES) {
          failures = 0;
          disable();
          return;
        }
        await deps.sleep(300 * failures);
        continue;
      }
      // Output is applied even when a newer loop has taken over: it is matched to subscribers by cursor, so it
      // can neither be applied twice nor to the wrong subscriber.
      apply(reply);
    }
  }

  /** Start (or restart) the watch so that it covers the current subscribers; several changes in one turn share one restart. */
  function schedule(): void {
    if (kick !== undefined) return;
    kick = setTimeout(() => {
      kick = undefined;
      if (entries.size === 0) return;
      current++;
      void loop(current);
    }, 0);
  }

  return {
    /**
     * Receive a terminal's output from `from` on. Returns a function that stops it, or `null` when watching is
     * switched off (the caller polls instead).
     */
    subscribe(session: string, from: number, sub: Subscriber): (() => void) | null {
      if (deps.now() < disabledUntil || entries.size >= MAX_CURSORS) return null;
      const id = nextId++;
      entries.set(id, { session, from, sub });
      schedule();
      return () => {
        entries.delete(id);
      };
    },
    /** Drop every subscriber and let the running watch finish (tests, page teardown). */
    stopAll(): void {
      entries.clear();
      current++;
    },
    /** For tests and diagnostics. */
    get subscribers(): number {
      return entries.size;
    },
  };
}
