// Agents launched in terminals: which terminal runs which agent, whether its transcript has been found
// (so an activity view can follow it), and which view each agent terminal currently shows.
// One shared instance; the registry itself lives natively (agent-terminals.json), this is the reactive mirror.
import { agentClient } from './tauri.js';
import { canvasTerminals } from './canvas-store.svelte.js';
import { openFromWatch } from './floating.js';
import { inspectorClient } from '../inspector/tauri.js';
import type { TerminalError } from './client.js';
import type { AgentEntry, AgentLaunch, AgentLink, AgentName } from './agents.js';

export type AgentView = 'terminal' | 'activity';

let entries = $state.raw<Record<string, AgentEntry>>({});
let links = $state.raw<Record<string, AgentLink>>({});
let views = $state.raw<Record<string, AgentView>>({});
let busy = false;
/** Agent terminals whose transcript says they are waiting for a person. */
let waiting = $state.raw<Record<string, boolean>>({});
let checking = false;
/** A session the user asked to open in a terminal whose transcript changed moments ago: waits for "open anyway". */
let pending = $state.raw<{ source: string; agent: AgentName; label: string; ageMs: number; running: boolean } | null>(null);
let resumeError = $state<string | null>(null);
let opening = $state(false);
let reveal: (() => void) | null = null;

export const agentStore = {
  get pendingResume() { return pending; },
  get resumeError(): string | null { return resumeError; },
  get opening(): boolean { return opening; },
  /** Called after a session was opened in a terminal, so the app can show the Canvas where it now is. */
  onOpened(handler: (() => void) | null): void { reveal = handler; },
  entryOf: (terminal: string): AgentEntry | undefined => entries[terminal],
  linkOf: (terminal: string): AgentLink | undefined => links[terminal],
  viewOf: (terminal: string): AgentView => views[terminal] ?? 'terminal',
  needsHuman: (terminal: string): boolean => waiting[terminal] === true,
  setView(terminal: string, view: AgentView): void { views = { ...views, [terminal]: view }; },

  /** Re-read the registry, forget agents whose terminal is gone, and look for transcripts not found yet. */
  async refresh(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      const list = await agentClient.list();
      const alive = new Set(canvasTerminals.terminals.map(t => t.id));
      const kept: AgentEntry[] = [];
      for (const entry of list) {
        // Only trust "gone" after a successful terminal listing.
        if (canvasTerminals.listed && !alive.has(entry.terminal)) void agentClient.forget(entry.terminal).catch(() => undefined);
        else kept.push(entry);
      }
      entries = Object.fromEntries(kept.map(e => [e.terminal, e]));
      for (const entry of kept) {
        if (links[entry.terminal]?.state === 'linked') continue;
        try {
          const link = await agentClient.link(entry.terminal);
          links = { ...links, [entry.terminal]: link };
        } catch {
          /* leave it pending; the next refresh tries again */
        }
      }
    } catch {
      /* the registry is unavailable right now: keep what is shown */
    } finally {
      busy = false;
    }
  },

  /** Ask each linked agent's transcript whether it is waiting for a person (state only; the inspector reads no prompts or replies). */
  async checkWaiting(): Promise<void> {
    if (checking) return;
    checking = true;
    try {
      const next: Record<string, boolean> = {};
      for (const entry of Object.values(entries)) {
        const link = links[entry.terminal];
        if (link?.state !== 'linked') continue;
        try {
          const reply = await inspectorClient.snapshot(link.source);
          if (reply.snapshot.state === 'waiting_for_human') next[entry.terminal] = true;
        } catch {
          /* unreadable right now: not flashing */
        }
      }
      waiting = next;
    } finally {
      checking = false;
    }
  },

  /** Start an agent in a new terminal. The caller shows the returned terminal. */
  async launch(agent: AgentName, cwd: string): Promise<AgentLaunch> {
    const launched = await agentClient.launch(agent, cwd);
    await canvasTerminals.refresh();
    await this.refresh();
    return launched;
  },

  /**
   * Open an existing session (found by the inspector, optionally watched on the canvas) in a Workbench terminal
   * by resuming it there. A watch window turns into the terminal window. A session that looks active elsewhere
   * is only opened after the user confirms (`pendingResume`).
   */
  async openInTerminal(source: string, agent: AgentName, label: string, force = false): Promise<void> {
    if (opening) return;
    opening = true;
    resumeError = null;
    try {
      const reply = await agentClient.resume(source, force);
      if (reply.state === 'busy') {
        pending = { source, agent, label, ageMs: reply.age_ms, running: reply.running };
        return;
      }
      pending = null;
      await canvasTerminals.refresh();
      canvasTerminals.commit(openFromWatch(canvasTerminals.state, source, reply.terminal.id, { w: 1400, h: 800 }));
      void this.refresh();
      reveal?.();
    } catch (e) {
      pending = null;
      resumeError = (e as TerminalError).message ?? 'The session could not be opened in a terminal.';
    } finally {
      opening = false;
    }
  },
  confirmResume(): void {
    const p = pending;
    if (p) void this.openInTerminal(p.source, p.agent, p.label, true);
  },
  cancelResume(): void { pending = null; },
  dismissResumeError(): void { resumeError = null; },

  /** Poll while the app is open: quickly while a transcript is still being waited for, slowly otherwise. */
  start(): () => void {
    void this.refresh();
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = () => {
      if (stopped) return;
      if (!document.hidden) void this.refresh().then(() => this.checkWaiting());
      const waiting = Object.values(entries).some(e => links[e.terminal]?.state !== 'linked');
      timer = setTimeout(tick, waiting ? 3000 : 10000);
    };
    timer = setTimeout(tick, 3000);
    return () => { stopped = true; clearTimeout(timer); };
  }
};
