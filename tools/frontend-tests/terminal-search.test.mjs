// Pure tests of the terminal pane's find-in-scrollback rules (query limits, patterns, result text).
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const s = require(`${root}artifacts/frontend-contract-src/terminals/search.js`);

const query = (text, over = {}) => ({ ...s.emptySearch(), query: text, ...over });

test('a query is one line and at most MAX_QUERY_LENGTH characters', () => {
  assert.equal(s.emptySearch().query, '');
  assert.equal(s.MAX_QUERY_LENGTH, 200);
  assert.equal(s.cleanQuery('x'.repeat(200)).length, 200, 'a full query is kept');
  assert.equal(s.cleanQuery('x'.repeat(250)).length, 200, 'longer is cut, not refused');
  assert.equal(s.cleanQuery('a\r\nb\tc'), 'a b c', 'newlines and tabs cannot reach a one-line search');
  const state = s.withQuery(s.emptySearch(), 'y'.repeat(500));
  assert.equal(state.query.length, 200);
  const same = s.withQuery(state, state.query);
  assert.equal(same, state, 'an unchanged query returns the same state');
});

test('the three switches flip without mutating the state they came from', () => {
  const base = s.emptySearch();
  assert.deepEqual([base.caseSensitive, base.regex, base.wholeWord], [false, false, false]);
  const cased = s.toggleCaseSensitive(base);
  const regexed = s.toggleRegex(base);
  const worded = s.toggleWholeWord(base);
  assert.equal(cased.caseSensitive, true);
  assert.equal(regexed.regex, true);
  assert.equal(worded.wholeWord, true);
  assert.deepEqual(base, s.emptySearch(), 'the original state is untouched');
  assert.equal(s.toggleRegex(regexed).regex, false, 'a second toggle turns it back off');
});

test('an invalid regex is reported as a message, never thrown', () => {
  const bad = query('(', { regex: true });
  let message;
  assert.doesNotThrow(() => { message = s.validate(bad); });
  assert.equal(typeof message, 'string');
  assert.match(message, /^Invalid regex: /);
  assert.equal(s.validate(query('(a+)+', { regex: true })), null, 'a valid pattern passes');
  assert.equal(s.validate(query('(', { regex: false })), null, 'the same text is fine as plain text');
  assert.equal(s.validate(s.emptySearch()), null, 'an empty query is nothing to complain about');
  assert.equal(s.canSearch(bad), false);
  assert.equal(s.canSearch(query('(a+)', { regex: true })), true);
  assert.equal(s.canSearch(s.emptySearch()), false, 'an empty query is never searched');
});

test('resultText says where in the scrollback the active match is', () => {
  assert.equal(s.resultText(-1, s.NO_RESULTS), '', 'no search has run yet');
  assert.equal(s.resultText(-1, 0), 'No matches');
  assert.equal(s.resultText(0, 17), '1 of 17');
  assert.equal(s.resultText(2, 17), '3 of 17');
  assert.equal(s.resultText(16, 17), '17 of 17');
  assert.equal(s.resultText(-1, 3), '3 matches', 'an unplaced active match still shows the total');
  assert.equal(s.resultText(-1, 1), '1 match');
  assert.equal(s.resultText(2.7, 17), '3 of 17', 'a fractional index is floored');
  assert.equal(s.resultText(99, 17), '17 matches', 'an out-of-range index does not invent a position');
});

test('an empty or invalid query never shows a count, even when the addon reports zero results', () => {
  // clearDecorations() emits { resultIndex: -1, resultCount: 0 }: that must not read "No matches".
  assert.equal(s.resultLabel(s.emptySearch(), -1, 0), '', 'an empty query stays blank');
  assert.equal(s.resultLabel(s.emptySearch(), -1, s.NO_RESULTS), '');
  const bad = query('(', { regex: true });
  const message = s.resultLabel(bad, -1, 0);
  assert.match(message, /^Invalid regex: /, 'a bad pattern keeps its message, not "No matches"');
  // A cleared search also fires a result report; it does not belong to a query that can be searched.
  assert.equal(s.acceptsResults(s.emptySearch()), false);
  assert.equal(s.acceptsResults(bad), false);
  assert.equal(s.acceptsResults(query('ERROR')), true);
  // A real query with no hits still says so.
  assert.equal(s.resultLabel(query('ERROR'), -1, 0), 'No matches');
  assert.equal(s.resultLabel(query('ERROR'), 2, 17), '3 of 17');
  assert.equal(s.resultLabel(query('ERROR'), -1, s.NO_RESULTS), '', 'nothing searched yet');
  assert.equal(s.resultLabel(query('('), -1, 0), 'No matches', 'plain text is searchable whatever it contains');
});

test('the addon gets exactly the switches, the decorations and the theme colours', () => {
  const plain = s.searchOptions(s.emptySearch());
  assert.deepEqual(
    { caseSensitive: plain.caseSensitive, regex: plain.regex, wholeWord: plain.wholeWord, incremental: plain.incremental },
    { caseSensitive: false, regex: false, wholeWord: false, incremental: false },
  );
  const full = s.searchOptions(query('ERROR', { caseSensitive: true, regex: true, wholeWord: true }), true);
  assert.equal(full.caseSensitive, true);
  assert.equal(full.regex, true);
  assert.equal(full.wholeWord, true);
  assert.equal(full.incremental, true);
  assert.equal(full.decorations.matchBackground, s.MATCH_COLOR);
  assert.equal(full.decorations.activeMatchBorder, s.ACTIVE_MATCH_COLOR);
  assert.match(full.decorations.matchBackground, /^#[0-9a-f]{6}$/, 'the addon only accepts #RRGGBB');
  assert.match(full.decorations.activeMatchBackground, /^#[0-9a-f]{6}$/);
  assert.ok(full.decorations.matchOverviewRuler && full.decorations.activeMatchColorOverviewRuler,
    'the ruler colours the addon requires are present');
  assert.equal(s.MATCH_DECORATIONS.matchBackground, '#2b4b73', 'muted blue for matches');
  assert.equal(s.MATCH_DECORATIONS.activeMatchBorder, '#e5bc6e', 'amber for the active match');
});
