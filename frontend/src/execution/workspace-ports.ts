// Integrator-owned FRONTEND-ONLY seam. Not an I0 wire/IPC contract change.
import type * as Wire from '../execution-v2/generated.js';
import type { ExecutionClient, RunProgress } from './client.js';
import type { TranscriptState } from './reconcile.js';

export type Unlisten = () => void;
export type SubscribePort = (onHint: (payload: unknown) => void) => Promise<Unlisten>;
export interface TimerPort {
  setInterval(callback: () => void, ms: number): unknown;
  clearInterval(handle: unknown): void;
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}
export interface LayoutAttempt {
  readonly request: Wire.SaveLayoutRequest;
  readonly edit_epoch: number;
}
export interface LayoutIntentState {
  readonly base: Wire.Layout | null;
  readonly draft: Wire.Layout | null;
  readonly warning: Wire.Error | null;
  readonly edit_epoch: number;
  readonly attempt: LayoutAttempt | null;
  readonly dirty: boolean;
  readonly conflicted: boolean;
  readonly problem: Wire.Error | null;
}
export interface LayoutManager {
  getState(): LayoutIntentState;
  observe(layout: Wire.Layout | null, warning: Wire.Error | null): void;
  edit(draft: Wire.Layout): void;
  beginSave(): Wire.SaveLayoutRequest;
  acknowledge(result: Wire.SaveLayoutResponse): void;
  reject(error: Wire.Error): void;
  /** Explicitly reapply preserved intent to a refreshed base, only after a known conflict. */
  reapply(): void;
}
export interface ViewportBounds { readonly width: number; readonly height: number }
/** Display-only geometry can be zero-sized in a hidden/zero-sized viewport. Never persist it automatically. */
export interface DisplayGeometry { readonly x: number; readonly y: number; readonly width: number; readonly height: number }
export interface LayoutModule {
  createLayoutManager(nextId: () => Wire.Id): LayoutManager;
  canSaveLayout(state: LayoutIntentState): boolean;
  clampPanelForViewport(panel: Wire.PanelLayout, viewport: ViewportBounds, container: Wire.ContainerLayout | null): DisplayGeometry;
  snapValue(value: number, shiftKey: boolean, grid?: number): number;
  ensureSessionPanel(layout: Wire.Layout, sessionId: Wire.Id, newPanelId: Wire.Id): { layout: Wire.Layout; panel: Wire.PanelLayout; created: boolean };
  cyclePanelStage(layout: Wire.Layout, panelId: Wire.Id): Wire.Layout;
  moveOrResizePanel(layout: Wire.Layout, panelId: Wire.Id, geometry: Wire.Geometry, containerId: Wire.Id | null): Wire.Layout;
  updateWorkspaceChrome(layout: Wire.Layout, patch: Partial<Pick<Wire.Layout, 'theme' | 'active_module' | 'sidebar_width'>>): Wire.Layout;
}
export interface MutationAttempt {
  readonly key: string;
  readonly command: 'create_session' | 'start_fake_run' | 'start_codex_run' | 'cancel_run';
  readonly request_id: Wire.Id;
  readonly pending: boolean;
}
export interface WorkspaceControllerState {
  readonly status: 'connecting' | 'ready' | 'unavailable' | 'stopped';
  readonly snapshot: Wire.WorkspaceSnapshot | null;
  readonly selected_session_id: Wire.Id | null;
  readonly selected_run_by_session: Readonly<Record<Wire.Id, Wire.Id>>;
  /** One active viewport only. No retained per-run transcript/event cache. */
  readonly transcript: TranscriptState | null;
  /** Volatile live progress of the active Codex run; null when none. Never persisted. */
  readonly progress: RunProgress | null;
  readonly layout: LayoutIntentState;
  readonly attempts: readonly MutationAttempt[];
  readonly last_error: Wire.Error | null;
  readonly dirty: boolean;
  readonly in_flight: boolean;
  readonly mutation_pending: boolean;
  readonly visible: boolean;
  readonly listener_ready: boolean;
}
export interface WorkspaceController {
  getState(): WorkspaceControllerState;
  start(): Promise<void>;
  stop(): void;
  reconcile(): void;
  setVisibility(visible: boolean): void;
  onFocus(): void;
  selectSession(sessionId: Wire.Id): void;
  selectRun(sessionId: Wire.Id, runId: Wire.Id): void;
  loadEarlier(): void;
  returnToLive(): void;
  createSession(label: string, retryKey?: string, providerId?: Wire.ProviderId): Promise<void>;
  startFakeRun(sessionId: Wire.Id, prompt: string, scenario: Wire.FakeScenario, retryKey?: string): Promise<void>;
  /** `confirmed` must be the user's explicit per-run consent to send to Codex. */
  startCodexRun(sessionId: Wire.Id, prompt: string, confirmed: boolean, retryKey?: string, contextRunIds?: readonly Wire.Id[]): Promise<void>;
  cancelRun(runId: Wire.Id, retryKey?: string): Promise<void>;
  retryMutation(key: string): Promise<void>;
  editLayout(draft: Wire.Layout): void;
  saveLayout(): Promise<void>;
  reapplyLayout(): void;
}
export interface WorkspaceDependencies {
  readonly client: ExecutionClient;
  readonly subscribe: SubscribePort;
  readonly timer: TimerPort;
  readonly nextId: () => Wire.Id;
  readonly layout: LayoutManager;
  readonly onChange?: (state: WorkspaceControllerState) => void;
}
