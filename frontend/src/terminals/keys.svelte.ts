// The prefix-key listener (keymap.ts holds the rules). One window listener in the capture phase sees a key before the
// focused terminal does; it takes the prefix and the command key after it, and nothing else. Commands go to the
// terminal the key was pressed in (search, copy mode, logging, sending the prefix) or to the Terminals view's panes.
import { settingsStore } from '../settings/settings-store.svelte.js';
import { command, interpret, isModifierKey, isPrefix, PREFIX_BYTES, PREFIX_LABEL, PREFIX_TIMEOUT_MS, REPEAT_MS, repeatable, type Action, type Prefix } from './keymap.js';
import { logStore } from './logs.svelte.js';
import { nearestInDirection, readingOrder, type Box } from './spatial.js';

/** What a shown terminal offers the keymap (registered by TerminalPane while it is mounted). `send` types through the
 *  pane's own input path (in order with typing, into all panes when that is on, and not at all in copy mode). */
export interface TerminalCommands { readonly search: () => void; readonly copyMode: () => void; readonly send: (text: string) => void }
/** What the Terminals view offers (registered while it is open). True when the action was taken. */
export type PaneCommands = (action: Action) => boolean;

const PANE_ACTIONS = new Set<Action['kind']>(['split', 'focus', 'resize', 'select', 'next-pane', 'close-pane', 'zoom', 'next-layout', 'new-terminal', 'sync']);

let waiting = $state(false);
let message = $state<string | null>(null);
let helpOpen = $state(false);
let waitingIn: Shown | null = null;
let waitTimer: ReturnType<typeof setTimeout> | undefined;
let messageTimer: ReturnType<typeof setTimeout> | undefined;
// Keyed by the shown terminal's element, not its id: the commands go to the emulator the key was pressed in.
const terminals = new WeakMap<Element, TerminalCommands>();
/** Where a key was pressed: a terminal (`session`, `el`), a window on the Canvas (`win`), or both. */
interface Shown { readonly session: string | null; readonly el: Element | null; readonly win: Element | null }
let panes: PaneCommands | null = null;

// The Canvas's windows: floating terminal and watch windows, and session panels (only while the Canvas is in front).
const CANVAS_WINDOWS = '.canvas .float-win, .canvas section.agent';
// As in tmux (whose floating panes resize with prefix+Ctrl/Alt+arrow): arrows move, Ctrl+arrow and Alt+arrow resize.
const CANVAS_ACTIONS = new Set<Action['kind']>(['focus', 'resize', 'next-pane', 'select']);
/** Pixels per keyboard resize of a floating window: Ctrl+arrow (tmux: 1 cell), Alt+arrow (tmux: 5). */
const RESIZE_STEP = 16;
const RESIZE_STEP_BIG = 80;

/** What a floating window offers the keymap (FloatingTerminals attaches it with `use:canvasWindow`). */
export interface WindowCommands { /** False when the window cannot be resized now (minimized). */ readonly resize: (dw: number, dh: number) => boolean }
const windowCommands = new WeakMap<Element, WindowCommands>();
/** Svelte action: registers a Canvas window's commands for as long as its element exists. */
export function canvasWindow(node: Element, commands: WindowCommands): { update: (next: WindowCommands) => void; destroy: () => void } {
  windowCommands.set(node, commands);
  return { update: next => { windowCommands.set(node, next); }, destroy: () => { windowCommands.delete(node); } };
}

// tmux's repeat-time: after an arrow command, more arrows (with the same modifiers meaning) work without the prefix
// until REPEAT_MS passes with no such key. Any other key ends it and goes where it would have gone.
let repeatTimer: ReturnType<typeof setTimeout> | undefined;
let repeating = false;
function armRepeat(): void {
  repeating = true;
  clearTimeout(repeatTimer);
  repeatTimer = setTimeout(() => { repeating = false; }, REPEAT_MS);
}
function stopRepeat(): void {
  repeating = false;
  clearTimeout(repeatTimer);
}

function canvasWindows(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>(CANVAS_WINDOWS)].filter(el => {
    if (el.closest('.background-hidden')) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  });
}
const boxOf = (el: Element): Box => { const r = el.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; };

/** Give a Canvas window the keyboard: a floating window through its grip (which raises it and makes its terminal take
 *  the keys), a session panel through its first control. */
function focusWindow(el: HTMLElement): void {
  const grip = el.querySelector<HTMLElement>('.float-grip');
  if (grip) {
    grip.focus();
    // An active floating terminal focuses itself once it is raised; a minimized one has none, so the grip keeps it.
    requestAnimationFrame(() => el.querySelector<HTMLTextAreaElement>('.xterm-helper-textarea')?.focus());
    return;
  }
  el.querySelector<HTMLElement>('button:not(:disabled), [tabindex]:not([tabindex="-1"])')?.focus();
}

/** What a Terminals-view command is called, for the message that it does not apply on the Canvas. */
function keyName(action: Action): string {
  switch (action.kind) {
    case 'split': return action.dir === 'row' ? 'split right' : 'split down';
    case 'close-pane': return 'close pane';
    case 'zoom': return 'zoom';
    case 'next-layout': return 'next layout';
    case 'new-terminal': return 'new terminal';
    case 'sync': return 'type in all panes';
    default: return action.kind;
  }
}

/** On the Canvas: prefix+arrow, +o and +1..9 move among the windows by where they are on screen; prefix+Ctrl/Alt+arrow
 *  resizes the window the key was pressed in (its bottom-right corner moves, as tmux resizes a floating pane). */
function runCanvas(action: Action, from: Element): void {
  const all = canvasWindows();
  const at = all.indexOf(from as HTMLElement);
  // The window the prefix was pressed in has gone (closed, or the Canvas left) while waiting for the command key.
  if (at < 0) return;
  if (action.kind === 'resize') {
    const commands = windowCommands.get(from);
    if (!commands) { say('Ctrl+arrow resizes floating windows; a session panel is resized from its corner.'); return; }
    const step = action.big ? RESIZE_STEP_BIG : RESIZE_STEP;
    const sign = action.dir === 'left' || action.dir === 'up' ? -1 : 1;
    const done = commands.resize(action.dir === 'left' || action.dir === 'right' ? sign * step : 0, action.dir === 'up' || action.dir === 'down' ? sign * step : 0);
    if (!done) say('A minimized window keeps its size: restore it first (its + button).');
    return;
  }
  if (all.length < 2) { say('There is no other window on the Canvas.'); return; }
  const order = readingOrder(all.map(boxOf));
  let target = -1;
  if (action.kind === 'focus') {
    const others = all.filter(el => el !== from);
    const i = nearestInDirection(boxOf(from), others.map(boxOf), action.dir);
    if (i < 0) { say(`No window ${action.dir === 'left' || action.dir === 'right' ? `to the ${action.dir}` : action.dir === 'up' ? 'above' : 'below'}.`); return; }
    target = all.indexOf(others[i]);
  } else if (action.kind === 'next-pane') {
    target = order[(order.indexOf(at) + 1) % order.length];
  } else if (action.kind === 'select') {
    target = order[action.index] ?? -1;
    if (target < 0) { say(`There are ${all.length} windows on the Canvas.`); return; }
  }
  if (target >= 0) focusWindow(all[target]);
}

function say(text: string, ms = 4000): void {
  message = text;
  clearTimeout(messageTimer);
  messageTimer = setTimeout(() => { message = null; }, ms);
}
function stopWaiting(): void {
  waiting = false;
  waitingIn = null;
  clearTimeout(waitTimer);
}

// The last place inside a terminal or a Canvas window that had the keyboard. When focus ends up nowhere in the page (a
// closed list or dialog took its focused control with it) the keys go back there, as tmux acts on the active pane.
let lastPlace: HTMLElement | null = null;
let returnFocus: HTMLElement | null = null;
function remember(event: FocusEvent): void {
  const el = event.target;
  if (el instanceof HTMLElement && el.closest(`[data-terminal], ${CANVAS_WINDOWS}`)) lastPlace = el;
}
function openHelp(): void {
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  helpOpen = true;
}
/** Close the key list and give the keyboard back to where it was before the list opened. */
function closeHelp(): void {
  helpOpen = false;
  const back = returnFocus?.isConnected ? returnFocus : lastPlace?.isConnected ? lastPlace : null;
  returnFocus = null;
  back?.focus();
}

/** Where a key was pressed: the shown terminal around the focused element and/or the Canvas window it is in. */
function terminalOf(target: EventTarget | null): Shown | null {
  if (!(target instanceof Element)) return null;
  const termEl = target.closest<HTMLElement>('[data-terminal]');
  const id = termEl?.dataset.terminal;
  const session = termEl && id && /^[0-9a-f]{8}$/.test(id) ? id : null;
  const win = target.closest(CANVAS_WINDOWS);
  // A text field (a composer, a search box) keeps its keys: Ctrl+A selects all there, Ctrl+B is its own. The emulator's
  // hidden textarea is how a terminal takes keys, so it is the one field the prefix is read in.
  const editing = !target.classList.contains('xterm-helper-textarea') && target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])') !== null;
  if (editing) return null;
  const onCanvas = win !== null && win.closest('.background-hidden') === null ? win : null;
  return session !== null || onCanvas !== null ? { session, el: session !== null ? termEl : null, win: onCanvas } : null;
}

function run(action: Action, shown: Shown, prefix: Exclude<Prefix, 'off'>): void {
  const t = shown.el ? terminals.get(shown.el) : undefined;
  const inTerminal = 'This works in a terminal: click in one first.';
  switch (action.kind) {
    case 'send-prefix':
      t?.send(PREFIX_BYTES[prefix]);
      return;
    case 'help':
      openHelp();
      return;
    case 'search':
    case 'copy-mode':
      if (t) (action.kind === 'search' ? t.search : t.copyMode)();
      else say(inTerminal);
      return;
    case 'toggle-log':
      if (shown.session) void logStore.toggle(shown.session).then(text => say(text, 8000));
      else say(inTerminal);
      return;
    default:
      if (!PANE_ACTIONS.has(action.kind)) return;
      // On the Canvas the windows sit anywhere: arrows, o and 1..9 move between them by position.
      if (shown.win) {
        if (CANVAS_ACTIONS.has(action.kind)) runCanvas(action, shown.win);
        else say(`On the Canvas the prefix moves between windows (arrows, o, 1 to 9); "${keyName(action)}" is a Terminals view command.`);
        return;
      }
      if (!(panes?.(action) ?? false)) say('Pane commands work in the Terminals view (the terminal icon in the module list).');
  }
}

function onKeydown(event: KeyboardEvent): void {
  const prefix = settingsStore.value.keys.prefix;
  if (prefix === 'off') { if (waiting) stopWaiting(); if (repeating) stopRepeat(); return; }
  if (helpOpen && event.key === 'Escape') { closeHelp(); event.preventDefault(); event.stopPropagation(); return; }
  // Repeat (tmux -r): another arrow command soon after the last one runs without the prefix, where the keyboard is now.
  if (repeating && !waiting) {
    if (isModifierKey(event.key)) return;
    const again = command(event);
    const where = terminalOf(event.target);
    if (again && repeatable(again) && where) {
      event.preventDefault();
      event.stopPropagation();
      run(again, where, prefix);
      armRepeat();
      return;
    }
    stopRepeat();
  }
  // The prefix counts only in a terminal or a Canvas window; the command key after it counts wherever focus is by then.
  let shown = waiting ? waitingIn : terminalOf(event.target);
  // The prefix pressed while nothing in the page has the keyboard (a closed list or dialog took its focused control
  // with it; the window itself still has focus): the last terminal or window used takes the keyboard back and the
  // prefix counts there. Only the prefix: other keys pressed with nothing focused are left alone.
  const nowhere = event.target === document.body || event.target === document.documentElement;
  if (shown === null && !waiting && nowhere && isPrefix(event, prefix) && lastPlace?.isConnected) {
    lastPlace.focus();
    shown = terminalOf(lastPlace);
  }
  if (shown === null) return;
  const step = interpret(waiting, event, prefix);
  if (step.swallow) { event.preventDefault(); event.stopPropagation(); }
  if (step.waiting && !waiting) {
    waiting = true;
    waitingIn = shown;
    clearTimeout(waitTimer);
    waitTimer = setTimeout(stopWaiting, PREFIX_TIMEOUT_MS);
    return;
  }
  if (step.waiting) return;
  stopWaiting();
  if (step.unknown) say(`${PREFIX_LABEL[prefix]} then "${event.key}" does nothing. Press ${PREFIX_LABEL[prefix]} then ? for the list.`);
  if (step.action) {
    run(step.action, shown, prefix);
    if (repeatable(step.action)) armRepeat();
  }
}

export const keyControl = {
  get waiting(): boolean { return waiting; },
  get message(): string | null { return message; },
  get helpOpen(): boolean { return helpOpen; },
  closeHelp,
  say,
  /** Start listening (once, from the app root). Returns the way to stop. */
  install(): () => void {
    window.addEventListener('keydown', onKeydown, true);
    window.addEventListener('focusin', remember, true);
    return () => { window.removeEventListener('keydown', onKeydown, true); window.removeEventListener('focusin', remember, true); stopWaiting(); stopRepeat(); };
  },
  /** `el` is the shown terminal's element carrying `data-terminal`. */
  registerTerminal(el: Element, commands: TerminalCommands): () => void {
    terminals.set(el, commands);
    return () => { if (terminals.get(el) === commands) terminals.delete(el); };
  },
  registerPanes(commands: PaneCommands): () => void {
    panes = commands;
    return () => { if (panes === commands) panes = null; };
  }
};
