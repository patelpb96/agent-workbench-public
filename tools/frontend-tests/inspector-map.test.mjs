// The agent map's model: a compact block of agents in the middle, tools outside it on two rails, integer geometry,
// determinism and unique ids.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const require = createRequire(import.meta.url);
const root = fileURLToPath(new URL('../../', import.meta.url));
const m = require(`${root}artifacts/frontend-contract-src/inspector/map.js`);

const NOW = 1_792_000_000_000;
const MIN = 60_000;
let n = 0;
const id = () => (++n).toString(16).padStart(16, '0');
const src = (over = {}) => ({ id: id(), agent: 'claude', label: 'C--Users-me-proj', modified_ms: NOW - 10 * MIN, bytes: 5000, active: false, running: null, model: null, last_line: null, folder: 'proj', kind: 'session', parent: null, ...over });
const tool = (name, calls = 1, errors = 0) => ({ name, calls, errors });
const build = (sources, over = {}) => m.buildMap({ sources, now: NOW, box: { width: 300, height: 400 }, ...over });
const agentsOf = map => map.nodes.filter(x => x.source !== null);
const toolsOf = map => map.nodes.filter(x => x.kind === 'tool');
const orthogonal = points => points.every((p, i) => i === 0 || p[0] === points[i - 1][0] || p[1] === points[i - 1][1]);

test('levels and tool groups follow the documented rules', () => {
  assert.deepEqual([479, 480, 999, 1000].map(m.levelFor), ['narrow', 'medium', 'medium', 'wide']);
  assert.equal(m.toolGroup('mcp__github__search'), 'mcp:github');
  assert.equal(m.toolGroup('mcp__my_server__a_b'), 'mcp:my_server');
  assert.equal(m.toolGroup('Bash'), 'shell');
  assert.equal(m.toolGroup('Read'), 'file');
  assert.equal(m.toolGroup('WebFetch'), 'web');
  assert.equal(m.toolGroup('Task'), 'agent');
  assert.equal(m.toolGroup('SomethingNew'), 'other');
  assert.equal(m.toolGroup('mcp__'), 'other');
});

test('no sources draws nothing but fills the box width; a few sessions sit side by side in one block', () => {
  const empty = build([]);
  assert.deepEqual([empty.nodes.length, empty.edges.length, empty.frame], [0, 0, null]);
  assert.deepEqual([empty.width, empty.height], [300, 16]);
  assert.equal(empty.summary, '0 agents (0 active), 0 subagents, 0 tools');

  const list = [src(), src(), src()];
  const map = build(list);
  assert.equal(map.level, 'narrow');
  assert.deepEqual(map.nodes.map(x => x.id), list.map(s => `s:${s.id}`));
  const [a, b, c] = map.nodes;
  // one row, a glyph and a family gap apart (20 + 12)
  assert.deepEqual([a.y === b.y && b.y === c.y, b.x - a.x, c.x - b.x], [true, 32, 32]);
  for (const x of map.nodes) assert.deepEqual([x.w, x.h, x.depth, x.glyph.shape, x.kind], [20, 20, 0, 'square', 'session']);
  // framed with 6 px around them, centred in the box, and the picture is as wide as the box
  assert.deepEqual([map.frame.w, map.frame.h], [84 + 12, 20 + 12]);
  assert.equal(map.frame.x, a.x - 6);
  assert.equal(map.frame.y, a.y - 6);
  assert.ok(Math.abs(map.frame.x * 2 + map.frame.w - 300) <= 1, 'centred');
  assert.deepEqual([map.width, map.height], [300, 32 + 16]);
  assert.equal(map.edges.length, 0);
});

test('many sessions wrap into rows instead of one long column: 80 sessions in a narrow pane stay as wide as the pane', () => {
  const twenty = build(Array.from({ length: 20 }, () => src()));
  const rowsOf = map => [...new Set(agentsOf(map).map(x => x.y))];
  // 272 px of room holds 8 families of 20 px with 12 px between them: 3 rows, 30 px apart
  assert.equal(agentsOf(twenty).filter(x => x.y === agentsOf(twenty)[0].y).length, 8);
  assert.deepEqual(rowsOf(twenty).map((y, i, all) => y - all[0]), [0, 30, 60]);
  assert.equal(twenty.width, 300);
  const eighty = build(Array.from({ length: 80 }, () => src()));
  assert.equal(eighty.width, 300);
  assert.equal(eighty.height, 20 * 10 + 9 * 10 + 12 + 16, '10 rows, not 80');
  // a wider box gets a bigger block, but one of a sensible shape in the middle (about 1.8 : 1), not a thin banner across it
  const wide = build(Array.from({ length: 80 }, () => src()), { box: { width: 1200, height: 600 } });
  assert.equal(wide.width, 1200);
  assert.ok(wide.height < eighty.height, `${wide.height} vs ${eighty.height}`);
  assert.ok(wide.frame.w <= 360 && wide.frame.w > wide.frame.h, `${wide.frame.w} x ${wide.frame.h}`);
  assert.ok(Math.abs(wide.frame.x * 2 + wide.frame.w - 1200) <= 1, 'centred in the box');
  const huge = build(Array.from({ length: 80 }, () => src()), { box: { width: 2600, height: 600 } });
  assert.equal(huge.frame.w, wide.frame.w, 'more room does not stretch the block');
});

test('a family is a session with its subagents and roster reports beside it, joined by lines through the gutter below the row', () => {
  const parent = src({ folder: 'workbench' });
  const child = src({ kind: 'subagent', parent: parent.id, folder: null });
  const orphan = src({ kind: 'subagent', parent: 'ffffffffffffffff' });
  const lone = src();
  const map = build([parent, lone, child, orphan]);
  const at = x => map.nodes.find(q => q.id === `s:${x.id}`);
  // the subagent is next to its parent, ahead of the next family, one level deeper, drawn as a diamond named after the parent
  assert.deepEqual(map.nodes.map(q => q.source), [parent.id, child.id, lone.id, orphan.id]);
  assert.equal(at(child).x - at(parent).x, 26);
  assert.deepEqual([at(child).depth, at(child).glyph.shape, at(child).kind], [1, 'diamond', 'subagent']);
  assert.ok(at(child).label.startsWith('workbench'), at(child).label);
  // an orphan (parent not listed) is a family of its own, with no line
  assert.equal(at(orphan).depth, 0);
  assert.deepEqual(map.edges.map(e => [e.from, e.to, e.kind]), [[`s:${parent.id}`, `s:${child.id}`, 'spawn']]);
  const e = map.edges[0];
  assert.equal(e.points.length, 4);
  assert.ok(orthogonal(e.points));
  assert.deepEqual(e.points[0], [at(parent).x + 10, at(parent).y + 20], 'leaves the parent from its bottom centre');
  assert.deepEqual(e.points[3], [at(child).x + 10, at(child).y + 20], 'enters the child from its bottom centre');
  assert.ok(e.points[1][1] > at(parent).y + 20 && e.points[1][1] < at(parent).y + 30, 'runs in the gutter between rows');
  assert.equal(map.summary, '4 agents (0 active), 2 subagents, 0 tools');

  // a roster chain a -> b -> c nests three deep in one row; a cyclic roster ends with exactly one line between the pair
  const [a, b, c] = [src(), src(), src()];
  const chain = build([a, b, c], { reports: [{ manager: a.id, report: b.id }, { manager: b.id, report: c.id }] });
  assert.deepEqual(chain.nodes.map(q => q.depth), [0, 1, 2]);
  assert.deepEqual(chain.edges.map(q => [q.kind, q.from, q.to]), [['reports', `s:${a.id}`, `s:${b.id}`], ['reports', `s:${b.id}`, `s:${c.id}`]]);
  const cyc = build([a, b], { reports: [{ manager: a.id, report: b.id }, { manager: b.id, report: a.id }] });
  assert.equal(cyc.edges.length, 1);
  assert.deepEqual(cyc.nodes.map(q => q.depth), [0, 1]);
  const odd = build([a, b], { reports: [{ manager: a.id, report: a.id }, { manager: 'ffffffffffffffff', report: b.id }] });
  assert.equal(odd.edges.length, 0);
  // two subagents naming each other cannot form a loop: all three drawn once
  const x = src({ kind: 'subagent' }), y = src({ kind: 'subagent' });
  assert.equal(new Set(build([{ ...x, parent: y.id }, { ...y, parent: x.id }]).nodes.map(q => q.id)).size, 2);
});

test('tools sit outside the block on two rails, and what is drawn depends on the width', () => {
  const s = src(), t = src();
  const facts = {
    [s.id]: { open_calls: 0, tools: [tool('Bash', 5), tool('Read', 9), tool('WebFetch', 2), tool('mcp__gh__a', 2), tool('mcp__gh__b', 3), tool('mcp__db__q', 1)] },
    [t.id]: { open_calls: 0, tools: [tool('Bash', 1), tool('Edit', 4)] }
  };
  const at = width => build([s, t], { facts, box: { width, height: 400 } });
  const labels = map => toolsOf(map).map(x => x.label);
  assert.deepEqual(labels(at(300)), ['file', 'shell', 'web', 'mcp'], 'narrow: one node per family, all MCP servers together');
  assert.deepEqual(labels(at(700)), ['file', 'shell', 'web', 'mcp:gh', 'mcp:db'], 'medium: each MCP server apart, busiest first');
  assert.deepEqual(labels(at(1200)), ['Read', 'Edit', 'Bash', 'WebFetch', 'gh', 'db'], 'wide: one node per tool, MCP tools collapsed to the server, busiest first within a family');

  const map = at(1200);
  const { frame } = map;
  const tools = toolsOf(map);
  // the first half is on the left, the rest on the right, both outside the frame
  assert.deepEqual(tools.map(x => (x.x + x.w <= frame.x ? 'L' : x.x >= frame.x + frame.w ? 'R' : '?')), ['L', 'L', 'L', 'R', 'R', 'R']);
  for (const side of ['L', 'R']) {
    const col = tools.filter((x, i) => (side === 'L') === (i < 3));
    assert.equal(new Set(col.map(x => x.x)).size, 1, 'one column');
    for (let i = 1; i < col.length; i++) assert.ok(col[i].y - col[i - 1].y >= 18, 'no overlap');
  }
  // the agents are inside the frame; the facts line says who uses it
  for (const a of agentsOf(map)) assert.ok(a.x >= frame.x && a.x + a.w <= frame.x + frame.w && a.y >= frame.y && a.y + a.h <= frame.y + frame.h);
  assert.equal(map.nodes.find(x => x.id === 't:Bash').detail, '2 agents, 6 calls');
  assert.equal(map.nodes.find(x => x.id === 't:mcp:gh').detail, '1 agent, 5 calls');
  // each tool has a stub to its rail, each rail has a spine and a port into the block; all straight
  const rails = map.edges.filter(e => e.kind === 'rail');
  assert.equal(rails.filter(e => e.to.startsWith('rail:') && e.from.startsWith('t:')).length, 6);
  assert.equal(rails.filter(e => e.to === 'frame').length, 2);
  assert.ok(rails.every(e => orthogonal(e.points)));
  assert.equal(map.edges.filter(e => e.kind === 'call').length, 0, 'no routes until an agent is selected');
  // the cap on tool nodes
  const many = src();
  const big = build([many], { facts: { [many.id]: { open_calls: 0, tools: Array.from({ length: 40 }, (_, i) => tool(`T${String(i + 1).padStart(2, '0')}`, i + 1)) } }, box: { width: 1200, height: 600 } });
  assert.equal(toolsOf(big).length, 24);
  assert.ok(toolsOf(big).some(x => x.label === 'T40') && !toolsOf(big).some(x => x.label === 'T16'), 'the busiest 24 are kept');
  // equal calls are ordered by name
  const tie = build([many], { facts: { [many.id]: { open_calls: 0, tools: [tool('Zed', 3), tool('Alpha', 3)] } }, box: { width: 1200, height: 400 } });
  assert.deepEqual(labels(tie), ['Alpha', 'Zed']);
});

test('selecting an agent draws a route from it, under its row and along the rail, to each tool it used and no other', () => {
  const s = src(), t = src();
  const facts = { [s.id]: { open_calls: 0, tools: [tool('Bash', 5), tool('Read', 9), tool('WebFetch', 2)] }, [t.id]: { open_calls: 0, tools: [tool('Edit', 1)] } };
  const map = build([s, t], { facts, box: { width: 1200, height: 400 }, selected: s.id });
  const calls = map.edges.filter(e => e.kind === 'call');
  assert.deepEqual(calls.map(e => e.to).sort(), ['t:Bash', 't:Read', 't:WebFetch']);
  const me = map.nodes.find(x => x.id === `s:${s.id}`);
  const tools = toolsOf(map);
  for (const e of calls) {
    assert.equal(e.from, me.id);
    assert.ok(orthogonal(e.points), JSON.stringify(e.points));
    assert.deepEqual(e.points[0], [me.x + 10, me.y + 20], 'starts at the agent');
    const target = tools.find(x => x.id === e.to);
    const end = e.points.at(-1);
    assert.equal(end[1], target.y + 6, 'ends at the middle of the tool');
    assert.ok(end[0] === target.x || end[0] === target.x + 12, 'touches the tool');
    assert.ok(e.points.every(p => Number.isInteger(p[0]) && Number.isInteger(p[1])));
  }
  assert.equal(map.edges.filter(e => e.kind === 'call').length, 3, 'the other agent has no routes');
  assert.equal(build([s, t], { facts, box: { width: 1200, height: 400 }, selected: null }).edges.filter(e => e.kind === 'call').length, 0);
});

test('glyphs show state, in-flight work, errors and the selection', () => {
  const active = src({ modified_ms: NOW - 10_000 }), idle = src(), inactive = src({ modified_ms: NOW - 5 * 3_600_000 }), other = src({ bytes: 10 });
  const facts = { [active.id]: { open_calls: 2, tools: [tool('Bash', 3, 1)] }, [idle.id]: { open_calls: 0, tools: [tool('Read', 1)] } };
  const map = build([active, idle, inactive, other], { facts, selected: idle.id });
  const g = q => map.nodes.find(x => x.id === `s:${q.id}`).glyph;
  assert.deepEqual([g(active).fill, g(idle).fill, g(inactive).fill, g(other).fill], ['solid', 'hollow', 'dashed', 'empty']);
  assert.deepEqual([g(active).inFlight, g(active).errors, g(idle).inFlight, g(idle).errors], [true, true, false, false]);
  assert.deepEqual([g(active).selected, g(idle).selected, g(inactive).selected], [false, true, false]);
  assert.equal(g(active).hue, '#e0925f', 'agents use the agent colour');
  const shell = toolsOf(map).find(x => x.label === 'shell');
  assert.deepEqual([shell.glyph.fill, shell.glyph.errors, shell.glyph.shape], ['dashed', true, 'chevron'], 'a tool with failed calls is dashed');
  assert.equal(toolsOf(map).find(x => x.label === 'file').glyph.fill, 'hollow');
  assert.equal(map.summary, '4 agents (1 active), 0 subagents, 2 tools');
});

test('everything is integer, ids are unique, and everything stays inside the picture with its padding', () => {
  const a = src(), b = src();
  const names = Array.from({ length: 8 }, (_, i) => tool(`Tool${i}`, i + 1));
  const facts = { [a.id]: { open_calls: 0, tools: names }, [b.id]: { open_calls: 0, tools: names } };
  const kid = src({ kind: 'subagent', parent: a.id });
  for (const width of [200, 300, 700, 1200, 2400]) {
    const map = build([a, kid, b, ...Array.from({ length: 30 }, () => src())], { facts, box: { width, height: 500 }, selected: a.id });
    assert.equal(new Set(map.nodes.map(x => x.id)).size, map.nodes.length, 'no duplicate ids');
    for (const x of map.nodes) for (const v of [x.x, x.y, x.w, x.h]) assert.ok(Number.isInteger(v), `${width} ${x.id} ${v}`);
    for (const e of map.edges) for (const p of e.points) for (const v of p) assert.ok(Number.isInteger(v), `${width} ${e.from} ${v}`);
    assert.ok(Number.isInteger(map.width) && Number.isInteger(map.height));
    assert.ok(map.width >= width, 'as wide as the box');
    for (const x of map.nodes) assert.ok(x.x >= 8 && x.y >= 8 && x.x + x.w <= map.width - 8 && x.y + x.h <= map.height - 8, `${width} ${x.id} inside the padding`);
    for (const e of map.edges) for (const p of e.points) assert.ok(p[0] >= 0 && p[0] <= map.width && p[1] >= 0 && p[1] <= map.height);
    assert.ok(map.nodes.slice(0, agentsOf(map).length).every(x => x.source !== null), 'agents first, then tools');
  }
});

test('the same input gives the same output and the input is never changed', () => {
  const a = src(), b = src({ modified_ms: NOW - 3 * MIN });
  const input = { sources: [a, b], now: NOW, box: { width: 1200, height: 500 }, selected: a.id, facts: { [a.id]: { open_calls: 1, tools: [tool('Bash', 2, 1), tool('Read', 5)] } }, reports: [{ manager: a.id, report: b.id }] };
  const before = JSON.stringify(input);
  assert.deepEqual(m.buildMap(input), m.buildMap(input));
  assert.equal(JSON.stringify(input), before, 'nothing in the input changed');
  // the caller's order is kept among families
  const flipped = m.buildMap({ ...input, sources: [b, a], reports: [] });
  assert.deepEqual(flipped.nodes.filter(x => x.source !== null).map(x => x.source), [b.id, a.id]);
  // the same source twice is drawn once
  assert.equal(agentsOf(build([a, a])).length, 1);
});

test('tools are named and sized by use, what is running now is marked and routed whoever is selected, and the pointed-at agent gets its routes', () => {
  const a = src(), b = src();
  const facts = {
    [a.id]: { open_calls: 1, open_tools: ['Bash'], tools: [tool('Bash', 5), tool('Read', 9), tool('WebFetch', 2)] },
    [b.id]: { open_calls: 0, open_tools: [], tools: [tool('Read', 3), tool('AVeryLongToolNameThatDoesNotFit', 1)] }
  };
  const wide = build([a, b], { facts, box: { width: 1200, height: 400 } });
  const tools = toolsOf(wide);
  const byLabel = x => tools.find(q => q.label === x);
  // names beside the glyphs, cut to fit with an ellipsis; sides follow the columns
  assert.equal(byLabel('Read').text, 'Read');
  const long = byLabel('AVeryLongToolNameThatDoesNotFit');
  assert.equal(long.text, 'AVeryLongTool…');
  assert.ok(long.text.length <= 14);
  assert.deepEqual(tools.map(q => q.side), ['left', 'left', 'right', 'right']);
  const narrow = toolsOf(build([a, b], { facts, box: { width: 300, height: 400 } }));
  assert.ok(narrow.every(q => q.text.length <= 9), 'a narrow map has shorter names');
  // the bars are in proportion to the busiest tool: Read has 12 calls, so it is the full 92
  assert.equal(byLabel('Read').bar, 92);
  assert.equal(byLabel('Bash').bar, Math.round(92 * 5 / 12));
  assert.ok(tools.every(q => q.bar >= 2 && q.bar <= 92));
  assert.deepEqual(byLabel('Read').stats, { agents: 2, calls: 12, errors: 0 });
  assert.deepEqual(byLabel('Read').users, [a.id, b.id], 'in the caller order');
  assert.deepEqual(byLabel('WebFetch').users, [a.id]);
  // the pictures keep every name inside the padding at both ends
  for (const q of tools) {
    if (q.side === 'left') assert.ok(q.x - 6 - 92 >= 8, `${q.label} ${q.x}`);
    else assert.ok(q.x + q.w + 6 + 92 <= wide.width - 8, `${q.label} ${q.x}`);
  }
  // what is running now: the tool is live and solid, and a route runs to it with nothing selected
  assert.deepEqual([byLabel('Bash').live, byLabel('Bash').glyph.fill, byLabel('Read').live], [true, 'solid', false]);
  const live = wide.edges.filter(e => e.kind === 'live');
  assert.deepEqual(live.map(e => [e.from, e.to]), [[`s:${a.id}`, 't:Bash']]);
  assert.ok(orthogonal(live[0].points) && live[0].points.length >= 4);
  assert.equal(wide.edges.filter(e => e.kind === 'call').length, 0);
  // an agent's fact line says what it is running and what it uses most
  assert.equal(wide.nodes.find(x => x.id === `s:${a.id}`).detail, 'running Bash - used Read 9, Bash 5, WebFetch 2');
  assert.equal(wide.nodes.find(x => x.id === `s:${b.id}`).detail, 'used Read 3, AVeryLongToolNameThatDoesNotFit 1');
  assert.match(byLabel('Bash').detail, /running now$/);
  // the pointed-at agent gets routes to its tools; the selected agent keeps its own; the same agent is drawn once
  const calls = map => map.edges.filter(e => e.kind === 'call').map(e => `${e.from}>${e.to}`).sort();
  assert.deepEqual(calls(build([a, b], { facts, box: { width: 1200, height: 400 }, highlight: b.id })), [`s:${b.id}>t:AVeryLongToolNameThatDoesNotFit`, `s:${b.id}>t:Read`].sort());
  assert.equal(calls(build([a, b], { facts, box: { width: 1200, height: 400 }, selected: a.id, highlight: a.id })).length, 3);
  assert.equal(calls(build([a, b], { facts, box: { width: 1200, height: 400 }, selected: a.id, highlight: b.id })).length, 5);
  // a running tool that has no node (cut by the cap) draws no route and no error
  const lots = Array.from({ length: 30 }, (_, i) => tool(`T${String(i).padStart(2, '0')}`, 100 - i));
  const capped = build([a], { facts: { [a.id]: { open_calls: 1, open_tools: ['Obscure'], tools: [...lots, tool('Obscure', 1)] } }, box: { width: 1200, height: 400 } });
  assert.equal(capped.edges.filter(e => e.kind === 'live').length, 0);
  assert.equal(toolsOf(capped).length, 24);
});

test('a map laid out small to be drawn scaled up keeps the detail of the width it is shown at', () => {
  const s = src();
  const facts = { [s.id]: { tools: [tool('Read', 3), tool('Edit', 2), tool('mcp__github__search', 1)], open_calls: 0 } };
  const small = build([s], { facts, box: { width: 300, height: 200 } });
  assert.equal(small.level, 'narrow');
  const scaled = build([s], { facts, box: { width: 300, height: 200 }, level: 'wide' });
  assert.equal(scaled.level, 'wide', 'the given level wins over the box width');
  assert.ok(toolsOf(scaled).length >= toolsOf(small).length, 'never less detail');
  assert.deepEqual(build([s], { facts, box: { width: 1200, height: 200 } }).level, 'wide', 'without a level the width decides, as before');
});
