// Pure find-in-scrollback rules for a terminal pane (no DOM, no xterm import), modelled on tmux's
// search in copy mode: a query plus three switches, and the wording the bar shows for a result.
// Small functions over a plain state object, so the rules are testable without an emulator and
// nothing here can hold hidden state between two panes.

/** Longer queries are refused (tmux searches a line, not a whole transcript): a clipboard dump is not a query. */
export const MAX_QUERY_LENGTH = 200;
/** The addon stops collecting (and therefore reporting) matches here; the count we show is capped by it. */
export const HIGHLIGHT_LIMIT = 1000;
/** `resultCount` while no search has run: the bar shows nothing rather than "No matches". */
export const NO_RESULTS = -1;

export interface SearchState {
  readonly query: string;
  readonly caseSensitive: boolean;
  readonly regex: boolean;
  readonly wholeWord: boolean;
}

/** Mirrors the addon's (unexported) decoration options; the overview-ruler colours are required. */
export interface SearchDecorationOptions {
  matchBackground?: string;
  matchBorder?: string;
  matchOverviewRuler: string;
  activeMatchBackground?: string;
  activeMatchBorder?: string;
  activeMatchColorOverviewRuler: string;
}

/** The shape `SearchAddon.findNext/findPrevious` take (`ISearchOptions`); kept structural so this file stays pure. */
export interface SearchOptions {
  regex?: boolean;
  wholeWord?: boolean;
  caseSensitive?: boolean;
  incremental?: boolean;
  decorations?: SearchDecorationOptions;
}

// Match: the pane's muted blue (darker than the #33557f selection, so the amber active match still
// reads on top of it). Active match: the theme's amber, the same one the model picker uses for "on".
export const MATCH_COLOR = '#2b4b73';
export const ACTIVE_MATCH_COLOR = '#e5bc6e';
export const MATCH_DECORATIONS: SearchDecorationOptions = Object.freeze({
  matchBackground: MATCH_COLOR,
  matchBorder: '#5b8ac4',
  matchOverviewRuler: '#5b8ac4',
  activeMatchBackground: '#7a5200',
  activeMatchBorder: ACTIVE_MATCH_COLOR,
  activeMatchColorOverviewRuler: ACTIVE_MATCH_COLOR,
});

export function emptySearch(): SearchState {
  return { query: '', caseSensitive: false, regex: false, wholeWord: false };
}

/** One line, at most MAX_QUERY_LENGTH characters: a newline would never match a single terminal line. */
export function cleanQuery(raw: string): string {
  return raw.replace(/[\r\n\t]+/g, ' ').slice(0, MAX_QUERY_LENGTH);
}

export function withQuery(state: SearchState, raw: string): SearchState {
  const query = cleanQuery(raw);
  return query === state.query ? state : { ...state, query };
}

export function toggleCaseSensitive(state: SearchState): SearchState {
  return { ...state, caseSensitive: !state.caseSensitive };
}

export function toggleRegex(state: SearchState): SearchState {
  return { ...state, regex: !state.regex };
}

export function toggleWholeWord(state: SearchState): SearchState {
  return { ...state, wholeWord: !state.wholeWord };
}

/**
 * The message to show instead of a result count, or null when the state can be searched. A bad
 * pattern is a normal thing to type on the way to a good one, so it is reported, never thrown:
 * the addon builds a RegExp out of the query and would throw through the emulator.
 */
export function validate(state: SearchState): string | null {
  if (!state.regex || state.query.length === 0) return null;
  try {
    new RegExp(state.query);
    return null;
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    return `Invalid regex: ${detail}`;
  }
}

/** The options object for the addon; decorations are what make it report results and highlight them. */
export function searchOptions(state: SearchState, incremental = false): SearchOptions {
  return {
    caseSensitive: state.caseSensitive,
    regex: state.regex,
    wholeWord: state.wholeWord,
    incremental,
    decorations: MATCH_DECORATIONS,
  };
}

/**
 * "3 of 17" for the active match, "No matches" for a query that found nothing, "" while no search
 * has run (empty query: `count` is NO_RESULTS). An active index outside the counted matches — the
 * addon reports -1 when it cannot place the selection in the highlighted set — leaves the total.
 */
export function resultText(index: number, count: number): string {
  if (!Number.isFinite(count) || count < 0) return '';
  if (count === 0) return 'No matches';
  if (Number.isFinite(index) && index >= 0 && index < count) return `${Math.floor(index) + 1} of ${count}`;
  return `${count} match${count === 1 ? '' : 'es'}`;
}

/** True when the query is worth handing to the addon (non-empty and valid). */
export function canSearch(state: SearchState): boolean {
  return state.query.length > 0 && validate(state) === null;
}

/**
 * Whether a result report from the addon belongs to the current query. `clearDecorations()` emits
 * `{ resultIndex: -1, resultCount: 0 }`, so after clearing for an empty or invalid query the addon's
 * next event would otherwise claim "No matches" for a search that never ran, or paper over the
 * validation message. A count is only meaningful while the query itself is searchable.
 */
export function acceptsResults(state: SearchState): boolean {
  return canSearch(state);
}

/**
 * What the bar shows under the field: the validation message while the pattern is bad, nothing for an
 * empty query (there is no result to report yet), otherwise the count. Query-aware on purpose: the
 * count alone cannot tell "nothing found" from "nothing searched".
 */
export function resultLabel(state: SearchState, index: number, count: number): string {
  const message = validate(state);
  if (message !== null) return message;
  if (!acceptsResults(state)) return '';
  return resultText(index, count);
}
