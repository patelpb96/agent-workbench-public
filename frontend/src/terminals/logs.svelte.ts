// What Workbench knows about each terminal's output log (tmux pipe-pane), shared by every place a terminal is shown,
// so the REC mark appears on a pane, a floating window and the terminal list alike. The service is the truth:
// this only remembers its last answer.
import { SvelteMap } from 'svelte/reactivity';
import type { LogState, TerminalError } from './client.js';
import { terminalClient } from './tauri.js';

const states = new SvelteMap<string, LogState>();
// A service from before logging drops the connection on a log request: after one such answer, status is not asked
// again for a minute (polling it would reconnect over and over). Toggling still tries, and says what happened.
const QUIET_MS = 60_000;
let quietUntil = 0;
const asked = new Set<string>();

export const logStore = {
  of: (session: string): LogState | undefined => states.get(session),
  on: (session: string): boolean => states.get(session)?.on === true,
  /** Ask the service (quietly: an older service without logging just leaves the terminal unmarked). */
  async refresh(session: string): Promise<void> {
    if (Date.now() < quietUntil) return;
    try {
      states.set(session, await terminalClient.log(session, 'status'));
      // Only an answer counts as asked: a failed first check is tried again on the next poll.
      asked.add(session);
    } catch (e) {
      if (/Lost the terminal service/i.test((e as TerminalError).message)) quietUntil = Date.now() + QUIET_MS;
    }
  },
  /** The Terminals view's regular check: each terminal is asked once, and those being logged every time (a log can
   *  stop by itself at the size cap or on a write error). Resolves with a sentence for each log that just stopped. */
  async poll(sessions: readonly string[]): Promise<string[]> {
    const stopped: string[] = [];
    for (const id of sessions) {
      const before = states.get(id);
      if (asked.has(id) && before?.on !== true) continue;
      await this.refresh(id);
      const after = states.get(id);
      if (before?.on === true && after?.on === false) stopped.push(`Logging stopped by itself${after.stopped_reason ? `: ${after.stopped_reason}` : ''}.`);
    }
    return stopped;
  },
  /** Turn logging on or off; resolves with a sentence for the person, never throws. */
  async toggle(session: string): Promise<string> {
    try {
      const was = states.get(session)?.on ?? (await terminalClient.log(session, 'status')).on;
      const now = await terminalClient.log(session, was ? 'stop' : 'start');
      states.set(session, now);
      if (now.on) return `Logging this terminal's output to ${now.path ?? 'a file'}. Everything it prints is saved, passwords included; prefix then L stops it.`;
      return `Logging stopped${now.bytes > 0 ? ` after ${formatBytes(now.bytes)}` : ''}${now.path ? `. The log is ${now.path}` : ''}.`;
    } catch (e) {
      const err = e as TerminalError;
      // A service from before logging closes the connection on a request it does not know.
      return /Lost the terminal service/i.test(err.message)
        ? 'The terminal service did not take the request: it is probably older than logging. It updates when the service restarts (your terminals keep running).'
        : `Logging could not change: ${err.message}`;
    }
  },
  forget(session: string): void { states.delete(session); }
};

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} bytes`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}
