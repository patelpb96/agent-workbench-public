import type * as Wire from '../execution-v2/generated.js';
import type {
  DisplayGeometry, LayoutIntentState, LayoutManager, ViewportBounds
} from './workspace-ports.js';
import {
  ExecutionClientError, isCanonicalId, isExecutionError, isLayout,
  isSaveLayoutResponse, sanitizeExecutionError
} from './client.js';

function failure(code: string, user_message: string): ExecutionClientError {
  return new ExecutionClientError({ code, user_message, recoverable: true });
}

function invalid(): ExecutionClientError {
  return failure('layout_invalid', 'The layout or layout operation is invalid.');
}

function conflict(): Wire.Error {
  return {
    code: 'layout_revision_conflict',
    user_message: 'The durable layout changed. Refresh and explicitly reapply the preserved layout intent.',
    recoverable: true
  };
}

function boundary<T>(operation: () => T): T {
  try {
    return operation();
  } catch (error: unknown) {
    throw new ExecutionClientError(sanitizeExecutionError(error));
  }
}

function copy<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function freeze<T>(value: T): T {
  if (value !== null && typeof value === 'object') {
    for (const child of Object.values(value)) freeze(child);
    Object.freeze(value);
  }
  return value;
}

function immutable<T>(value: T): T {
  return freeze(copy(value));
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') {
    const fields = value as Record<string, unknown>;
    return '{' + Object.keys(fields).sort().map(key =>
      JSON.stringify(key) + ':' + canonical(fields[key])).join(',') + '}';
  }
  return JSON.stringify(value) ?? 'undefined';
}

function same(left: unknown, right: unknown): boolean {
  return canonical(left) === canonical(right);
}

function validLayout(value: unknown): value is Wire.Layout {
  try {
    // isLayout owns the common shape, identity, geometry, capacity and reference checks.
    // Require actual enum strings, rather than accepting string-coercible objects.
    return isLayout(value) && typeof value.theme === 'string' &&
      typeof value.active_module === 'string' &&
      value.panels.every(panel => typeof panel.stage === 'string');
  } catch {
    return false;
  }
}

function checkedLayout(value: unknown): Wire.Layout {
  if (!validLayout(value)) throw invalid();
  return value;
}

function checkedId(value: unknown): asserts value is Wire.Id {
  if (!isCanonicalId(value)) throw invalid();
}

function nextEpoch(epoch: number): number {
  if (epoch >= Number.MAX_SAFE_INTEGER) {
    throw failure('capacity_exceeded', 'The layout edit epoch limit has been reached.');
  }
  return epoch + 1;
}

function savedLayout(request: Wire.SaveLayoutRequest): Wire.Layout {
  return { ...request.layout, revision: request.expected_revision + 1 };
}

export function canSaveLayout(state: LayoutIntentState): boolean {
  try {
    if (state.warning !== null || !state.dirty ||
        !validLayout(state.base) || !validLayout(state.draft) ||
        state.base.workspace_id !== state.draft.workspace_id) return false;
    if (state.attempt !== null) {
      const request = state.attempt.request;
      return request.contract_version === 2 && isCanonicalId(request.request_id) &&
        request.workspace_id === state.base.workspace_id && validLayout(request.layout) &&
        request.layout.workspace_id === request.workspace_id &&
        request.expected_revision === request.layout.revision &&
        request.expected_revision < Number.MAX_SAFE_INTEGER;
    }
    return !state.conflicted && state.draft.revision === state.base.revision &&
      state.base.revision < Number.MAX_SAFE_INTEGER;
  } catch {
    return false;
  }
}

export function createLayoutManager(nextId: () => Wire.Id): LayoutManager {
  // This is the only private intent state; there is no receipt/history cache.
  let state: LayoutIntentState = freeze({
    base: null, draft: null, warning: null, edit_epoch: 0,
    attempt: null, dirty: false, conflicted: false, problem: null
  });

  return {
    getState(): LayoutIntentState {
      return immutable(state);
    },

    observe(layout: Wire.Layout | null, warning: Wire.Error | null): void {
      boundary(() => {
        if (layout === null) {
          if (!isExecutionError(warning)) throw invalid();
          state = freeze({ ...state, warning: immutable(warning) });
          return;
        }
        if (warning !== null) throw invalid();
        const incoming = immutable(checkedLayout(layout));
        const workspaceId = state.base?.workspace_id ?? state.draft?.workspace_id ??
          state.attempt?.request.workspace_id;
        if (workspaceId !== undefined && incoming.workspace_id !== workspaceId) throw invalid();
        const base = state.base;
        if (base !== null && incoming.revision < base.revision) return;
        if (base !== null && incoming.revision === base.revision && !same(incoming, base)) {
          state = freeze({ ...state, conflicted: true, problem: conflict() });
          return;
        }
        if (!state.dirty && state.attempt === null) {
          state = freeze({
            ...state, base: incoming, draft: immutable(incoming), warning: null,
            conflicted: false, problem: null
          });
          return;
        }
        const advanced = base === null || incoming.revision > base.revision;
        const attempt = state.attempt;
        const observedOwnCommit = attempt !== null &&
          same(incoming, savedLayout(attempt.request));
        const conflicted = state.conflicted || (advanced && !observedOwnCommit);
        // An observation is not a save acknowledgement and never consumes an edit epoch.
        state = freeze({
          ...state, base: incoming, warning: null, conflicted,
          problem: conflicted ? conflict() : state.problem
        });
      });
    },

    edit(draft: Wire.Layout): void {
      boundary(() => {
        if (state.warning !== null || state.base === null || state.draft === null) {
          throw failure('layout_invalid', 'A valid authoritative layout is required before editing.');
        }
        const incoming = immutable(checkedLayout(draft));
        if (incoming.workspace_id !== state.base.workspace_id ||
            incoming.revision !== state.draft.revision) throw invalid();
        if (same(incoming, state.draft)) return;
        state = freeze({
          ...state, draft: incoming, edit_epoch: nextEpoch(state.edit_epoch), dirty: true,
          problem: state.conflicted ? state.problem : null
        });
      });
    },

    beginSave(): Wire.SaveLayoutRequest {
      return boundary(() => {
        if (state.warning !== null) {
          throw failure('layout_invalid', 'Saving is disabled while the stored layout is preserved under a warning.');
        }
        // Even after later edits or remote observations, an unresolved outcome uses this DTO.
        if (state.attempt !== null) return immutable(state.attempt.request);
        if (!canSaveLayout(state) || state.base === null || state.draft === null) {
          if (state.conflicted) throw new ExecutionClientError(conflict());
          throw failure('layout_invalid', 'There is no saveable layout intent.');
        }
        const captured = state;
        const requestId = nextId();
        if (state !== captured) {
          throw failure('state_conflict', 'The layout changed while preparing the save request.');
        }
        if (!isCanonicalId(requestId)) {
          throw failure('validation_failed', 'A canonical request identity is required.');
        }
        const request: Wire.SaveLayoutRequest = immutable({
          contract_version: 2, workspace_id: state.base.workspace_id,
          request_id: requestId, expected_revision: state.base.revision,
          layout: checkedLayout(state.draft)
        });
        state = freeze({
          ...state, attempt: { request, edit_epoch: state.edit_epoch }, problem: null
        });
        return immutable(request);
      });
    },

    acknowledge(result: Wire.SaveLayoutResponse): void {
      boundary(() => {
        if (!isSaveLayoutResponse(result) || !validLayout(result.layout)) {
          throw failure('invalid_execution_response', 'The saved layout response could not be validated.');
        }
        const incoming = immutable(result.layout);
        const base = state.base;
        if (base === null || incoming.workspace_id !== base.workspace_id) {
          throw failure('invalid_execution_response', 'The saved layout response is unrelated to this workspace.');
        }
        const attempt = state.attempt;
        // Old callbacks cannot consume a newer attempt. Same-revision contradictions remain visible.
        if (attempt === null || incoming.revision <= attempt.request.expected_revision) {
          if (incoming.revision > base.revision) {
            throw failure('invalid_execution_response', 'No matching layout save attempt exists.');
          }
          if (incoming.revision === base.revision && !same(incoming, base)) {
            state = freeze({ ...state, conflicted: true, problem: conflict() });
          }
          return;
        }
        if (!same(incoming, savedLayout(attempt.request))) {
          throw failure('invalid_execution_response', 'The saved layout response does not match the captured save attempt.');
        }
        if (state.warning !== null) {
          // Success resolves the outcome, but cannot override an unsupported authoritative record.
          state = freeze({
            ...state, attempt: null, dirty: true, conflicted: true, problem: state.warning
          });
          return;
        }
        const contradictory = incoming.revision === base.revision && !same(incoming, base);
        const safe = !state.conflicted && !contradictory && incoming.revision >= base.revision;
        const durable = incoming.revision > base.revision ? incoming : base;
        if (!safe || state.draft === null) {
          state = freeze({
            ...state, base: durable, attempt: null, dirty: true,
            conflicted: true, problem: conflict()
          });
          return;
        }
        const covered = attempt.edit_epoch === state.edit_epoch;
        // Only this exact attempt can safely advance the expected revision of later edits.
        const draft = covered ? immutable(durable) : immutable({
          ...state.draft, revision: durable.revision
        });
        state = freeze({
          ...state, base: durable, draft, attempt: null,
          dirty: !covered, conflicted: false, problem: null
        });
      });
    },

    reject(error: Wire.Error): void {
      boundary(() => {
        const detail = immutable(sanitizeExecutionError(error));
        if (detail.code === 'layout_revision_conflict') {
          // Native checks a matching receipt before revision preconditions. This rejection
          // therefore resolves the attempt as not committed; reapplication is still explicit.
          state = freeze({
            ...state, attempt: null, conflicted: true, problem: detail
          });
        } else {
          // Conservatively retain the whole DTO for transport, storage and invalid replies.
          state = freeze({ ...state, problem: detail });
        }
      });
    },

    reapply(): void {
      boundary(() => {
        if (state.warning !== null || state.base === null || state.draft === null ||
            !state.dirty || !state.conflicted || state.attempt !== null) {
          throw failure('layout_revision_conflict', 'Layout intent cannot be reapplied while an outcome is unresolved or the base is unavailable.');
        }
        if (state.base.revision <= state.draft.revision) {
          throw failure('layout_revision_conflict', 'Refresh a newer authoritative layout before reapplying intent.');
        }
        const draft = immutable(checkedLayout({ ...state.draft, revision: state.base.revision }));
        state = freeze({
          ...state, draft, edit_epoch: nextEpoch(state.edit_epoch),
          conflicted: false, problem: null, dirty: true
        });
      });
    }
  };
}

function plainRecord(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function panelIndex(layout: Wire.Layout, panelId: Wire.Id): number {
  checkedId(panelId);
  const index = layout.panels.findIndex(panel => panel.panel_id === panelId);
  if (index < 0) throw invalid();
  return index;
}

function collapsedGeometry(geometry: Wire.Geometry, stage: 'compact' | 'block'): Wire.Geometry {
  return {
    ...geometry,
    width: Math.min(geometry.width, stage === 'compact' ? 320 : 80),
    height: Math.min(geometry.height, 48)
  };
}

export function clampPanelForViewport(
  panel: Wire.PanelLayout,
  viewport: ViewportBounds,
  container: Wire.ContainerLayout | null
): DisplayGeometry {
  return boundary(() => {
    if (!plainRecord(viewport) || typeof viewport.width !== 'number' ||
        typeof viewport.height !== 'number' || !Number.isFinite(viewport.width) ||
        !Number.isFinite(viewport.height) || viewport.width < 0 || viewport.height < 0) throw invalid();
    // Validation-only envelope; never a durable/default workspace layout.
    checkedLayout({
      layout_version: 1, workspace_id: panel?.session_id, revision: 0,
      theme: 'dark', active_module: 'agents', sidebar_width: 300,
      panels: [panel], containers: container === null ? [] : [container]
    });
    if ((panel.container_id === null) !== (container === null) ||
        (container !== null && panel.container_id !== container.container_id)) throw invalid();
    const left = container === null ? 0 : Math.min(viewport.width, container.geometry.x);
    const top = container === null ? 0 : Math.min(viewport.height, container.geometry.y);
    const right = container === null ? viewport.width : Math.max(left,
      Math.min(viewport.width, container.geometry.x + container.geometry.width));
    const bottom = container === null ? viewport.height : Math.max(top,
      Math.min(viewport.height, container.geometry.y + container.geometry.height));
    const geometry = panel.stage === 'full' ? panel.geometry :
      collapsedGeometry(panel.geometry, panel.stage);
    const width = Math.min(geometry.width, right - left);
    const height = Math.min(geometry.height, bottom - top);
    return immutable({
      x: Math.max(left, Math.min(right - width, geometry.x)),
      y: Math.max(top, Math.min(bottom - height, geometry.y)), width, height
    });
  });
}

export function snapValue(value: number, shiftKey: boolean, grid: number = 20): number {
  return boundary(() => {
    if (typeof value !== 'number' || !Number.isFinite(value) ||
        typeof shiftKey !== 'boolean' || typeof grid !== 'number' ||
        !Number.isFinite(grid) || grid <= 0) throw invalid();
    if (!shiftKey) return value;
    const result = Math.round(value / grid) * grid;
    if (!Number.isFinite(result)) throw invalid();
    return result === 0 ? 0 : result;
  });
}

export function ensureSessionPanel(
  layout: Wire.Layout, sessionId: Wire.Id, newPanelId: Wire.Id
): { layout: Wire.Layout; panel: Wire.PanelLayout; created: boolean } {
  return boundary(() => {
    checkedLayout(layout);
    checkedId(sessionId);
    checkedId(newPanelId);
    const existing = layout.panels.find(panel => panel.session_id === sessionId);
    if (existing !== undefined) {
      return { layout: immutable(layout), panel: immutable(existing), created: false };
    }
    if (layout.panels.length >= 16) {
      throw failure('capacity_exceeded', 'The workspace panel capacity has been reached.');
    }
    const offset = layout.panels.length * 24;
    const geometry: Wire.Geometry = { x: 48 + offset, y: 64 + offset, width: 460, height: 350 };
    const panel: Wire.PanelLayout = {
      panel_id: newPanelId, session_id: sessionId, container_id: null,
      stage: 'full', geometry, expanded_geometry: { ...geometry }
    };
    const result = immutable(checkedLayout({ ...layout, panels: [...layout.panels, panel] }));
    return { layout: result, panel: immutable(panel), created: true };
  });
}

export function cyclePanelStage(layout: Wire.Layout, panelId: Wire.Id): Wire.Layout {
  return boundary(() => {
    checkedLayout(layout);
    const index = panelIndex(layout, panelId);
    const result = copy(layout);
    const panel = result.panels[index];
    switch (panel.stage) {
      case 'full':
        panel.expanded_geometry = { ...panel.geometry };
        panel.geometry = collapsedGeometry(panel.geometry, 'compact');
        panel.stage = 'compact';
        break;
      case 'compact':
        panel.geometry = collapsedGeometry(panel.geometry, 'block');
        panel.stage = 'block';
        break;
      case 'block':
        panel.geometry = { ...panel.expanded_geometry };
        panel.stage = 'full';
        break;
    }
    return immutable(checkedLayout(result));
  });
}

export function moveOrResizePanel(
  layout: Wire.Layout, panelId: Wire.Id, geometry: Wire.Geometry,
  containerId: Wire.Id | null
): Wire.Layout {
  return boundary(() => {
    checkedLayout(layout);
    const index = panelIndex(layout, panelId);
    if (containerId !== null) checkedId(containerId);
    const panels = layout.panels.map((panel, position): Wire.PanelLayout => position === index ? {
      ...panel, geometry, container_id: containerId,
      expanded_geometry: panel.stage === 'full' ? geometry : panel.expanded_geometry
    } : panel);
    // Validate before copying: malformed geometry must not be normalized by JSON serialization.
    return immutable(checkedLayout({ ...layout, panels }));
  });
}

export function updateWorkspaceChrome(
  layout: Wire.Layout,
  patch: Partial<Pick<Wire.Layout, 'theme' | 'active_module' | 'sidebar_width'>>
): Wire.Layout {
  return boundary(() => {
    checkedLayout(layout);
    if (!plainRecord(patch) || Object.keys(patch).some(key =>
      key !== 'theme' && key !== 'active_module' && key !== 'sidebar_width')) throw invalid();
    return immutable(checkedLayout({ ...layout, ...patch }));
  });
}
