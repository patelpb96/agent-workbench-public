// Test-only entry point, never imported by frontend/index.html or production builds.
// Window/event/appearance plumbing is simulated; execution goes to real native code.
import { mockIPC, mockWindows } from '../../../frontend/node_modules/@tauri-apps/api/mocks.js';
import { emit } from '../../../frontend/node_modules/@tauri-apps/api/event.js';

import { executionCommands } from '../commands.mjs';
globalThis.isTauri = true;
mockWindows('headless-test');
// In-page stand-in for the terminal service (never the real broker): sessions and the saved pane layout live
// in sessionStorage so they survive a reload of the test page; `reads` counts polling for load assertions.
const store = key => { try { return JSON.parse(sessionStorage.getItem(key) ?? 'null'); } catch { return null; } };
const keep = (key, value) => sessionStorage.setItem(key, JSON.stringify(value));
globalThis.__fakeTerminals = {
  // What the native folder picker / background picker return next (tests set these; null = cancelled).
  picked: null,
  background: null,
  get sessions() { return store('fake.sessions') ?? []; },
  set sessions(value) { keep('fake.sessions', value); },
  get layout() { return store('fake.layout'); },
  set layout(value) { keep('fake.layout', value); },
  get agents() { return store('fake.agents') ?? []; },
  set agents(value) { keep('fake.agents', value); },
  /** Terminal ids whose transcript "exists": agent_link answers linked for these, pending for the rest. */
  get linked() { return store('fake.linked') ?? []; },
  set linked(value) { keep('fake.linked', value); },
  launches: [],
  // What agent_resume answers next: 'ok', 'busy' (until forced) or 'error'; and what it was asked.
  resumeMode: 'ok',
  resumes: [],
  get settings() { return store('fake.settings') ?? { version: 1, terminals: { start_folder: '', extended_history: false }, agents: { launch_folder: '' }, appearance: { sidebar1_opacity: 100, sidebar2_opacity: 100 }, window: { titlebar_autohide: true, canvas_mode: 'solid', canvas_color: '#0c1118', canvas_alpha: 100, see_through: false }, keys: { prefix: 'ctrl+b' } }; },
  set settings(value) { keep('fake.settings', value); },
  // What "where is everything" lists (tests change it); opened ids are recorded.
  places: [
    { id: 'app_data', label: 'App data (settings, layout, tags)', path: 'C:\\Users\\me\\AppData\\Roaming\\local.agent-workbench.spike', detail: '640 KB', file: false },
    { id: 'settings', label: 'Settings file', path: 'C:\\Users\\me\\AppData\\Roaming\\local.agent-workbench.spike\\settings.json', detail: '', file: true },
    { id: 'backup_repo', label: 'Private backup repository (encrypted, never share)', path: 'C:\\Users\\me\\agent-workbench-private-backup', detail: '3 encrypted backups, newest 2 h ago', file: false }
  ],
  opened: [],
  get tags() { return store('fake.tags') ?? { version: 1, tags: {} }; },
  set tags(value) { keep('fake.tags', value); },
  get windows() { return store('fake.windows'); },
  set windows(value) { keep('fake.windows', value); },
  // When on, typed text comes back as output after `echoDelayMs` (like a shell echoing a keystroke).
  echo: false, echoDelayMs: 3, typed: {}, sent: {},
  reads: 0, resizes: 0, saves: 0,
  // The shared output watcher: `watches` counts its calls; `watchOff` makes it fail (like a service that predates it).
  watches: 0, watchOff: false,
  // Output a test makes a terminal print (by session id); it comes before any echo of typed keys.
  stream: {},
  // See-through mode: the native side is faked (the key is handled by the page, as when the global hotkey is taken).
  ghost: { on: false, reports: [] },
  // The Files panel's folders (tests may replace it).
  tree: {
    'C:\\Users\\me': [
      { name: 'project', kind: 'folder', size: 0, modified_ms: 0 },
      { name: '.secret', kind: 'folder', size: 0, modified_ms: 0, hidden: true },
      { name: 'notes.txt', kind: 'file', size: 12, modified_ms: 0 }
    ],
    'C:\\Users\\me\\project': [
      { name: 'src', kind: 'folder', size: 0, modified_ms: 0 },
      { name: 'README.md', kind: 'file', size: 2048, modified_ms: 0 }
    ],
    'C:\\Users\\me\\project\\src': []
  }
};
globalThis.__headlessInvoke ??= async () => ({ error: { code: 'execution_transport_error', user_message: 'Execution is not part of the terminal tests.', recoverable: true } });
// What a terminal has printed so far: text a test put there (`stream`), then the echo of typed keys.
function fakeOutput(session) {
  const fake = globalThis.__fakeTerminals;
  const echo = fake.echo ? (fake.typed[session] ?? []).filter(chunk => chunk.at <= performance.now()).map(chunk => chunk.text).join('') : '';
  return (fake.stream[session] ?? '') + echo;
}
function fakeTerminal(command, args) {
  const fake = globalThis.__fakeTerminals;
  switch (command) {
    case 'terminal_list': return { op: 'sessions', items: fake.sessions };
    case 'terminal_read': {
      fake.reads++;
      const bytes = new TextEncoder().encode(fakeOutput(args.session));
      const slice = bytes.slice(Math.min(args.from, bytes.length));
      return { op: 'output', data: btoa(String.fromCharCode(...slice)), next: bytes.length > args.from ? bytes.length : args.from, skipped: 0 };
    }
    case 'terminal_watch': {
      fake.watches++;
      if (fake.watchOff) throw { code: 'terminal_unavailable', user_message: 'The terminal service did not respond.' };
      const echoed = session => new TextEncoder().encode(fakeOutput(session));
      return new Promise(resolve => {
        const deadline = performance.now() + Math.min(args.waitMs, 1000);
        const check = () => {
          const items = [];
          const gone = [];
          for (const [session, from] of args.cursors) {
            if (!fake.sessions.some(x => x.id === session)) { gone.push(session); continue; }
            const bytes = echoed(session);
            if (bytes.length > from) items.push({ session, from, data: btoa(String.fromCharCode(...bytes.slice(from))), next: bytes.length, skipped: 0 });
          }
          if (items.length > 0 || gone.length > 0 || performance.now() >= deadline) resolve({ op: 'watch', items, gone });
          else setTimeout(check, 2);
        };
        check();
      });
    }
    case 'ghost_info': return { on: fake.ghost.on, registered: false, hotkey: 'Ctrl+Alt+G' };
    case 'ghost_toggle': fake.ghost.on = !fake.ghost.on; return fake.ghost.on;
    case 'ghost_report': fake.ghost.reports.push({ effective: args.effective, whole: args.whole, rects: args.rects }); return null;
    case 'terminal_input':
      (fake.inputs ??= []).push({ session: args.session, text: args.text });
      if (fake.echo) (fake.typed[args.session] ??= []).push({ text: args.text, at: performance.now() + fake.echoDelayMs });
      fake.sent[args.session] = (fake.sent[args.session] ?? 0) + 1;
      return true;
    case 'terminal_resize': fake.resizes++; (fake.sizes ??= []).push([args.cols, args.rows]); return true;
    case 'terminal_new': {
      const id = Math.floor(Math.random() * 0xfffffff0).toString(16).padStart(8, '0');
      const session = { id, label: args.label, state: 'running', persistent: true, cols: args.cols, rows: args.rows, host_pid: 1 };
      (fake.newFolders ??= []).push(args.folder ?? null);
      fake.sessions = [...fake.sessions, session];
      return { op: 'created', session };
    }
    case 'terminal_end': fake.sessions = fake.sessions.filter(x => x.id !== args.session); return true;
    case 'terminal_log': {
      // Output logging (tmux pipe-pane): the fake keeps on/off per terminal; `logOff` stands for a service that predates it.
      if (fake.logOff) throw { code: 'terminal_unavailable', user_message: 'Lost the terminal service (pipe closed).' };
      const logs = (fake.logs ??= {});
      const was = logs[args.session] ?? { op: 'logging', on: false, path: null, bytes: 0, stopped_reason: null };
      const path = `C:\\Users\\me\\AppData\\Roaming\\local.agent-workbench.spike\\logs\\${args.session}-20261025T101112Z.log`;
      if (args.action === 'start') logs[args.session] = { ...was, on: true, path, bytes: 0, stopped_reason: null };
      else if (args.action === 'stop') logs[args.session] = { ...was, on: false, bytes: was.on ? 1234 : was.bytes };
      (fake.logCalls ??= []).push([args.session, args.action]);
      return logs[args.session] ?? was;
    }
    case 'terminal_draft': return { op: 'draft', draft: null };
    case 'terminal_grants': return { op: 'grants', items: [] };
    case 'terminal_autostart_get': return false;
    case 'terminal_layout_load': return fake.layout;
    case 'terminal_layout_save': fake.saves++; fake.layout = args.layout; return null;
    case 'agent_launch': {
      const id = Math.floor(Math.random() * 0xfffffff0).toString(16).padStart(8, '0');
      const title = { claude: 'Claude Code', pi: 'Pi', codex: 'Codex' }[args.agent];
      const session = { id, label: `${title} · ${args.cwd.split('\\').pop()}`, state: 'running', persistent: true, cols: 120, rows: 30, host_pid: 1 };
      fake.sessions = [...fake.sessions, session];
      fake.agents = [...fake.agents, { terminal: id, agent: args.agent, cwd: args.cwd, launched_ms: Date.now() }];
      fake.launches.push({ agent: args.agent, cwd: args.cwd });
      return { terminal: session, agent: args.agent };
    }
    case 'agent_resume': {
      fake.resumes.push({ source: args.source, force: args.force });
      const agent = args.source === 'fedcba9876543210' ? 'pi' : 'claude';
      if (fake.resumeMode === 'error') throw { code: 'agent_failed', user_message: 'The folder this session ran in no longer exists.', recoverable: true };
      if (fake.resumeMode === 'busy' && !args.force) return { state: 'busy', agent, age_ms: 12000, running: false };
      if (fake.resumeMode === 'running' && !args.force) return { state: 'busy', agent, age_ms: 0, running: true };
      const id = Math.floor(Math.random() * 0xfffffff0).toString(16).padStart(8, '0');
      const title = { claude: 'Claude Code', pi: 'Pi' }[agent];
      const session = { id, label: `${title} \u00b7 proj`, state: 'running', persistent: true, cols: 120, rows: 30, host_pid: 1 };
      fake.sessions = [...fake.sessions, session];
      fake.agents = [...fake.agents, { terminal: id, agent, cwd: 'C:\\Users\\me\\proj', launched_ms: Date.now() }];
      return { state: 'started', agent, terminal: session, source: args.source };
    }
    case 'terminal_restore': {
      const i = fake.sessions.findIndex(x => x.id === args.session && x.state === 'lost');
      if (i < 0) throw { code: 'terminal_failed', user_message: 'No lost terminal has that id.', recoverable: true };
      fake.sessions = fake.sessions.map((x, n) => (n === i ? { ...x, state: 'running' } : x));
      (fake.restored ??= []).push({ terminal: args.session, agent: false });
      return { op: 'created', session: fake.sessions[i] };
    }
    case 'agent_restore': {
      const i = fake.sessions.findIndex(x => x.id === args.terminal && x.state === 'lost');
      if (i < 0) throw { code: 'agent_failed', user_message: 'No lost terminal has that id.', recoverable: true };
      fake.sessions = fake.sessions.map((x, n) => (n === i ? { ...x, state: 'running' } : x));
      const entry = fake.agents.find(a => a.terminal === args.terminal);
      (fake.restored ??= []).push({ terminal: args.terminal, agent: entry?.agent ?? null });
      return { terminal: fake.sessions[i], agent: entry?.agent ?? null, resumed: Boolean(entry) };
    }
    case 'agent_terminals': return fake.agents;
    case 'agent_forget': fake.agents = fake.agents.filter(a => a.terminal !== args.terminal); return null;
    case 'agent_link': {
      const entry = fake.agents.find(a => a.terminal === args.terminal);
      if (!entry) throw { code: 'agent_failed', user_message: 'That terminal was not launched as an agent.', recoverable: true };
      return fake.linked.includes(args.terminal) ? { state: 'linked', agent: entry.agent, source: '0123456789abcdef' } : { state: 'pending', agent: entry.agent };
    }
    case 'inspector_map': return fake.mapPayload ?? { generated_ms: Date.now(), sessions: [{ id: '0123456789abcdef', open_calls: 1, open_tools: ['Bash'], tools: [{ name: 'Bash', calls: 4, errors: 1 }, { name: 'Read', calls: 9, errors: 0 }] }], reports: [] };
    case 'inspector_sources': {
      const now = Date.now();
      return [...fake.extraSources ?? [], { id: '0123456789abcdef', agent: 'claude', label: 'C--Users-me-proj', modified_ms: now - 5000, bytes: 4096, active: true, running: 'background', model: 'claude-opus-5', last_line: 'Done: all tests pass', folder: 'proj', kind: 'session', parent: null },
              { id: 'fedcba9876543210', agent: 'pi', label: '--C--Users-me--', modified_ms: now - 600000, bytes: 2048, active: false, running: null, model: 'gemini-x', last_line: 'Waiting for your review', folder: 'me', kind: 'session', parent: null },
              { id: '1111111111111111', agent: 'codex', label: '2026-10-20', modified_ms: now - 5 * 3600000, bytes: 9000, active: false, running: null, model: 'gpt-6', last_line: 'Patch applied', folder: 'repo', kind: 'session', parent: null },
              { id: '2222222222222222', agent: 'claude', label: 'C--Users-me-x', modified_ms: now - 3 * 3600000, bytes: 216, active: false, running: null, model: null, last_line: null, folder: null, kind: 'session', parent: null }];
    }
    case 'inspector_snapshot': {
      const now = Date.now();
      return { source: { id: args.id, agent: 'claude', label: 'proj', modified_ms: now, bytes: 1234 }, now_ms: now, peek: { last_line: 'Done: all tests pass', model: 'claude-opus-5' }, snapshot: {
        state: fake.snapshotState ?? 'working', first_event_ms: now - 60000, last_event_ms: now - 2000, turns: 3, open_calls: 1,
        usage: { input: 1200, output: 340, cache_read: null, cache_write: null, reasoning: null, records: 3 },
        breakdown: { tool_ms: 12000, model_ms: 30000, human_ms: 5000 },
        tools: [{ name: 'Bash', calls: 4, completed: 4, errors: 1, unknown_outcome: 0, total_ms: 9000, max_ms: 5000, median_ms: 1500, last_used_ms: now - 4000 },
                { name: 'Read', calls: 9, completed: 9, errors: 0, unknown_outcome: 0, total_ms: 3000, max_ms: 900, median_ms: 300, last_used_ms: now - 9000 }],
        tools_omitted: 0, recent: [{ tool: 'Bash', started_ms: now - 4000, duration_ms: 1500, ok: true }], timeline: null, orphan_results: 0 } };
    }
    case 'locations_list': return fake.places;
    case 'locations_open': fake.opened.push(args.id); return null;
    // The Files panel: a small fake tree keyed by folder path; revealed paths are recorded.
    case 'files_home': return 'C:\\Users\\me';
    case 'files_list': {
      const entries = fake.tree[args.path];
      if (!entries) throw { code: 'files_failed', user_message: 'That folder does not exist or cannot be read.', recoverable: true };
      const shown = entries.filter(e => args.showHidden || !e.hidden).map(({ hidden, ...e }) => e);
      return { path: args.path, entries: shown, more: 0, hidden: entries.length - shown.length };
    }
    case 'files_reveal': (fake.revealed ??= []).push(args.path); return null;
    case 'inspector_tags_load': return fake.tags;
    case 'inspector_tags_save': fake.tags = args.tags; return fake.tags;
    case 'settings_load': return fake.settings;
    case 'settings_save': {
      // The native side also requires the folders to exist; the fake accepts the shapes the client already checked.
      if (args.settings.terminals.start_folder === 'C:\\does\\not\\exist') throw { code: 'settings_invalid', user_message: 'The terminal start folder does not exist.', recoverable: true };
      fake.settings = args.settings;
      return fake.settings;
    }
    case 'terminal_windows_load': return fake.windows;
    case 'terminal_windows_save': fake.windows = args.windows; return null;
    default: return true;
  }
}
mockIPC(async (command, args) => {
  if (command.startsWith('terminal_') || command.startsWith('ghost_') || command.startsWith('agent_') || command.startsWith('settings_') || command.startsWith('locations_') || command.startsWith('files_') || command.startsWith('inspector_')) return fakeTerminal(command, args);
  if (executionCommands.includes(command)) {
    const reply = await globalThis.__headlessInvoke({ command, request: args?.request });
    if ('error' in reply) throw reply.error;
    return reply.ok;
  }
  if (command === 'get_runtime_info') return { interface_version: 1, app_version: globalThis.__awRuntimeVersion ?? 'headless-test', os: 'headless shell stub', providers: [] };
  if (command === 'plugin:window|is_visible') return true;
  if (command === 'plugin:window|is_minimized') return false;
  if (command === 'set_window_appearance') { (globalThis.__fakeTerminals.appearances ??= []).push(args.appearance); return null; }
  if (command === 'open_background_file') {
    const next = globalThis.__fakeTerminals.background;
    if (typeof next === 'string') throw { code: 'media_too_large', user_message: next, recoverable: true };
    return next ?? { status: 'cancelled' };
  }
  if (command === 'pick_folder') {
    const next = globalThis.__fakeTerminals.picked;
    globalThis.__fakeTerminals.pickedStart = args?.start ?? null;
    return next ?? null;
  }
  throw { code: 'headless_shell_unavailable', user_message: 'This OS shell operation is not implemented by the headless test facade.', recoverable: true };
}, { shouldMockEvents: true });
globalThis.__headlessNotify = payload => emit('execution_changed_v2', payload);
await import('../../../frontend/src/main.ts');
