// tmux's synchronize-panes: while it is on, what a person types into one of the shown panes is typed into all of
// them. Only keystrokes and pastes from the emulator pass through here (TerminalPane's onData); an agent's drafts and
// grants go through the terminal service and are never copied to other terminals.
import { terminalClient } from './tauri.js';

let targets = $state.raw<readonly string[]>([]);
// Per terminal, keystrokes stay in order: one request at a time.
const queues = new Map<string, Promise<unknown>>();

export const broadcast = {
  /** True while typing goes to several terminals. */
  get on(): boolean { return targets.length > 1; },
  get targets(): readonly string[] { return targets; },
  includes: (session: string): boolean => targets.length > 1 && targets.includes(session),
  /** Turn it on for these terminals (the panes in view), or off with an empty list. */
  set(sessions: readonly string[]): void {
    targets = [...new Set(sessions)];
    for (const id of queues.keys()) if (!targets.includes(id)) queues.delete(id);
  },
  /** Copy typing from `from` to every other target terminal. `from` sends its own copy itself. */
  fanOut(from: string, data: string, fail: (e: unknown) => void): void {
    if (!this.includes(from)) return;
    for (const to of targets) {
      if (to === from) continue;
      const next = (queues.get(to) ?? Promise.resolve()).then(() => terminalClient.input(to, data)).catch(fail);
      queues.set(to, next);
    }
  }
};
