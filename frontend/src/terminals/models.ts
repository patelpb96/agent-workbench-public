// The model picker's data: which models to offer a terminal, favourites, and the line the picker types.
// Pure and framework-free. The picker only ever FILLS the terminal's input with the line; it never sends it.
import type { AgentName } from './agents.js';

export interface ModelChoice { readonly name: string; readonly note: string }

/**
 * Claude Code accepts aliases and full model ids after `/model`. Pi and Codex open their own picker for a bare `/model`, so for
 * them the list holds only that, and anything else comes from what the person types in the search box.
 */
const CHOICES: Readonly<Record<AgentName, readonly ModelChoice[]>> = {
  claude: [
    { name: 'default', note: 'the account default' },
    { name: 'fable', note: 'most capable' },
    { name: 'opus', note: 'latest Opus' },
    { name: 'sonnet', note: 'latest Sonnet' },
    { name: 'haiku', note: 'fastest, cheapest' },
    { name: 'opusplan', note: 'Opus plans, Sonnet does' },
    { name: 'claude-fable-5-1', note: 'Fable 5.1' },
    { name: 'claude-opus-5-5', note: 'Opus 5.5' },
    { name: 'claude-sonnet-5-5', note: 'Sonnet 5.5' },
    { name: 'claude-haiku-5-5', note: 'Haiku 5.5' }
  ],
  pi: [{ name: '', note: "open Pi's own model list" }],
  codex: [{ name: '', note: "open Codex's own model list" }]
};

/** A model name is a single token of letters, digits and `._:/-` (what every agent accepts), at most 80 characters. */
export const validModelName = (name: string): boolean => name === '' || /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,79}$/.test(name);

export const modelsFor = (agent: AgentName): readonly ModelChoice[] => CHOICES[agent];

/** The text to put in the input: `/model <name>`, or just `/model` for the agent's own picker. No newline, ever. */
export const modelCommand = (name: string): string | null => (validModelName(name) ? (name === '' ? '/model' : `/model ${name}`) : null);

/**
 * What the picker lists for a search: choices matching the text (name or note, any case), favourites first, then a typed custom
 * name when it is valid and not already listed.
 */
export function visibleModels(agent: AgentName, search: string, favourites: readonly string[]): ModelChoice[] {
  const q = search.trim().toLowerCase();
  const all = [...modelsFor(agent)];
  const typed = search.trim();
  if (typed !== '' && validModelName(typed) && !all.some(m => m.name === typed)) all.push({ name: typed, note: 'type this name' });
  const hit = all.filter(m => q === '' || m.name.toLowerCase().includes(q) || m.note.toLowerCase().includes(q));
  const fav = new Set(favourites);
  return [...hit.filter(m => fav.has(m.name)), ...hit.filter(m => !fav.has(m.name))];
}

/** Favourites are kept per agent kind: `{ claude: ['opus'], ... }`. Anything unexpected in stored text is dropped. */
export type Favourites = Readonly<Partial<Record<AgentName, readonly string[]>>>;

export function parseFavourites(text: string | null): Favourites {
  if (text === null) return {};
  try {
    const raw: unknown = JSON.parse(text);
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return {};
    const out: Partial<Record<AgentName, string[]>> = {};
    for (const agent of ['claude', 'pi', 'codex'] as const) {
      const list = (raw as Record<string, unknown>)[agent];
      if (Array.isArray(list)) out[agent] = [...new Set(list.filter((n): n is string => typeof n === 'string' && validModelName(n)))].slice(0, 40);
    }
    return out;
  } catch {
    return {};
  }
}

export function toggleFavourite(favourites: Favourites, agent: AgentName, name: string): Favourites {
  const current = favourites[agent] ?? [];
  const next = current.includes(name) ? current.filter(n => n !== name) : [...current, name].slice(-40);
  return { ...favourites, [agent]: next };
}
