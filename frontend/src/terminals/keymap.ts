// tmux-style keyboard control for terminals (docs/design/tmux-parity-plan.md, wave 2): a prefix key (Ctrl+B by
// default, as in tmux), then one command key. Pure: no DOM. The listener that feeds key presses in and carries the
// actions out is in keys.svelte.ts.
//
// Only the prefix is ever taken from the terminal, and only when it is pressed in a terminal; pressing it twice sends
// it through (Ctrl+B Ctrl+B types Ctrl+B), and it can be turned off in Properties > Terminals.

export type Prefix = 'ctrl+b' | 'ctrl+a' | 'off';
export const PREFIXES: readonly Prefix[] = ['ctrl+b', 'ctrl+a', 'off'];
export const PREFIX_LABEL: Readonly<Record<Prefix, string>> = { 'ctrl+b': 'Ctrl+B', 'ctrl+a': 'Ctrl+A', off: 'Off' };
/** What a terminal receives when the prefix is pressed twice (the control character the key stands for). */
export const PREFIX_BYTES: Readonly<Record<Exclude<Prefix, 'off'>, string>> = { 'ctrl+b': '\u0002', 'ctrl+a': '\u0001' };
/** How long the prefix waits for its command key, as tmux's prompt does not: a forgotten prefix should not linger. */
export const PREFIX_TIMEOUT_MS = 2500;

export type Direction = 'left' | 'right' | 'up' | 'down';
export type Action =
  | { readonly kind: 'split'; readonly dir: 'row' | 'column' }
  | { readonly kind: 'focus'; readonly dir: Direction }
  | { readonly kind: 'resize'; readonly dir: Direction; /** Alt+arrow: tmux's resize by 5 instead of 1. */ readonly big: boolean }
  | { readonly kind: 'select'; readonly index: number }
  | { readonly kind: 'next-pane' }
  | { readonly kind: 'close-pane' }
  | { readonly kind: 'zoom' }
  | { readonly kind: 'next-layout' }
  | { readonly kind: 'new-terminal' }
  | { readonly kind: 'copy-mode' }
  | { readonly kind: 'search' }
  | { readonly kind: 'toggle-log' }
  | { readonly kind: 'sync' }
  | { readonly kind: 'help' }
  | { readonly kind: 'send-prefix' };

/** The parts of a KeyboardEvent this module reads. */
export interface KeyInput {
  readonly key: string;
  readonly ctrlKey: boolean;
  readonly altKey: boolean;
  readonly shiftKey: boolean;
  readonly metaKey: boolean;
}

export function isPrefixSetting(v: unknown): v is Prefix {
  return typeof v === 'string' && (PREFIXES as readonly string[]).includes(v);
}

/** Is this key press the prefix? Exactly Ctrl and the letter: Ctrl+Shift+B or Ctrl+Alt+B are left to the terminal. */
export function isPrefix(k: KeyInput, prefix: Prefix): boolean {
  if (prefix === 'off' || !k.ctrlKey || k.altKey || k.metaKey || k.shiftKey) return false;
  return k.key.toLowerCase() === (prefix === 'ctrl+b' ? 'b' : 'a');
}

const ARROWS: Readonly<Record<string, Direction>> = { ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' };
const MODIFIERS = new Set(['Shift', 'Control', 'Alt', 'Meta', 'AltGraph', 'CapsLock']);

/** A modifier key pressed on its own (it neither runs nor ends anything). */
export function isModifierKey(key: string): boolean {
  return MODIFIERS.has(key);
}

/** tmux's repeat-time: after a repeatable command (the arrow ones, bound with `-r` in tmux), more of them work without
 *  the prefix for this long after the last one. */
export const REPEAT_MS = 500;
export function repeatable(action: Action): boolean {
  return action.kind === 'focus' || action.kind === 'resize';
}

/** The command a key stands for after the prefix (tmux's default bindings where tmux has one), or null. As in tmux:
 *  arrow moves to the pane that way, Ctrl+arrow resizes by a little, Alt+arrow by more. */
export function command(k: KeyInput): Action | null {
  const dir = ARROWS[k.key];
  if (dir !== undefined) {
    if (k.metaKey || (k.ctrlKey && k.altKey)) return null;
    if (k.ctrlKey) return { kind: 'resize', dir, big: false };
    if (k.altKey) return { kind: 'resize', dir, big: true };
    return { kind: 'focus', dir };
  }
  if (k.ctrlKey || k.altKey || k.metaKey) return null;
  if (/^[0-9]$/.test(k.key)) return { kind: 'select', index: Number(k.key) === 0 ? 9 : Number(k.key) - 1 };
  switch (k.key) {
    case '%': return { kind: 'split', dir: 'row' };
    case '"': return { kind: 'split', dir: 'column' };
    case 'o': return { kind: 'next-pane' };
    case 'x': return { kind: 'close-pane' };
    case 'z': return { kind: 'zoom' };
    case ' ': return { kind: 'next-layout' };
    case 'c': return { kind: 'new-terminal' };
    case '[': return { kind: 'copy-mode' };
    case '/': return { kind: 'search' };
    case 'L': return { kind: 'toggle-log' };
    case 'S': return { kind: 'sync' };
    case '?': return { kind: 'help' };
    default: return null;
  }
}

export interface Step {
  /** Still waiting for the command key after this press. */
  readonly waiting: boolean;
  /** Keep this press away from the terminal (and everything else). */
  readonly swallow: boolean;
  readonly action: Action | null;
  /** A key after the prefix that is not bound (tmux beeps; we say so). */
  readonly unknown: boolean;
}

const pass: Step = { waiting: false, swallow: false, action: null, unknown: false };

/** One key press: `waiting` is whether the prefix was pressed before it. */
export function interpret(waiting: boolean, k: KeyInput, prefix: Prefix): Step {
  if (prefix === 'off') return pass;
  if (!waiting) return isPrefix(k, prefix) ? { waiting: true, swallow: true, action: null, unknown: false } : pass;
  // Shift (to type % or ") and the other modifiers on their own keep the prefix waiting.
  if (isModifierKey(k.key)) return { waiting: true, swallow: false, action: null, unknown: false };
  if (isPrefix(k, prefix)) return { waiting: false, swallow: true, action: { kind: 'send-prefix' }, unknown: false };
  if (k.key === 'Escape') return { waiting: false, swallow: true, action: null, unknown: false };
  const action = command(k);
  return { waiting: false, swallow: true, action, unknown: action === null };
}

/** The bindings, as the help list shows them (prefix first, then the key). */
export const BINDINGS: readonly { readonly keys: string; readonly what: string }[] = [
  { keys: '%', what: 'Split the pane to the right' },
  { keys: '"', what: 'Split the pane downwards' },
  { keys: 'Arrow keys', what: 'Move to the pane in that direction' },
  { keys: 'Ctrl+arrow keys', what: 'Resize the pane (on the Canvas, the window)' },
  { keys: 'Alt+arrow keys', what: 'Resize by more' },
  { keys: 'then arrows again', what: 'For half a second after an arrow command, more arrows work without the prefix' },
  { keys: 'o', what: 'Next pane' },
  { keys: '1 to 9, 0', what: 'Go to pane 1 to 9, 10' },
  { keys: 'z', what: 'Zoom the pane to the whole view, and back' },
  { keys: 'x', what: 'Close the pane (the terminal keeps running)' },
  { keys: 'Space', what: 'Next layout (even, main, tiled)' },
  { keys: 'c', what: 'New terminal' },
  { keys: '[', what: 'Copy mode: move through the scrollback, select and copy' },
  { keys: '/', what: 'Search the scrollback' },
  { keys: 'S', what: 'Type into every pane at once (on/off)' },
  { keys: 'L', what: 'Log this terminal to a file (on/off)' },
  { keys: '?', what: 'This list' },
  { keys: 'the prefix again', what: 'Send the prefix key itself to the terminal' },
  { keys: 'Escape', what: 'Cancel' }
];
