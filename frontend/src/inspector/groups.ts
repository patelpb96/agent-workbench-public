// Pure logic behind the inspector's compact view: which state group a session belongs to, how groups are
// sorted into sections, and the colours used to tell sessions apart. No DOM, no Tauri; unit tested.
// A future agent dashboard reads the same `SourceInfo` list through these functions so the two views never
// disagree about what "active" means.
import type { AgentName, SourceInfo } from './client.js';

export type StateGroup = 'active' | 'idle' | 'inactive' | 'other';
export type TabId = 'all' | StateGroup;

/** A transcript written to this recently counts as active (also what the native side calls `active`). */
export const ACTIVE_WITHIN_MS = 2 * 60_000;
/** Quiet for longer than that but not longer than this: idle. Older: inactive. */
export const IDLE_WITHIN_MS = 2 * 60 * 60_000;
/** A transcript smaller than this has nothing worth showing (a stub): it goes to "other". */
export const STUB_BYTES = 1024;

export const TABS: readonly { readonly id: TabId; readonly label: string; readonly hint: string }[] = [
  { id: 'all', label: 'All', hint: 'Every session found' },
  { id: 'active', label: 'Active', hint: 'Running now, or written to within the last 2 minutes' },
  { id: 'idle', label: 'Idle', hint: 'Quiet for 2 minutes up to 2 hours' },
  { id: 'inactive', label: 'Inactive', hint: 'Quiet for more than 2 hours' },
  { id: 'other', label: 'Other', hint: 'Nearly empty transcripts, with nothing to show' }
];

/** Which state group a session is in. A Claude session that Claude itself reports as running is active whatever its file age. */
export function classify(source: SourceInfo, now: number): StateGroup {
  if (source.running !== null) return 'active';
  if (source.bytes < STUB_BYTES) return 'other';
  const age = Math.max(0, now - source.modified_ms);
  if (age <= ACTIVE_WITHIN_MS) return 'active';
  return age <= IDLE_WITHIN_MS ? 'idle' : 'inactive';
}

export function counts(list: readonly SourceInfo[], now: number): Record<TabId, number> {
  const out: Record<TabId, number> = { all: list.length, active: 0, idle: 0, inactive: 0, other: 0 };
  for (const s of list) out[classify(s, now)]++;
  return out;
}
export function inTab(list: readonly SourceInfo[], tab: TabId, now: number): SourceInfo[] {
  return tab === 'all' ? [...list] : list.filter(s => classify(s, now) === tab);
}

export type SortKey = 'newest' | 'agent' | 'model' | 'folder';
export const SORTS: readonly { readonly id: SortKey; readonly label: string }[] = [
  { id: 'newest', label: 'Newest first' },
  { id: 'agent', label: 'Agent' },
  { id: 'model', label: 'Model' },
  { id: 'folder', label: 'Folder' }
];

export const agentName = (agent: AgentName): string => ({ claude: 'Claude Code', pi: 'Pi', codex: 'Codex' })[agent];
const AGENT_ORDER: Record<AgentName, number> = { claude: 0, pi: 1, codex: 2 };

/** The project folder name Claude Code and Pi use as a label, made readable: drive and user-folder prefixes dropped. */
export function tidyLabel(label: string): string {
  const tidy = label.replace(/^-+|-+$/g, '').replace(/^[A-Za-z]--/, '').replace(/^Users-[^-]+-?/, '');
  return tidy === '' ? 'home folder' : tidy;
}
/** The name to show: the parent's name for a subagent, else the folder, else the tidied label. */
export function displayName(source: SourceInfo, parentName?: string | null): string {
  if (source.kind === 'subagent') return `${parentName ?? source.folder ?? tidyLabel(source.label)} \u00b7 sub ${source.id.slice(0, 4)}`;
  return source.folder ?? tidyLabel(source.label);
}

const byNewest = (a: SourceInfo, b: SourceInfo): number => b.modified_ms - a.modified_ms || a.id.localeCompare(b.id);
function groupKey(s: SourceInfo, key: SortKey): string {
  switch (key) {
    case 'agent': return String(AGENT_ORDER[s.agent]);
    case 'model': return s.model === null ? '￿' : s.model.toLowerCase();
    case 'folder': return displayName(s).toLowerCase();
    default: return '';
  }
}
export function sortSources(list: readonly SourceInfo[], key: SortKey): SourceInfo[] {
  const out = [...list];
  out.sort((a, b) => (key === 'newest' ? 0 : groupKey(a, key).localeCompare(groupKey(b, key))) || byNewest(a, b));
  return out;
}

export interface Section { readonly key: string; readonly label: string | null; readonly items: readonly SourceInfo[] }
/** Sorted sessions in labelled sections (one per agent, model or folder); newest-first is a single unlabelled section. */
export function sections(list: readonly SourceInfo[], key: SortKey): Section[] {
  const sorted = sortSources(list, key);
  if (key === 'newest') return sorted.length === 0 ? [] : [{ key: '', label: null, items: sorted }];
  const out: { key: string; label: string; items: SourceInfo[] }[] = [];
  for (const s of sorted) {
    const k = groupKey(s, key);
    let section = out.find(o => o.key === k);
    if (!section) {
      section = { key: k, label: key === 'agent' ? agentName(s.agent) : key === 'model' ? (s.model ?? 'Model not named') : displayName(s), items: [] };
      out.push(section);
    }
    section.items.push(s);
  }
  return out;
}

// ---- colours -----------------------------------------------------------------------------------------------

/** Provider identity (kept apart from status, which is shown by the group a session is in). */
export const AGENT_COLOR: Record<AgentName, string> = { claude: '#e0925f', pi: '#7db4ff', codex: '#7fd6a4' };
export const TAG_NAMES = ['red', 'orange', 'yellow', 'green', 'blue', 'purple'] as const;
export type TagName = (typeof TAG_NAMES)[number];
/** The user's colour tags (set in the inspector, remembered natively). */
export const TAG_COLOR: Record<TagName, string> = { red: '#e5736b', orange: '#e59a4f', yellow: '#e5cd5f', green: '#6fc48b', blue: '#6ea8ef', purple: '#a98be0' };
export const isTagName = (v: unknown): v is TagName => typeof v === 'string' && (TAG_NAMES as readonly string[]).includes(v);

/** A steady colour per model name, so the same model always looks the same; neutral when the model is not known. */
export function modelColor(model: string | null): string {
  if (model === null) return '#8696ab';
  let h = 2166136261;
  for (let i = 0; i < model.length; i++) h = Math.imul(h ^ model.charCodeAt(i), 16777619);
  return `hsl(${Math.abs(h) % 360} 55% 68%)`;
}
/** A model name made short enough for a small tile: vendor prefix and a trailing date dropped. */
export function shortModel(model: string): string {
  const short = model.replace(/^claude-/, '').replace(/-\d{8}$/, '').replace(/^models\//, '');
  return short.length > 22 ? `${short.slice(0, 21)}…` : short;
}

/** A compact age for a tile: "now", "40s", "12m", "5h", "3d". */
export function agoShort(then: number, now: number): string {
  const d = Math.max(0, now - then);
  if (d < 5_000) return 'now';
  if (d < 60_000) return `${Math.floor(d / 1000)}s`;
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m`;
  if (d < 48 * 3_600_000) return `${Math.floor(d / 3_600_000)}h`;
  return `${Math.floor(d / 86_400_000)}d`;
}
