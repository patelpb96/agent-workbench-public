// Integrator-owned thin desktop adapter. No providers, paths, shell commands or
// text-only shell error helper: the execution client preserves structured errors.
import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { createExecutionClient, ExecutionClientError, executionNotification } from './client.js';
import { createWorkspaceController } from './controller.js';
import { createLayoutManager } from './layout.js';
import type { TimerPort, Unlisten, WorkspaceController, WorkspaceControllerState } from './workspace-ports.js';

function desktopOnly(): ExecutionClientError {
  return new ExecutionClientError({ code: 'execution_transport_error',
    user_message: 'Simulated durable sessions are available in the desktop app, not this browser preview.',
    recoverable: true });
}
const desktopClient = createExecutionClient((command, args) => isTauri() ? invoke(command, args) : Promise.reject(desktopOnly()));
/** Read-only: the exact text a carried-context Codex turn would send (nothing is admitted or journaled). */
export function previewCodexContext(request: Parameters<typeof desktopClient.preview_codex_context>[0]) {
  return desktopClient.preview_codex_context(request);
}
export function createDesktopController(onChange?: (state: WorkspaceControllerState) => void): WorkspaceController {
  const timer: TimerPort = {
    setInterval: (callback, ms) => window.setInterval(callback, ms),
    clearInterval: handle => window.clearInterval(handle as number),
    setTimeout: (callback, ms) => window.setTimeout(callback, ms),
    clearTimeout: handle => window.clearTimeout(handle as number)
  };
  const nextId = () => crypto.randomUUID();
  return createWorkspaceController({
    client: createExecutionClient((command, args) => isTauri() ? invoke(command, args) : Promise.reject(desktopOnly())),
    subscribe: callback => isTauri()
      ? listen<unknown>(executionNotification, event => callback(event.payload))
      : Promise.reject(desktopOnly()),
    timer, nextId, layout: createLayoutManager(nextId), onChange
  });
}

/** Mount once from Svelte onMount; returned disposal is synchronous even when
 * native listener/visibility/start promises have not settled. No late restart. */
export function mountDesktopController(controller: WorkspaceController): Unlisten {
  let disposed = false;
  let nativeVisible = true;
  let visibilityEpoch = 0;
  let unlisten: Unlisten | null = null;
  const update = () => { if (!disposed) controller.setVisibility(!document.hidden && nativeVisible); };
  const refreshVisibility = async () => {
    const epoch = ++visibilityEpoch;
    if (isTauri()) {
      try {
        const current = getCurrentWindow();
        const [visible, minimized] = await Promise.all([current.isVisible(), current.isMinimized()]);
        if (disposed || epoch !== visibilityEpoch) return;
        nativeVisible = visible && !minimized;
      } catch {
        // Native visibility can be unavailable during close/reconnect. Retain
        // the last known value; document visibility/focus still reconcile.
      }
    }
    if (!disposed && epoch === visibilityEpoch) update();
  };
  const focus = () => {
    void refreshVisibility().then(() => { if (!disposed) controller.onFocus(); });
  };
  const visibility = () => {
    update();
    if (!document.hidden) void refreshVisibility();
  };
  window.addEventListener('focus', focus);
  document.addEventListener('visibilitychange', visibility);
  void (async () => {
    if (isTauri()) {
      try {
        const cleanup = await listen<unknown>('window_visibility_changed', event => {
          const value = event.payload;
          if (disposed || value === null || typeof value !== 'object' ||
              !('is_visible' in value) || typeof value.is_visible !== 'boolean') return;
          ++visibilityEpoch;
          nativeVisible = value.is_visible;
          update();
        });
        if (disposed) { cleanup(); return; }
        unlisten = cleanup;
      } catch {
        // Execution subscription has its own visible degraded state in the
        // controller. Document/focus observation remains active independently.
      }
    }
    await refreshVisibility();
    if (!disposed) {
      // The controller installs its execution listener BEFORE bootstrap.
      // It publishes sanitized failures; avoid an unhandled mount rejection.
      try { await controller.start(); } catch { /* surfaced in controller state */ }
    }
  })();
  return () => {
    if (disposed) return;
    disposed = true;
    ++visibilityEpoch;
    window.removeEventListener('focus', focus);
    document.removeEventListener('visibilitychange', visibility);
    unlisten?.();
    unlisten = null;
    controller.stop();
  };
}
