// Shared, reactive copy of the colour tags the user put on sessions. The file lives natively
// (inspector-tags.json); every change is saved at once and the native answer replaces the local copy.
import { inspectorClient } from './tauri.js';
import { isTagName, type TagName } from './groups.js';

let tags = $state.raw<Record<string, string>>({});

export const inspectorTags = {
  /** The tag of a session, or null. */
  colorOf(id: string): TagName | null {
    const color = tags[id];
    return isTagName(color) ? color : null;
  },

  async load(): Promise<void> {
    try {
      tags = { ...(await inspectorClient.tags()).tags };
    } catch {
      /* not available (browser preview): no tags */
    }
  },

  /** Tag a session (or clear its tag with null). Resolves true when it was saved. */
  async set(id: string, color: TagName | null): Promise<boolean> {
    const before = tags;
    const next = { ...tags };
    if (color === null) delete next[id];
    else next[id] = color;
    tags = next;
    try {
      tags = { ...(await inspectorClient.saveTags({ version: 1, tags: next })).tags };
      return true;
    } catch {
      tags = before;
      return false;
    }
  }
};
