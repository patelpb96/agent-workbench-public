import { isFolderPath, parentPath } from './files.js';

export type FocusSnapshot = { readonly terminalId: string | null; readonly recordedFolder: string | null | undefined };
export type FollowState = { readonly root: string | null; readonly following: boolean; readonly closed: boolean };
export type FollowReason =
  | 'initialized' | 'invalid-initial' | 'focused-folder' | 'same-focus' | 'ineligible-focus'
  | 'following-off' | 'follow-enabled' | 'follow-enabled-navigation' | 'follow-disabled'
  | 'unchanged' | 'paused' | 'refreshed' | 'manual-up' | 'manual-up-no-parent'
  | 'manual-choice' | 'manual-choice-same' | 'manual-choice-invalid' | 'closed';
export type Update = { readonly root: string | null; readonly following: boolean; readonly changed: boolean; readonly reason: FollowReason };
export type IntentKind = 'startup' | 'picker';

declare const contentTokenBrand: unique symbol;
export type ContentToken = { readonly [contentTokenBrand]: true };
declare const intentTokenBrand: unique symbol;
export type IntentToken = { readonly [intentTokenBrand]: true };

export interface FollowPolicy {
  state(): FollowState;
  setFollowing(enabled: boolean, focus: FocusSnapshot | null | undefined): Update;
  pause(): Update;
  focusChanged(focus: FocusSnapshot | null | undefined): Update;
  setInitialRoot(path: string | null | undefined): Update;
  manualUp(): Update;
  manualChoose(path: string | null | undefined): Update;
  refresh(): Update;
  captureContentToken(): ContentToken;
  isContentCurrent(token: ContentToken): boolean;
  captureIntentToken(kind: IntentKind): IntentToken;
  isIntentCurrent(kind: IntentKind, token: IntentToken): boolean;
  dispose(): void;
}

const mintContent = (): ContentToken => Object.freeze({}) as unknown as ContentToken;
const mintIntent = (): IntentToken => Object.freeze({}) as unknown as IntentToken;

function normalizeFolder(path: string): string {
  const lower = path.toLowerCase();
  const trimmed = lower.replace(/\\+$/, '');
  return /^[a-z]:$/.test(trimmed) ? `${trimmed}\\` : trimmed;
}

export function sameFolder(a: string | null | undefined, b: string | null | undefined): boolean {
  return isFolderPath(a) && isFolderPath(b) && normalizeFolder(a) === normalizeFolder(b);
}

function folderOf(focus: FocusSnapshot | null | undefined): string | null {
  if (focus === null || focus === undefined || typeof focus !== 'object') return null;
  if (typeof focus.terminalId !== 'string') return null;
  const folder = focus.recordedFolder;
  return isFolderPath(folder) ? folder : null;
}

export function createFollowPolicy(initial: { readonly root: string | null | undefined; readonly following: unknown }): FollowPolicy {
  let root: string | null = isFolderPath(initial.root) ? initial.root : null;
  let following = initial.following === true;
  let closed = false;
  let content = mintContent();
  let startupIntent = mintIntent();
  let pickerIntent = mintIntent();

  const advanceContent = (): void => { content = mintContent(); };
  // Startup only waits to supply a first root, so it stays current until there is one: a toggle, pause, refresh or
  // cancelled picker while the panel is still empty must not leave it with no folder. Call this after setting root.
  const advanceIntent = (): void => { pickerIntent = mintIntent(); if (root !== null) startupIntent = mintIntent(); };
  const update = (changed: boolean, reason: FollowReason): Update => ({ root, following, changed, reason });
  const closedUpdate = (): Update => ({ root: null, following: false, changed: false, reason: 'closed' });

  return {
    state: () => ({ root, following, closed }),
    setFollowing: (enabled, focus) => {
      if (closed) return closedUpdate();
      if (enabled === following) return update(false, 'unchanged');
      following = enabled;
      const folder = enabled ? folderOf(focus) : null;
      const navigates = folder !== null && !sameFolder(root, folder);
      if (navigates) { root = folder; advanceContent(); }
      advanceIntent();
      if (!enabled) return update(false, 'follow-disabled');
      return navigates ? update(true, 'follow-enabled-navigation') : update(false, 'follow-enabled');
    },
    pause: () => {
      if (closed) return closedUpdate();
      following = false;
      advanceIntent();
      return update(false, 'paused');
    },
    focusChanged: (focus) => {
      if (closed) return closedUpdate();
      if (!following) return update(false, 'following-off');
      const folder = folderOf(focus);
      if (folder === null) return update(false, 'ineligible-focus');
      if (sameFolder(root, folder)) return update(false, 'same-focus');
      root = folder;
      advanceContent();
      advanceIntent();
      return update(true, 'focused-folder');
    },
    setInitialRoot: (path) => {
      if (closed) return closedUpdate();
      if (!isFolderPath(path)) return update(false, 'invalid-initial');
      root = path;
      advanceContent();
      // The startup default arriving must not cancel a folder picker the person has open.
      startupIntent = mintIntent();
      return update(true, 'initialized');
    },
    manualUp: () => {
      if (closed) return closedUpdate();
      following = false;
      advanceIntent();
      const parent = root === null ? null : parentPath(root);
      if (parent === null) return update(false, 'manual-up-no-parent');
      root = parent;
      advanceContent();
      return update(true, 'manual-up');
    },
    manualChoose: (path) => {
      if (closed) return closedUpdate();
      following = false;
      if (!isFolderPath(path)) { advanceIntent(); return update(false, 'manual-choice-invalid'); }
      if (sameFolder(root, path)) { advanceIntent(); return update(false, 'manual-choice-same'); }
      root = path;
      advanceContent();
      advanceIntent();
      return update(true, 'manual-choice');
    },
    refresh: () => {
      if (closed) return closedUpdate();
      advanceContent();
      advanceIntent();
      return update(true, 'refreshed');
    },
    captureContentToken: () => content,
    isContentCurrent: (token) => !closed && token === content,
    captureIntentToken: (kind) => kind === 'startup' ? startupIntent : pickerIntent,
    isIntentCurrent: (kind, token) => !closed && token === (kind === 'startup' ? startupIntent : pickerIntent),
    dispose: () => {
      if (closed) return;
      closed = true;
      root = null;
      following = false;
      advanceContent();
      advanceIntent();
    }
  };
}
