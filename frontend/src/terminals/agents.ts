// Thin, strictly validated client for launching an agent in a terminal and linking it to its transcript.
// The webview never sees a transcript path: it asks for the link by terminal id and gets an opaque id.
import { isTerminalInfo, sanitizeError, type TerminalError, type TerminalInfo } from './client.js';

export type AgentName = 'claude' | 'pi' | 'codex';
export const agentChoices: readonly { id: AgentName; label: string }[] = [
  { id: 'claude', label: 'Claude Code' },
  { id: 'pi', label: 'Pi' },
  { id: 'codex', label: 'Codex' }
];
export const agentLabel = (agent: string): string => agentChoices.find(a => a.id === agent)?.label ?? 'Agent';

export interface AgentEntry { readonly terminal: string; readonly agent: AgentName; readonly cwd: string; readonly launched_ms: number }
export type AgentLink =
  | { readonly state: 'pending'; readonly agent: AgentName }
  | { readonly state: 'linked'; readonly agent: AgentName; readonly source: string };
export interface AgentLaunch { readonly terminal: TerminalInfo; readonly agent: AgentName }
/** The answer to "open this existing session in a terminal". `busy`: its transcript changed moments ago, so it may still be running elsewhere. */
export type AgentResume =
  | { readonly state: 'started'; readonly agent: AgentName; readonly terminal: TerminalInfo; readonly source: string }
  | { readonly state: 'already_open'; readonly agent: AgentName; readonly terminal: TerminalInfo }
  | { readonly state: 'busy'; readonly agent: AgentName; readonly age_ms: number; readonly running: boolean };

export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const terminalPattern = /^[0-9a-f]{8}$/;
const sourcePattern = /^[0-9a-f]{16}$/;

function record(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function agentName(v: unknown): v is AgentName {
  return v === 'claude' || v === 'pi' || v === 'codex';
}

/** The same rules the native side enforces, so the form can say what is wrong before a round trip. */
export function folderProblem(cwd: string): string | null {
  const text = cwd.trim();
  if (text.length === 0) return 'Enter the folder to start the agent in.';
  if (text.length > 240) return 'The folder path is too long.';
  if (!/^[A-Za-z]:\\/.test(text)) return 'Use an absolute path such as C:\\Users\\you\\project.';
  // Control characters, quotes (including the Unicode ones PowerShell treats as quotes) and characters Windows forbids.
  if (/[\u0000-\u001f\u007f-\u009f'"`<>|*?\u2018\u2019\u201a\u201b]/.test(text)) return 'The folder path contains a character that is not allowed.';
  if (text.split('\\').some(part => part === '..' || part === '.')) return 'Use a full path without . or .. parts.';
  return null;
}

export function isAgentEntry(v: unknown): v is AgentEntry {
  return record(v) && typeof v.terminal === 'string' && terminalPattern.test(v.terminal) && agentName(v.agent) &&
    typeof v.cwd === 'string' && v.cwd.length <= 240 && typeof v.launched_ms === 'number' && Number.isSafeInteger(v.launched_ms) && v.launched_ms > 0;
}
export function isAgentResume(v: unknown): v is AgentResume {
  if (!record(v) || !agentName(v.agent)) return false;
  if (v.state === 'busy') return typeof v.age_ms === 'number' && Number.isSafeInteger(v.age_ms) && v.age_ms >= 0 && typeof v.running === 'boolean' && Object.keys(v).length === 4;
  if (v.state === 'already_open') return isTerminalInfo(v.terminal) && Object.keys(v).length === 3;
  return v.state === 'started' && isTerminalInfo(v.terminal) && typeof v.source === 'string' && sourcePattern.test(v.source) && Object.keys(v).length === 4;
}
export function isAgentLink(v: unknown): v is AgentLink {
  if (!record(v) || !agentName(v.agent)) return false;
  if (v.state === 'pending') return Object.keys(v).length === 2;
  return v.state === 'linked' && typeof v.source === 'string' && sourcePattern.test(v.source) && Object.keys(v).length === 3;
}

export function createAgentClient(call: Invoke) {
  const wrap = async <T>(command: string, args: Record<string, unknown> | undefined, check: (v: unknown) => T | null): Promise<T> => {
    let reply: unknown;
    try {
      reply = await call(command, args);
    } catch (e) {
      throw sanitizeError(e);
    }
    const ok = check(reply);
    if (ok === null) throw { code: 'invalid_response', message: 'The agent reply could not be validated.' } satisfies TerminalError;
    return ok;
  };
  return {
    launch: (agent: AgentName, cwd: string) => wrap('agent_launch', { agent, cwd }, v =>
      record(v) && agentName(v.agent) && isTerminalInfo(v.terminal) ? ({ terminal: v.terminal, agent: v.agent } as AgentLaunch) : null),
    /** Resume the session behind an inspector source in a new Workbench terminal (`force`: the user confirmed it is not running elsewhere). */
    resume: (source: string, force: boolean) => {
      if (!sourcePattern.test(source)) return Promise.reject({ code: 'agent_failed', message: 'Invalid session id.' } satisfies TerminalError);
      return wrap('agent_resume', { source, force }, v => (isAgentResume(v) ? v : null));
    },
    /** Recreate an agent's terminal after a restart and resume the agent in it (`resumed` says whether a resume line was typed). */
    restore: (terminal: string) => wrap('agent_restore', { terminal }, v =>
      record(v) && isTerminalInfo(v.terminal) && typeof v.resumed === 'boolean' && (v.agent === null || agentName(v.agent)) ? ({ terminal: v.terminal, resumed: v.resumed } as { terminal: TerminalInfo; resumed: boolean }) : null),
    list: () => wrap('agent_terminals', undefined, v => (Array.isArray(v) && v.length <= 64 && v.every(isAgentEntry) ? (v as AgentEntry[]) : null)),
    forget: (terminal: string) => wrap('agent_forget', { terminal }, () => true),
    link: (terminal: string) => wrap('agent_link', { terminal }, v => (isAgentLink(v) ? v : null))
  };
}
