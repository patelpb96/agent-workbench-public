// Which sidebar sections the user has opened or closed, remembered in the browser storage of this window. This is
// a convenience only: storage can be missing or blocked (the page then behaves as if nothing was remembered), and a
// stored value that is not a boolean is ignored.
const KEY = 'aw.sections';

function read(): Record<string, boolean> {
  try {
    const raw = typeof localStorage === 'undefined' ? null : localStorage.getItem(KEY);
    const parsed: unknown = raw === null ? {} : JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return {};
    const out: Record<string, boolean> = {};
    for (const [name, value] of Object.entries(parsed)) if (typeof value === 'boolean' && /^[a-z0-9-]{1,40}$/.test(name)) out[name] = value;
    return out;
  } catch {
    return {};
  }
}

/** Whether a section is open: what the user chose, or `first` if they have not chosen. */
export function sectionOpen(id: string, first: boolean): boolean {
  return read()[id] ?? first;
}

export function setSectionOpen(id: string, open: boolean): void {
  if (!/^[a-z0-9-]{1,40}$/.test(id)) return;
  try {
    if (typeof localStorage !== 'undefined') localStorage.setItem(KEY, JSON.stringify({ ...read(), [id]: open }));
  } catch {
    // Not remembered; the section still opens and closes.
  }
}
