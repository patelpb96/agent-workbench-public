// The agent map's model: pure functions that turn the inspector's source list (plus optional tool facts and roster edges)
// into pixel-snapped nodes and orthogonal edges. No DOM, no Tauri, no clock, no randomness: the same input always gives the same
// output, and the input is never changed.
//
// Layout (user direction 2026-10-23: agents together in the centre, tools and connections outside them, a circuit-board look):
//   * The agents sit in one compact block. Each top-level session is a "family": the session followed by its subagents and roster
//     reports in one short row. Families flow left to right and wrap, so the block uses the available width instead of one long column.
//   * The tools sit outside that block, in two columns (left and right), joined by a vertical rail on each side. A thin line from each
//     rail enters the block. Selecting an agent draws its own routes (along the gutter under its row, then the rail) to the tools it used.
//   * How much is drawn depends on the width: tool families when narrow, families with each MCP server when medium, single tools when wide.
import type { SourceInfo } from './client.js';
import { AGENT_COLOR, classify, displayName } from './groups.js';

export type MapLevel = 'narrow' | 'medium' | 'wide';
export type ToolGroup = 'file' | 'shell' | 'web' | 'agent' | 'other' | `mcp:${string}`;

export const GLYPH = 20; // agent glyph box, px
export const TOOL_GLYPH = 12; // tool glyph box, px
export const GAP = 6; // px between neighbouring agents in a family
export const PAD = 8; // px around everything
const FAMILY_GAP = 12; // px between families in a row
const FRAME_PAD = 6; // px between the agents' frame and the agents
const ROW = GLYPH + 10; // px between rows of families; the gutter in between carries the lines
const RAIL_GAP = 10; // px between a tool column and its rail
const PORT_GAP = 16; // px between a rail and the agents' frame
const LABEL_GAP = 6; // px between a tool glyph and its name
const labelWidth = (level: MapLevel): number => (level === 'narrow' ? 58 : 92);
/** Width of each tool column: its name, the glyph, the rail, up to the frame. */
const bandFor = (level: MapLevel): number => labelWidth(level) + LABEL_GAP + TOOL_GLYPH + RAIL_GAP + PORT_GAP;
const TOOL_PITCH = 18; // least px between tool nodes in a column
/** The block of agents aims at this width-to-height shape (a wide pane gets a block in the middle, not a thin banner across it) ... */
const BLOCK_ASPECT = 1.8;
/** ... but never narrower than eight single sessions in a row, so a short list stays on one line. */
const MIN_BLOCK = 8 * (GLYPH + FAMILY_GAP) - FAMILY_GAP;
/** Wide maps draw at most this many tool nodes in all. */
const MAX_TOOL_NODES = 24;

export interface MapTool { readonly name: string; readonly calls: number; readonly errors: number }
export interface MapSessionFacts { readonly tools: readonly MapTool[]; readonly open_calls: number; /** names of the tools running right now */ readonly open_tools?: readonly string[] }
export type MapFacts = Readonly<Record<string, MapSessionFacts>>;

/** <480 narrow, <1000 medium, else wide. */
export function levelFor(width: number): MapLevel {
  return width < 480 ? 'narrow' : width < 1000 ? 'medium' : 'wide';
}

const FILE_TOOLS = new Set(['read', 'write', 'edit', 'multiedit', 'notebookedit', 'notebookread', 'glob', 'grep', 'ls', 'find', 'view', 'apply_patch', 'str_replace_editor']);
const SHELL_TOOLS = new Set(['bash', 'bashoutput', 'killbash', 'killshell', 'powershell', 'shell', 'local_shell', 'exec', 'exec_command', 'run_command']);
const WEB_TOOLS = new Set(['webfetch', 'websearch', 'web_search', 'fetch']);
const AGENT_TOOLS = new Set(['task', 'agent', 'taskoutput', 'spawn_agent']);

/** Which family a tool belongs to: `mcp__server__tool` is `mcp:server`, the built-in families by name, anything else `other`. */
export function toolGroup(name: string): ToolGroup {
  const mcp = /^mcp__([A-Za-z0-9][A-Za-z0-9_.-]*?)__./.exec(name);
  if (mcp?.[1]) return `mcp:${mcp[1]}`;
  const key = name.toLowerCase();
  if (FILE_TOOLS.has(key)) return 'file';
  if (SHELL_TOOLS.has(key)) return 'shell';
  if (WEB_TOOLS.has(key)) return 'web';
  if (AGENT_TOOLS.has(key)) return 'agent';
  return 'other';
}

export interface MapInput {
  readonly sources: readonly SourceInfo[]; // already ordered; the caller passes sortSources(list, 'newest')
  readonly now: number;
  readonly facts?: MapFacts; // absent = tools not indexed yet (agents only)
  readonly box: { readonly width: number; readonly height: number };
  readonly selected?: string | null;
  /** An agent whose routes to its tools are drawn as well (the one under the pointer or focus). */
  readonly highlight?: string | null;
  readonly reports?: readonly { readonly manager: string; readonly report: string }[]; // roster edges
  /** How much detail to draw; by default from `box.width`. A map laid out small to be drawn scaled up passes the
   *  level of the width it is shown at, so a bigger map does not show less. */
  readonly level?: MapLevel;
}
export interface MapGlyph {
  readonly shape: 'square' | 'diamond' | 'sheet' | 'chevron' | 'globe' | 'stack' | 'plain';
  readonly hue: string; // AGENT_COLOR for agents, theme accents for tools
  readonly fill: 'solid' | 'hollow' | 'dashed' | 'empty';
  readonly inFlight: boolean;
  readonly errors: boolean;
  readonly selected: boolean;
}
export interface MapNode {
  readonly id: string;
  readonly label: string;
  readonly kind: 'session' | 'subagent' | 'tool';
  readonly source: string | null; // the SourceInfo id for session/subagent nodes
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  /** 0 for a top-level session; one more for each level of subagent or roster report beneath it. Tools are 0. */
  readonly depth: number;
  readonly glyph: MapGlyph;
  /** A short fact line for a tooltip: what an agent has used and is running, or who uses a tool. */
  readonly detail: string;
  /** Tools: the name as drawn beside the glyph (cut to fit) and which side of it; empty and null for agents. */
  readonly text: string;
  readonly side: 'left' | 'right' | null;
  /** Tools: how far the name reaches beside the glyph, px (the pointer target covers it); 0 for agents. */
  readonly reach: number;
  /** Tools: width in px of the usage bar under the name, in proportion to the busiest tool. */
  readonly bar: number;
  /** Tools: some agent has a call to it running right now. */
  readonly live: boolean;
  readonly stats: { readonly agents: number; readonly calls: number; readonly errors: number } | null;
  /** Tools: the source ids of the agents that used it. */
  readonly users: readonly string[];
}
/**
 * `spawn` and `reports` join an agent to its parent inside the block; `rail` is the structure around the tools; `call` is the route
 * from the selected (or pointed-at) agent to the tools it used; `live` is the route to a tool it is running right now.
 */
export interface MapEdge {
  readonly from: string;
  readonly to: string;
  readonly kind: 'spawn' | 'reports' | 'call' | 'live' | 'rail';
  readonly points: readonly (readonly [number, number])[];
}
export interface MapFrame { readonly x: number; readonly y: number; readonly w: number; readonly h: number }
export interface MapResult {
  readonly nodes: readonly MapNode[];
  readonly edges: readonly MapEdge[];
  /** The box drawn around the agents; null when there are none. */
  readonly frame: MapFrame | null;
  readonly width: number;
  readonly height: number;
  readonly level: MapLevel;
  readonly summary: string;
}

const MUTED_HUE = '#a5b3c6';
/** Tool colours come from the splash accents, so they never compete with the agent colours. */
const GROUP_HUE = { file: '#A6D9AC', shell: '#E4A376', web: '#78BBC0', agent: '#E3EAD2', mcp: '#607A71', other: MUTED_HUE } as const;
const GROUP_ORDER: readonly string[] = ['file', 'shell', 'web', 'agent'];

function groupShape(group: ToolGroup): MapGlyph['shape'] {
  if (group === 'file') return 'sheet';
  if (group === 'shell') return 'chevron';
  if (group === 'web') return 'globe';
  return group.startsWith('mcp:') ? 'stack' : 'plain';
}
function groupHue(group: ToolGroup): string {
  return group.startsWith('mcp:') ? GROUP_HUE.mcp : GROUP_HUE[group as 'file' | 'shell' | 'web' | 'agent' | 'other'];
}
const groupRank = (group: ToolGroup): number => {
  const i = GROUP_ORDER.indexOf(group);
  if (i >= 0) return i;
  return group.startsWith('mcp:') ? GROUP_ORDER.length : GROUP_ORDER.length + 1; // mcp servers after the built-ins, then other
};
const byText = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

function agentFill(group: ReturnType<typeof classify>): MapGlyph['fill'] {
  return group === 'active' ? 'solid' : group === 'idle' ? 'hollow' : group === 'inactive' ? 'dashed' : 'empty';
}

/** What a tool becomes on the map at a given level: a node key, its family and the label shown. */
function toolKey(level: MapLevel, name: string): { key: string; group: ToolGroup; label: string } {
  const group = toolGroup(name);
  const mcp = group.startsWith('mcp:');
  if (level === 'narrow') return mcp ? { key: 'mcp', group: 'mcp:', label: 'mcp' } : { key: group, group, label: group };
  if (level === 'medium') return { key: group, group, label: group };
  return mcp ? { key: group, group, label: group.slice(4) } : { key: name, group, label: name };
}

interface ToolNode { key: string; group: ToolGroup; label: string; calls: number; errors: number; users: Set<string> }

/** Everything the map needs, laid out. Deterministic and free of side effects. */
export function buildMap(input: MapInput): MapResult {
  const level = input.level ?? levelFor(input.box.width);
  const byId = new Map<string, SourceInfo>();
  for (const s of input.sources) if (!byId.has(s.id)) byId.set(s.id, s);

  // ---- who hangs under whom: roster managers for sessions, the parent session for subagents, else top level ------------------
  const parentOf = new Map<string, { parent: string; edge: 'spawn' | 'reports' }>();
  const reaches = (from: string, target: string): boolean => {
    // Does walking up from `from` pass through `target`? (the cycle test; the visited set also ends any pre-existing loop)
    const seen = new Set<string>();
    for (let cur: string | undefined = from; cur !== undefined && !seen.has(cur); cur = parentOf.get(cur)?.parent) {
      if (cur === target) return true;
      seen.add(cur);
    }
    return false;
  };
  const attach = (child: string, parent: string, edge: 'spawn' | 'reports'): void => {
    if (child === parent || parentOf.has(child) || !byId.has(parent) || reaches(parent, child)) return;
    parentOf.set(child, { parent, edge });
  };
  for (const r of input.reports ?? []) {
    if (byId.get(r.report)?.kind === 'session' && byId.get(r.manager)?.kind === 'session') attach(r.report, r.manager, 'reports');
  }
  for (const s of byId.values()) {
    if (s.kind === 'subagent' && s.parent !== null) attach(s.id, s.parent, 'spawn');
  }
  const kids = new Map<string, string[]>();
  const top: string[] = [];
  for (const s of byId.values()) {
    const link = parentOf.get(s.id);
    if (!link) top.push(s.id);
    else kids.set(link.parent, [...(kids.get(link.parent) ?? []), s.id]);
  }

  // ---- families: a top-level session and everything under it, depth first, in one row ----------------------------------------
  interface Cell { id: string; depth: number; parent: string | null; edge: 'spawn' | 'reports' }
  const families: Cell[][] = top.map(id => {
    const cells: Cell[] = [];
    const walk = (cur: string, depth: number, parent: string | null, edge: 'spawn' | 'reports'): void => {
      cells.push({ id: cur, depth, parent, edge });
      for (const k of kids.get(cur) ?? []) walk(k, depth + 1, cur, parentOf.get(k)?.edge ?? 'spawn');
    };
    walk(id, 0, null, 'spawn');
    return cells;
  });

  // ---- tools: one node per family / MCP server / tool, counted over every listed agent ---------------------------------------
  const toolMap = new Map<string, ToolNode>();
  const names = new Set<string>();
  const running = new Map<string, Set<string>>(); // source id -> keys of the tool nodes it has a call running to
  for (const s of byId.values()) {
    const open = input.facts?.[s.id]?.open_tools ?? [];
    if (open.length > 0) running.set(s.id, new Set(open.map(name => toolKey(level, name).key)));
    for (const t of input.facts?.[s.id]?.tools ?? []) {
      names.add(t.name);
      const k = toolKey(level, t.name);
      const node = toolMap.get(k.key) ?? { ...k, calls: 0, errors: 0, users: new Set<string>() };
      node.calls += t.calls;
      node.errors += t.errors;
      node.users.add(s.id);
      toolMap.set(k.key, node);
    }
  }
  let tools = [...toolMap.values()];
  if (level === 'wide' && tools.length > MAX_TOOL_NODES) tools = tools.sort((a, b) => b.calls - a.calls || byText(a.key, b.key)).slice(0, MAX_TOOL_NODES);
  tools.sort((a, b) => groupRank(a.group) - groupRank(b.group) || b.calls - a.calls || byText(a.key, b.key));
  const hasTools = tools.length > 0;
  const leftTools = tools.slice(0, Math.ceil(tools.length / 2));
  const rightTools = tools.slice(leftTools.length);

  // ---- the block of agents: families flow left to right and wrap -------------------------------------------------------------
  const band = hasTools ? bandFor(level) : 0;
  const LW = labelWidth(level);
  const room = Math.max(GLYPH, input.box.width - 2 * PAD - 2 * band - 2 * FRAME_PAD);
  const cells = families.reduce((sum, f) => sum + f.length, 0);
  const available = Math.min(room, Math.max(MIN_BLOCK, Math.round(Math.sqrt(cells * (GLYPH + GAP) * ROW * BLOCK_ASPECT))));
  const familyWidth = (f: readonly Cell[]): number => f.length * GLYPH + (f.length - 1) * GAP;
  const placed: { cell: Cell; col: number; row: number; x: number }[] = []; // x relative to the block's inner left edge
  let row = 0;
  let cursor = 0;
  let innerW = 0;
  for (const f of families) {
    const w = familyWidth(f);
    if (cursor > 0 && cursor + FAMILY_GAP + w > available) {
      row++;
      cursor = 0;
    }
    if (cursor > 0) cursor += FAMILY_GAP;
    f.forEach((cell, i) => placed.push({ cell, col: i, row, x: cursor + i * (GLYPH + GAP) }));
    cursor += w;
    innerW = Math.max(innerW, cursor);
  }
  const rows = families.length === 0 ? 0 : row + 1;
  const innerH = rows === 0 ? 0 : rows * GLYPH + (rows - 1) * (ROW - GLYPH);
  const groupW = rows === 0 ? 0 : innerW + 2 * FRAME_PAD;
  const groupH = rows === 0 ? 0 : innerH + 2 * FRAME_PAD;

  // ---- the whole picture: tool columns left and right, block in the middle, everything centred in the box -----------------
  const contentH = Math.max(groupH, leftTools.length * TOOL_PITCH, rightTools.length * TOOL_PITCH);
  const contentW = groupW + 2 * band;
  const width = Math.max(Math.round(input.box.width), contentW + 2 * PAD);
  const height = contentH + 2 * PAD;
  const x0 = Math.floor((width - contentW) / 2);
  const frame: MapFrame | null = rows === 0 ? null : { x: x0 + band, y: PAD + Math.floor((contentH - groupH) / 2), w: groupW, h: groupH };
  const leftNodeX = x0 + LW + LABEL_GAP;
  const leftRail = leftNodeX + TOOL_GLYPH + RAIL_GAP;
  const rightRail = x0 + band + groupW + PORT_GAP;
  const rightNodeX = rightRail + RAIL_GAP;

  const nodes: MapNode[] = [];
  const edges: MapEdge[] = [];
  const pos = new Map<string, MapNode>();
  const innerX = (frame?.x ?? 0) + FRAME_PAD;
  const innerY = (frame?.y ?? 0) + FRAME_PAD;
  const gutterY = (r: number): number => innerY + r * ROW + GLYPH + (ROW - GLYPH) / 2;
  const selectedId = input.selected ?? null;
  for (const p of placed) {
    const s = byId.get(p.cell.id) as SourceInfo;
    const facts = input.facts?.[s.id];
    const subagent = s.kind === 'subagent';
    const node: MapNode = {
      id: `s:${s.id}`,
      label: displayName(s, s.parent ? displayParent(byId.get(s.parent)) : null),
      kind: subagent ? 'subagent' : 'session',
      source: s.id,
      x: innerX + p.x,
      y: innerY + p.row * ROW,
      w: GLYPH,
      h: GLYPH,
      depth: p.cell.depth,
      detail: agentDetail(facts),
      text: '',
      side: null,
      reach: 0,
      bar: 0,
      live: false,
      stats: null,
      users: [],
      glyph: {
        shape: subagent ? 'diamond' : 'square',
        hue: AGENT_COLOR[s.agent],
        fill: agentFill(classify(s, input.now)),
        inFlight: (facts?.open_calls ?? 0) > 0,
        errors: (facts?.tools ?? []).some(t => t.errors > 0),
        selected: selectedId === s.id
      }
    };
    nodes.push(node);
    pos.set(s.id, node);
  }
  // parent to child inside a family: down into the gutter under the row, across, and up to the child
  for (const p of placed) {
    if (p.cell.parent === null) continue;
    const a = pos.get(p.cell.parent) as MapNode;
    const b = pos.get(p.cell.id) as MapNode;
    const y = gutterY(p.row);
    edges.push({ from: a.id, to: b.id, kind: p.cell.edge, points: [[a.x + GLYPH / 2, a.y + GLYPH], [a.x + GLYPH / 2, y], [b.x + GLYPH / 2, y], [b.x + GLYPH / 2, b.y + GLYPH]] });
  }

  const maxCalls = Math.max(1, ...tools.map(t => t.calls));
  const keyOrder = new Map(input.sources.map((q, i) => [q.id, i]));
  const column = (list: readonly ToolNode[], nodeX: number, railX: number, side: 'left' | 'right'): Map<string, { cy: number; node: MapNode }> => {
    const made = new Map<string, { cy: number; node: MapNode }>();
    if (list.length === 0) return made;
    const pitch = contentH / list.length;
    const toFrame = side === 'left' ? nodeX + TOOL_GLYPH : nodeX;
    const chars = Math.floor(LW / 6.2);
    list.forEach((t, i) => {
      const cy = PAD + Math.round((i + 0.5) * pitch);
      const live = [...running.values()].some(keys => keys.has(t.key));
      const users = [...t.users].sort((a, b) => (keyOrder.get(a) ?? 0) - (keyOrder.get(b) ?? 0));
      const node: MapNode = {
        id: `t:${t.key}`, label: t.label, kind: 'tool', source: null, x: nodeX, y: cy - TOOL_GLYPH / 2, w: TOOL_GLYPH, h: TOOL_GLYPH, depth: 0,
        detail: `${users.length} agent${users.length === 1 ? '' : 's'}, ${t.calls} call${t.calls === 1 ? '' : 's'}${t.errors > 0 ? `, ${t.errors} error${t.errors === 1 ? '' : 's'}` : ''}${live ? ', running now' : ''}`,
        text: t.label.length > chars ? `${t.label.slice(0, chars - 1)}\u2026` : t.label,
        side,
        reach: LW + LABEL_GAP,
        bar: Math.max(2, Math.round((LW * t.calls) / maxCalls)),
        live,
        stats: { agents: users.length, calls: t.calls, errors: t.errors },
        users,
        glyph: { shape: groupShape(t.group), hue: groupHue(t.group), fill: live ? 'solid' : t.errors > 0 ? 'dashed' : 'hollow', inFlight: live, errors: t.errors > 0, selected: false }
      };
      nodes.push(node);
      made.set(t.key, { cy, node });
      edges.push({ from: node.id, to: `rail:${side}`, kind: 'rail', points: [[toFrame, cy], [railX, cy]] });
    });
    // the rail itself, with one port into the block at its middle
    const midY = frame ? frame.y + Math.floor(frame.h / 2) : PAD + Math.floor(contentH / 2);
    const ys = [...made.values()].map(m => m.cy);
    const top0 = Math.min(midY, ...ys);
    const bottom0 = Math.max(midY, ...ys);
    edges.push({ from: `rail:${side}`, to: `rail:${side}`, kind: 'rail', points: [[railX, top0], [railX, bottom0]] });
    if (frame) edges.push({ from: `rail:${side}`, to: 'frame', kind: 'rail', points: [[railX, midY], [side === 'left' ? frame.x : frame.x + frame.w, midY]] });
    return made;
  };
  const left = column(leftTools, leftNodeX, leftRail, 'left');
  const right = column(rightTools, rightNodeX, rightRail, 'right');

  // routes from an agent to a tool: under its row to the rail, along the rail, into the tool
  const routeTo = (me: MapNode, key: string): { to: string; points: [number, number][] } | null => {
    const side = left.has(key) ? 'left' : right.has(key) ? 'right' : null;
    if (side === null) return null;
    const target = (side === 'left' ? left : right).get(key) as { cy: number; node: MapNode };
    const railX = side === 'left' ? leftRail : rightRail;
    const edgeX = side === 'left' ? target.node.x + TOOL_GLYPH : target.node.x;
    const cx = me.x + GLYPH / 2;
    const y = gutterY(Math.round((me.y - innerY) / ROW));
    return { to: target.node.id, points: dedupe([[cx, me.y + GLYPH], [cx, y], [railX, y], [railX, target.cy], [edgeX, target.cy]]) };
  };
  const drawn = new Set<string>();
  for (const [id, kind] of [[selectedId, 'call'], [input.highlight ?? null, 'call']] as const) {
    const me = id ? pos.get(id) : undefined;
    if (!me || !id || drawn.has(id)) continue;
    drawn.add(id);
    const used = new Set((input.facts?.[id]?.tools ?? []).map(t => toolKey(level, t.name).key));
    for (const t of tools) {
      const r = used.has(t.key) ? routeTo(me, t.key) : null;
      if (r) edges.push({ from: me.id, to: r.to, kind, points: r.points });
    }
  }
  // work in progress is always shown, whoever is selected
  for (const [id, keys] of running) {
    const me = pos.get(id);
    if (!me) continue;
    for (const key of keys) {
      const r = routeTo(me, key);
      if (r) edges.push({ from: me.id, to: r.to, kind: 'live', points: r.points });
    }
  }

  const all = [...byId.values()];
  const summary = `${all.length} agents (${all.filter(s => classify(s, input.now) === 'active').length} active), ${all.filter(s => s.kind === 'subagent').length} subagents, ${names.size} tools`;
  return { nodes, edges, frame, width, height, level, summary };
}

/** What an agent has used, busiest first, and what it is running now. Names and counts only. */
function agentDetail(facts: MapSessionFacts | undefined): string {
  if (!facts) return '';
  const top = [...facts.tools].sort((a, b) => b.calls - a.calls || byText(a.name, b.name)).slice(0, 3).map(t => `${t.name} ${t.calls}`).join(', ');
  const now = (facts.open_tools ?? []).join(', ');
  return [now ? `running ${now}` : '', top ? `used ${top}` : ''].filter(Boolean).join(' - ');
}

function displayParent(s: SourceInfo | undefined): string | null {
  return s ? displayName(s) : null;
}

/** Drop repeated points, so a route that already sits on a line has no zero-length segments. */
function dedupe(points: [number, number][]): [number, number][] {
  return points.filter((p, i) => i === 0 || p[0] !== (points[i - 1] as [number, number])[0] || p[1] !== (points[i - 1] as [number, number])[1]);
}
