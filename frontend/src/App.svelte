<script lang="ts">
  import { onMount } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { native, resetWindow, runtimeInfo, setAppearance, openBackground, selectedAssetUrl } from './native';
  import type { BackgroundKind, RuntimeInfo } from './native';
  import SessionPanel from './components/SessionPanel.svelte';
  import Section from './components/Section.svelte';
  import { sidebars } from './sidebars-store.svelte.js';
  import { ghost, hitRects } from './ghost.svelte.js';
  import { logoPath } from './brand.js';
  import { gridFor, zoneAt, type BarName, type Dock } from './sidebars.js';
  import TerminalsView from './terminals/TerminalsView.svelte';
  import FloatingTerminals from './terminals/FloatingTerminals.svelte';
  import PropertiesWindow from './settings/PropertiesWindow.svelte';
  import StartScreen from './StartScreen.svelte';
  import FilesPanel from './files/FilesPanel.svelte';
  import ResumeDialog from './terminals/ResumeDialog.svelte';
  import { agentStore } from './terminals/agent-store.svelte.js';
  import { keyControl } from './terminals/keys.svelte.js';
  import KeyHint from './terminals/KeyHint.svelte';
  import { settingsStore } from './settings/settings-store.svelte.js';
  import { defaultWindow } from './settings/settings.js';
  import { canvasTerminals } from './terminals/canvas-store.svelte.js';
  import { flip, link as linkTerminal, linkOf, unlink } from './terminals/floating.js';
  import InspectorView from './inspector/InspectorView.svelte';
  import RunBoard from './components/RunBoard.svelte';
  import UsageOverview from './components/UsageOverview.svelte';
  import EventTimeline from './components/EventTimeline.svelte';
  import { projectEventWindow } from './components/event-metadata.js';
  import WorkspaceDiagnostics from './components/WorkspaceDiagnostics.svelte';
  import { projectDiagnostics } from './components/diagnostics-metadata.js';
  import SessionTranscript from './components/SessionTranscript.svelte';
  import { createDesktopController, mountDesktopController, previewCodexContext } from './execution/tauri.js';
  import { ExecutionClientError, sanitizeExecutionError, utf8Bytes } from './execution/client.js';
  import { canSaveLayout, ensureSessionPanel, cyclePanelStage, moveOrResizePanel, clampPanelForViewport, snapValue, updateWorkspaceChrome } from './execution/layout.js';
  import { displayText, omission } from './execution/reconcile.js';
  import { limits, runTransitions } from './execution-v2/generated.js';
  import type * as Wire from './execution-v2/generated.js';
  import type { DisplayGeometry, WorkspaceControllerState } from './execution/workspace-ports.js';

  const modules: { id: Wire.ModuleName; label: string; icon: string }[] = [
    { id: 'agents', label: 'Canvas', icon: 'C' },
    { id: 'tasks', label: 'Tasks', icon: 'T' },
    { id: 'usage', label: 'Models & Usage', icon: 'M' },
    { id: 'approvals', label: 'Events & Approvals', icon: 'E' },
    { id: 'diagnostics', label: 'Logs & Diagnostics', icon: 'L' }
  ];
  let showTerminals = $state(false);
  let appFocused = $state(true);
  let showProperties = $state(false);
  let showInspector = $state(false);
  const controller = createDesktopController(next => { workspaceState = next; });
  let workspaceState: WorkspaceControllerState = $state.raw(controller.getState());
  let uiError = $state.raw<Wire.Error | null>(null);
  let disposed = false;
  let runtime = $state.raw<RuntimeInfo | undefined>();
  let message = $state(native ? 'Connecting to the native desktop shell…' : 'Browser preview — desktop persistence and execution are unavailable.');
  let previewModule = $state<Wire.ModuleName>('agents');
  let previewTheme = $state<Wire.ThemeName>('dark');
  let previewSide = $state(300);
  const draft = $derived(workspaceState.layout.draft);
  const editable = $derived(native && workspaceState.layout.base !== null && draft !== null && workspaceState.layout.warning === null);
  const active = $derived(editable && draft ? draft.active_module : previewModule);
  const theme = $derived(editable && draft ? draft.theme : previewTheme);
  const sideWidth = $derived(editable && draft ? draft.sidebar_width : previewSide);
  const sessions = $derived(workspaceState.snapshot?.sessions ?? []);
  const runs = $derived(workspaceState.snapshot?.runs ?? []);
  const selectedSession = $derived(sessions.find(session => session.session_id === workspaceState.selected_session_id));
  const selectedPanel = $derived(draft?.panels.find(panel => panel.session_id === workspaceState.selected_session_id));
  const selectedRunId = $derived(workspaceState.selected_session_id ? workspaceState.selected_run_by_session[workspaceState.selected_session_id] : undefined);
  const selectedRun = $derived(runs.find(run => run.run_id === selectedRunId && run.session_id === workspaceState.selected_session_id));
  const eventWindow = $derived(active === 'approvals' ? projectEventWindow(workspaceState.transcript, selectedRun) : null);
  const diagnosticMetadata = $derived(active === 'diagnostics' ? projectDiagnostics(workspaceState, runtime) : null);
  const ready = $derived(native && workspaceState.status === 'ready' && workspaceState.snapshot?.execution_available === true);
  const error = $derived(uiError ?? workspaceState.last_error);
  const layoutProblem = $derived(workspaceState.layout.warning ?? workspaceState.layout.problem);
  const plural = (n: number, word: string): string => `${n} ${word}${n === 1 ? '' : 's'}`;
  let label = $state('New session');
  const defaultLabels = { fake: 'New session', codex: 'Codex session' } as const;
  // Only swap an untouched default label; never overwrite what the user typed.
  function agentChanged(): void {
    if (label === defaultLabels.fake || label === defaultLabels.codex) label = defaultLabels[agent as 'fake' | 'codex'];
  }
  let prompt = $state('');
  let scenario = $state<Wire.FakeScenario>('success');
  let composerSession = $state<string | null>(null);
  // Codex (v2): availability comes only from native read-only pin verification.
  let agent = $state<Wire.ProviderId>('fake');
  let codexConfirmed = $state(false);
  // Carry earlier turns of this session into the next Codex turn (codex_text_v2). On by default;
  // the per-run consent below names exactly what is sent.
  let carryContext = $state(true);
  const codexAvailability = $derived(workspaceState.snapshot?.codex_availability ?? 'not_configured');
  const codexReady = $derived(codexAvailability === 'ready');
  const codexSession = $derived(selectedSession?.provider_id === 'codex');
  const codexPromptValid = $derived(prompt.length > 0 && utf8Bytes(prompt) <= limits.codex_prompt_bytes && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(prompt));
  const carriable = $derived(
    selectedSession === undefined ? [] : (workspaceState.snapshot?.runs ?? [])
      .filter(run => run.session_id === selectedSession.session_id && run.provider_id === 'codex' && run.state === 'succeeded')
      .sort((a, b) => a.enqueue_order - b.enqueue_order).slice(-limits.codex_context_runs));
  const contextRunIds = $derived(carryContext ? carriable.map(run => run.run_id) : []);
  // The exact text a carried-context turn would send, shown before consent. Tied to the inputs it was
  // computed for, so any change to the prompt, session or carried runs hides it.
  let contextPreview = $state<{ key: string; text: string; bytes: number; carried: number; trimmed: boolean } | null>(null);
  let previewError = $state<string | null>(null);
  const previewKey = $derived(`${workspaceState.selected_session_id}\u0000${prompt}\u0000${contextRunIds.join(',')}`);
  const previewShown = $derived(contextPreview !== null && contextPreview.key === previewKey ? contextPreview : null);
  async function showContext() {
    const workspaceId = workspaceState.snapshot?.workspace.workspace_id;
    const sessionId = workspaceState.selected_session_id;
    if (!workspaceId || !sessionId || contextRunIds.length === 0 || !codexPromptValid) return;
    const key = previewKey;
    previewError = null;
    try {
      const result = await previewCodexContext({ contract_version: 2, workspace_id: workspaceId, session_id: sessionId, prompt, context_run_ids: [...contextRunIds] });
      contextPreview = { key, text: result.text, bytes: result.bytes, carried: result.carried, trimmed: result.trimmed };
    } catch { previewError = 'The preview could not be produced; nothing was sent.'; }
  }
  // Consent names exactly this prompt and which earlier turns go along; changing the prompt, the session
  // or the carried set voids it.
  $effect(() => { void [contextRunIds.join(','), prompt, workspaceState.selected_session_id]; codexConfirmed = false; });
  const inFlight = $derived(selectedSession !== undefined && (workspaceState.snapshot?.runs ?? []).some(run =>
    run.session_id === selectedSession.session_id && ['queued', 'starting', 'running', 'cancel_requested'].includes(run.state)));
  const hasCodexStart = $derived(workspaceState.attempts.some(attempt => attempt.command === 'start_codex_run'));
  const validLabel = $derived(Array.from(label).length > 0 && Array.from(label).length <= 128 && !/[\u0000-\u001f\u007f-\u009f]/.test(label));
  const promptBytes = $derived(utf8Bytes(prompt));
  const validPrompt = $derived(prompt.length > 0 && promptBytes <= limits.prompt_bytes && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(prompt));
  const hasCreate = $derived(workspaceState.attempts.some(attempt => attempt.command === 'create_session'));
  const hasStart = $derived(workspaceState.attempts.some(attempt => attempt.command === 'start_fake_run'));
  const hasCancel = $derived(workspaceState.attempts.some(attempt => attempt.command === 'cancel_run'));
  const cancelable = $derived(selectedRun !== undefined && selectedRun.state !== 'cancel_requested' && runTransitions[selectedRun.state].length > 0);
  let surfaces = $state<Record<string, { fill: string; opacity: number }>>({});
  const selectedSurface = $derived(selectedPanel ? surfaces[selectedPanel.panel_id] ?? { fill: '#172534', opacity: 96 } : { fill: '#172534', opacity: 96 });

  // Sidebars: collapse (full, compact, flat) and, once collapsed, dock to the top or bottom edge (sidebars.ts).
  // The title bar slides away unless the cursor rests on the top edge for a moment (or something inside it has focus).
  // Properties > Appearance chooses (settings `window.titlebar_autohide`); `localStorage['aw.titlebar'] = 'pinned'` also keeps
  // it always showing (the headless tests use that).
  const pinnedByStorage = (() => { try { return localStorage.getItem('aw.titlebar') === 'pinned'; } catch { return false; } })();
  const autoHide = $derived(settingsStore.value.window.titlebar_autohide && !pinnedByStorage);
  // Sign out shows the start screen (StartScreen.svelte). It only covers the window: terminals and agents keep running and
  // no credential is touched. Remembered in this window's storage so a reload stays signed out.
  let signedOut = $state((() => { try { return localStorage.getItem('aw.signedOut') === '1'; } catch { return false; } })());
  function setSignedOut(value: boolean): void {
    signedOut = value;
    if (value) showProperties = false;
    try { if (value) localStorage.setItem('aw.signedOut', '1'); else localStorage.removeItem('aw.signedOut'); } catch { /* storage unavailable: this window only */ }
  }
  const REVEAL_MS = 250;
  const HIDE_MS = 400;
  const UNUSED_MS = 1500;
  let barShown = $state(false);
  let barFocused = $state(false);
  let revealTimer: ReturnType<typeof setTimeout> | undefined;
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  function edgeEnter() {
    clearTimeout(hideTimer);
    if (barShown) return;
    clearTimeout(revealTimer);
    revealTimer = setTimeout(() => {
      barShown = true;
      // If the cursor never comes down onto the bar (it went off the window, or is still in the resize border), the bar goes again.
      clearTimeout(hideTimer);
      hideTimer = setTimeout(() => { if (!barFocused) barShown = false; }, UNUSED_MS);
    }, REVEAL_MS);
  }
  // A window that is not maximized has a resize border along its top edge, and pointer events do not reach the page there. The strip
  // is therefore taller than that border, and leaving it upward (out of the page, so no element is entered) keeps the dwell going:
  // pushing the cursor to the top edge and resting there still reveals the bar. Leaving it downward cancels, as before.
  function edgeLeave(event: PointerEvent) {
    if (event.relatedTarget === null) return;
    clearTimeout(revealTimer);
  }
  function barEnter() { clearTimeout(hideTimer); }
  function barLeave() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => { if (!barFocused) barShown = false; }, HIDE_MS);
  }
  const grid = $derived(gridFor(sidebars.bars, sidebars.arrangement));
  // A debug build reports a version ending in -dev (native side: DEV_BUILD).
  const devBuild = $derived(runtime?.app_version.endsWith('-dev') ?? false);
  const layoutStatusText = $derived(workspaceState.layout.warning ? 'LAYOUT WARNING' : workspaceState.layout.conflicted ? 'CONFLICT' : workspaceState.layout.attempt ? 'SAVE PENDING' : workspaceState.layout.dirty ? 'STAGED · UNSAVED' : workspaceState.layout.base ? `SAVED · REV ${workspaceState.layout.base.revision}` : 'NO AUTHORITATIVE LAYOUT');
  // The badge shows just the revision number when everything is saved; the full status is its tooltip and accessible name.
  const layoutBadge = $derived(!workspaceState.layout.warning && !workspaceState.layout.conflicted && !workspaceState.layout.attempt && !workspaceState.layout.dirty && workspaceState.layout.base ? String(workspaceState.layout.base.revision) : layoutStatusText);
  $effect(() => { document.body.classList.toggle('picking-bar', dockDrag !== null); return () => document.body.classList.remove('picking-bar'); });
  // In the compact module list only the icons show, so each button says what it is by its label.
  const railName = (label: string): string | undefined => (sidebars.bars.rail.stage === 'full' ? undefined : label);
  let shellEl = $state<HTMLDivElement>();
  // See-through (Ctrl+Alt+G): on every tab the backgrounds disappear and clicks on bare background pass through to the
  // windows behind; panels, terminal windows, each view's contents, sidebars and the title bar stay usable.
  const ghostActive = $derived(native && ghost.on);
  $effect(() => {
    if (!native) return;
    if (!ghostActive) { ghost.report(false, false, []); return; }
    let last = '';
    let frame = 0;
    let fallback: ReturnType<typeof setTimeout> | undefined;
    const send = () => {
      if (frame !== 0) cancelAnimationFrame(frame);
      clearTimeout(fallback);
      frame = 0;
      fallback = undefined;
      const { whole, rects } = hitRects();
      const key = JSON.stringify([whole, rects]);
      if (key === last) return;
      last = key;
      ghost.report(true, whole, rects);
    };
    // A frame callback can be held back (the window hidden while it starts, restored see-through from the last run):
    // the timer sends anyway, so one held-back frame never leaves the native side with stale areas. Seen as a dead zone
    // below and right of the window's starting size until the mode was toggled.
    const schedule = () => {
      if (frame === 0) frame = requestAnimationFrame(send);
      fallback ??= setTimeout(send, 120);
    };
    const watch = new MutationObserver(schedule);
    watch.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ['style', 'class', 'hidden', 'open'] });
    const sizes = new ResizeObserver(schedule);
    sizes.observe(document.body);
    window.addEventListener('resize', schedule);
    window.addEventListener('focus', schedule);
    document.addEventListener('visibilitychange', schedule);
    // And a slow check on its own: a layout change that touches no observed attribute is picked up within a second.
    const steady = setInterval(send, 1000);
    send();
    return () => {
      watch.disconnect();
      sizes.disconnect();
      window.removeEventListener('resize', schedule);
      window.removeEventListener('focus', schedule);
      document.removeEventListener('visibilitychange', schedule);
      clearInterval(steady);
      clearTimeout(fallback);
      if (frame !== 0) cancelAnimationFrame(frame);
      ghost.report(false, false, []);
    };
  });
  // The native window is made transparent for the duration (the solid colour sits in the page canvas otherwise).
  let ghostApplied = false;
  $effect(() => {
    const now = ghostActive;
    if (now === ghostApplied) return;
    ghostApplied = now;
    if (now) void setAppearance({ background_kind: 'transparent', color_rgba: `${background}ff`, window_alpha: 0 }).catch(() => undefined);
    else void applyAppearance();
  });
  let dockDrag = $state<{ name: BarName; zone: Dock } | null>(null);
  function dockStart(event: PointerEvent, name: BarName) {
    if (event.button !== 0) return;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    event.preventDefault();
    dockDrag = { name, zone: sidebars.bars[name].dock };
  }
  function dockMove(event: PointerEvent) {
    if (!dockDrag || !shellEl) return;
    dockDrag = { ...dockDrag, zone: zoneAt(event.clientY, shellEl.getBoundingClientRect()) };
  }
  function dockEnd(event: PointerEvent) {
    if (!dockDrag) return;
    (event.currentTarget as HTMLElement).releasePointerCapture?.(event.pointerId);
    sidebars.drop(dockDrag.name, dockDrag.zone);
    dockDrag = null;
  }
  // Press and hold anywhere on a sidebar (about a third of a second, without moving) to pick it up, then drag it to the
  // top or bottom edge. A quick click, or a press that moves, behaves as usual; typing fields are never grabbed.
  const HOLD_MS = 300;
  const HOLD_SLOP = 6;
  let hold: { name: BarName; x: number; y: number; pointerId: number; bar: HTMLElement; timer: ReturnType<typeof setTimeout> } | null = null;
  function holdCancel() {
    if (hold) clearTimeout(hold.timer);
    hold = null;
  }
  function holdStart(event: PointerEvent, name: BarName) {
    if (event.button !== 0 || dockDrag) return;
    const target = event.target as HTMLElement;
    if (target.closest('.bar-grip, input, textarea, select, [contenteditable="true"]')) return;
    const bar = event.currentTarget as HTMLElement;
    holdCancel();
    const timer = setTimeout(() => {
      if (!hold) return;
      const { pointerId, bar: el } = hold;
      hold = null;
      try { el.setPointerCapture(pointerId); } catch { return; }
      window.getSelection()?.removeAllRanges();
      // The release that ends the drag must not also click the control it started on.
      const swallow = (e: Event) => { e.preventDefault(); e.stopPropagation(); };
      window.addEventListener('click', swallow, { capture: true, once: true });
      setTimeout(() => window.removeEventListener('click', swallow, { capture: true }), 400);
      dockDrag = { name, zone: sidebars.bars[name].dock };
    }, HOLD_MS);
    hold = { name, x: event.clientX, y: event.clientY, pointerId: event.pointerId, bar, timer };
  }
  function holdMove(event: PointerEvent) {
    if (dockDrag) { dockMove(event); return; }
    if (hold && Math.hypot(event.clientX - hold.x, event.clientY - hold.y) > HOLD_SLOP) holdCancel();
  }
  function holdEnd(event: PointerEvent) {
    if (dockDrag) dockEnd(event);
    else holdCancel();
  }
  function holdAbort() {
    holdCancel();
    dockDrag = null;
  }
  // Keyboard: Up and Down move a collapsed bar through top, side, bottom.
  function dockKey(event: KeyboardEvent, name: BarName) {
    const now = sidebars.bars[name].dock;
    const order: Dock[] = ['top', 'side', 'bottom'];
    const step = event.key === 'ArrowUp' ? -1 : event.key === 'ArrowDown' ? 1 : 0;
    if (step === 0) return;
    event.preventDefault();
    sidebars.dock(name, order[Math.max(0, Math.min(2, order.indexOf(now) + step))] ?? now);
  }

  let canvas = $state<HTMLDivElement>();
  let viewport = $state({ width: 0, height: 0 });
  let shift = $state(false);
  let dragging = $state(false);
  type Interaction = { kind: 'move' | 'resize' | 'container-move' | 'container-resize' | 'side'; id: string; x: number; y: number; geometry: Wire.Geometry; side: number };
  let interaction = $state.raw<Interaction | undefined>();
  // The snap grid appears only while Shift is held DURING a pointer drag of a
  // panel or container (move or resize) - not for any Shift press, keyboard
  // nudges or the inspector splitter (user request 2026-10-21).
  const snapGrid = $derived(shift && dragging && interaction !== undefined && interaction.kind !== 'side');
  let mode = $state<BackgroundKind>('solid');
  let background = $state('#0c1118');
  let alpha = $state(100);
  let mediaUrl = $state('');
  let video = $state<HTMLVideoElement>();
  let pageVisible = $state(true);
  let choosing = $state(false);
  // The outcome of the last background change, shown next to the controls (the status bar is hidden behind Properties).
  let backgroundNote = $state('');
  // Sidebar opacities (Properties > Appearance): local so a drag shows at once, saved when released.
  let side1 = $state(100);
  let side2 = $state(100);
  $effect(() => { side1 = settingsStore.value.appearance.sidebar1_opacity; side2 = settingsStore.value.appearance.sidebar2_opacity; });
  function saveSidebars(): void {
    void settingsStore.update(v => ({ ...v, appearance: { ...v.appearance, sidebar1_opacity: side1, sidebar2_opacity: side2 } }));
  }
  // The canvas background (mode, colour, alpha) and the see-through mode are remembered between launches (settings `window`).
  // Nothing is written until the saved values have been applied once, so a start never overwrites them with the defaults.
  let windowRestored = $state(false);
  function rememberCanvas(): void {
    if (!windowRestored) return;
    const saved = settingsStore.value.window;
    const next = {
      ...saved,
      canvas_mode: mode === 'solid' || mode === 'transparent' ? mode : saved.canvas_mode,
      canvas_color: /^#[0-9a-fA-F]{6}$/.test(background) ? background : saved.canvas_color,
      canvas_alpha: alpha
    };
    if (next.canvas_mode === saved.canvas_mode && next.canvas_color === saved.canvas_color && next.canvas_alpha === saved.canvas_alpha) return;
    void settingsStore.update(s => ({ ...s, window: { ...s.window, canvas_mode: next.canvas_mode, canvas_color: next.canvas_color, canvas_alpha: next.canvas_alpha } }));
  }
  async function restoreWindow(): Promise<void> {
    const saved = settingsStore.value.window;
    mode = saved.canvas_mode; background = saved.canvas_color; alpha = saved.canvas_alpha;
    if (native && (saved.canvas_mode !== defaultWindow.canvas_mode || saved.canvas_color !== defaultWindow.canvas_color || saved.canvas_alpha !== defaultWindow.canvas_alpha)) await applyAppearance();
    // A restored see-through canvas shows its SEE-THROUGH note from the first frame, and the hotkey turns it off as always.
    if (native && saved.see_through && !ghost.on) await ghost.toggle();
    windowRestored = true;
  }
  $effect(() => {
    const on = ghost.on;
    if (!windowRestored || !native || on === settingsStore.value.window.see_through) return;
    void settingsStore.update(s => ({ ...s, window: { ...s.window, see_through: on } }));
  });
  let appearancePending = $state(false);
  let appearanceSequence = 0;
  let mediaSequence = 0;

  $effect(() => {
    const id = workspaceState.selected_session_id;
    if (id !== composerSession) {
      composerSession = id;
      prompt = '';
      scenario = 'success';
      codexConfirmed = false; // Consent never carries over to another session.
    }
  });
  $effect(() => {
    if (!video) return;
    video.muted = true;
    if (workspaceState.visible && pageVisible && mode === 'video') {
      void video.play().catch(() => { if (!disposed && mode === 'video') mediaFailed(); });
    } else video.pause();
  });

  function act(operation: () => void) {
    try { operation(); uiError = null; }
    catch (failure: unknown) { uiError = sanitizeExecutionError(failure); }
  }
  async function perform(operation: () => Promise<void>): Promise<boolean> {
    uiError = null;
    try { await operation(); return !disposed; }
    catch (failure: unknown) { if (!disposed) uiError = sanitizeExecutionError(failure); return false; }
  }
  function layoutRequired(): Wire.Layout {
    const current = controller.getState().layout;
    if (!native || current.base === null || current.draft === null || current.warning !== null) {
      throw new ExecutionClientError({ code: 'layout_invalid', user_message: 'A valid authoritative desktop layout is required. The stored layout will not be replaced.', recoverable: true });
    }
    return current.draft;
  }
  function edit(transform: (layout: Wire.Layout) => Wire.Layout) {
    act(() => controller.editLayout(transform(layoutRequired())));
  }
  function chrome(patch: Partial<Pick<Wire.Layout, 'theme' | 'active_module' | 'sidebar_width'>>) {
    if (editable) edit(layout => updateWorkspaceChrome(layout, patch));
    else {
      if (patch.theme !== undefined) previewTheme = patch.theme;
      if (patch.active_module !== undefined) previewModule = patch.active_module;
      if (patch.sidebar_width !== undefined) previewSide = patch.sidebar_width;
    }
  }
  function openSession(sessionId: string) {
    act(() => {
      controller.selectSession(sessionId);
      const current = controller.getState().layout;
      if (!native || current.base === null || current.draft === null || current.warning !== null) return;
      const existing = current.draft.panels.find(panel => panel.session_id === sessionId);
      const result = ensureSessionPanel(current.draft, sessionId, existing?.panel_id ?? crypto.randomUUID());
      if (result.created) controller.editLayout(result.layout);
    });
  }
  function inspectRun(sessionId: string, runId: string) {
    act(() => {
      // Selection is read-only. Revealing a collapsed/absent panel and changing
      // modules edits only the layout draft; it never saves or dispatches work.
      controller.selectRun(sessionId, runId);
      const current = controller.getState().layout;
      if (!native || current.base === null || current.draft === null || current.warning !== null) {
        previewModule = 'agents';
        return;
      }
      const existing = current.draft.panels.find(panel => panel.session_id === sessionId);
      const result = ensureSessionPanel(current.draft, sessionId, existing?.panel_id ?? crypto.randomUUID());
      let revealed = result.layout;
      if (result.panel.stage === 'compact') revealed = cyclePanelStage(revealed, result.panel.panel_id);
      if (result.panel.stage !== 'full') revealed = cyclePanelStage(revealed, result.panel.panel_id);
      controller.editLayout(updateWorkspaceChrome(revealed, { active_module: 'agents' }));
    });
  }
  function sessionRuns(sessionId: string) {
    return runs.filter(run => run.session_id === sessionId).sort((a, b) => b.enqueue_order - a.enqueue_order);
  }
  function selectRun(sessionId: string, runId: string) { act(() => controller.selectRun(sessionId, runId)); }
  async function createSession() {
    const known = new Set(controller.getState().snapshot?.sessions.map(session => session.session_id) ?? []);
    const submittedLabel = label;
    const submittedAgent = agent === 'codex' && codexReady ? 'codex' : 'fake';
    if (await perform(() => controller.createSession(submittedLabel, submittedAgent === 'codex' ? 'create_codex_session' : 'create_session', submittedAgent))) {
      const created = controller.getState().snapshot?.sessions.find(session => !known.has(session.session_id));
      if (created) openSession(created.session_id);
    }
  }
  async function send() {
    const sessionId = workspaceState.selected_session_id;
    if (!sessionId || !validPrompt) return;
    const submittedPrompt = prompt;
    const submittedScenario = scenario;
    const highest = Math.max(0, ...(controller.getState().snapshot?.runs.filter(run => run.session_id === sessionId).map(run => run.enqueue_order) ?? []));
    if (await perform(() => controller.startFakeRun(sessionId, submittedPrompt, submittedScenario))) {
      const created = controller.getState().snapshot?.runs.filter(run => run.session_id === sessionId && run.enqueue_order > highest).sort((a, b) => b.enqueue_order - a.enqueue_order)[0];
      if (created) { openSession(sessionId); selectRun(sessionId, created.run_id); }
      if (workspaceState.selected_session_id === sessionId && prompt === submittedPrompt) prompt = '';
    }
  }
  async function sendCodex() {
    const sessionId = workspaceState.selected_session_id;
    // Explicit per-run consent: the checkbox is reset after every send attempt.
    if (!sessionId || !codexPromptValid || !codexConfirmed) return;
    const submittedPrompt = prompt;
    codexConfirmed = false;
    const highest = Math.max(0, ...(controller.getState().snapshot?.runs.filter(run => run.session_id === sessionId).map(run => run.enqueue_order) ?? []));
    const carried = [...contextRunIds];
    if (await perform(() => controller.startCodexRun(sessionId, submittedPrompt, true, undefined, carried))) {
      const created = controller.getState().snapshot?.runs.filter(run => run.session_id === sessionId && run.enqueue_order > highest).sort((a, b) => b.enqueue_order - a.enqueue_order)[0];
      if (created) { openSession(sessionId); selectRun(sessionId, created.run_id); }
      if (workspaceState.selected_session_id === sessionId && prompt === submittedPrompt) prompt = '';
    }
  }
  async function retry(key: string) {
    const before = controller.getState();
    const known = new Set(before.snapshot?.sessions.map(session => session.session_id) ?? []);
    const highest = Math.max(0, ...(before.snapshot?.runs.map(run => run.enqueue_order) ?? []));
    if (await perform(() => controller.retryMutation(key))) {
      const after = controller.getState();
      const createdSession = after.snapshot?.sessions.find(session => !known.has(session.session_id));
      const createdRun = after.snapshot?.runs.filter(run => run.enqueue_order > highest).sort((a, b) => b.enqueue_order - a.enqueue_order)[0];
      if (createdSession) openSession(createdSession.session_id);
      if (createdRun) { openSession(createdRun.session_id); selectRun(createdRun.session_id, createdRun.run_id); }
    }
  }
  function cancelRun(runId: string) { void perform(() => controller.cancelRun(runId)); }
  function cycle(panelId: string) { edit(layout => cyclePanelStage(layout, panelId)); }
  function panelDisplay(panel: Wire.PanelLayout): DisplayGeometry {
    const container = draft?.containers.find(item => item.container_id === panel.container_id) ?? null;
    return clampPanelForViewport(panel, viewport, container);
  }
  function containerDisplay(geometry: Wire.Geometry): DisplayGeometry {
    const width = Math.min(geometry.width, viewport.width);
    const height = Math.min(geometry.height, viewport.height);
    return { x: Math.max(0, Math.min(viewport.width - width, geometry.x)), y: Math.max(0, Math.min(viewport.height - height, geometry.y)), width, height };
  }
  function rectangle(geometry: DisplayGeometry): string {
    return `left:${geometry.x}px;top:${geometry.y}px;width:${geometry.width}px;height:${geometry.height}px;`;
  }
  function boundedGeometry(geometry: Wire.Geometry): Wire.Geometry {
    return { x: Math.max(0, Math.min(100000, geometry.x)), y: Math.max(0, Math.min(100000, geometry.y)), width: Math.max(1, Math.min(100000, geometry.width)), height: Math.max(1, Math.min(100000, geometry.height)) };
  }
  function resizeOrigin(stored: Wire.Geometry, displayed: DisplayGeometry): Wire.Geometry {
    // Logical sizes must not inherit clipping; anchor stays at the visible corner.
    return { ...stored, x: displayed.x, y: displayed.y };
  }
  function changePanel(panelId: string, geometry: Wire.Geometry, intent: 'move' | 'resize' | 'nest', containerId?: string | null) {
    edit(layout => {
      const panel = layout.panels.find(item => item.panel_id === panelId);
      if (!panel) return layout;
      const parentId = containerId === undefined ? panel.container_id : containerId;
      if (parentId !== null && (parentId === panelId || layout.panels.some(item => item.panel_id === parentId) || !layout.containers.some(item => item.container_id === parentId))) {
        throw new ExecutionClientError({ code: 'layout_invalid', user_message: 'Only a root container may parent a session panel. Self, panel-parent and deeper nesting are blocked.', recoverable: true });
      }
      // A display clamp is not a resize request. Preserve durable dimensions
      // for move/nest; only explicit resize intent may replace them.
      const requested = boundedGeometry({ ...geometry,
        width: intent === 'resize' ? geometry.width : panel.geometry.width,
        height: intent === 'resize' ? geometry.height : panel.geometry.height });
      const candidate = { ...panel, container_id: parentId, geometry: requested };
      const parent = layout.containers.find(item => item.container_id === parentId) ?? null;
      const rendered = clampPanelForViewport(candidate, viewport, parent);
      if (rendered.width < 1 || rendered.height < 1) return layout;
      return moveOrResizePanel(layout, panelId, { ...requested, x: rendered.x, y: rendered.y }, parentId);
    });
  }
  function addContainer() {
    edit(layout => {
      if (layout.containers.length >= 8) throw new ExecutionClientError({ code: 'capacity_exceeded', user_message: 'The eight-container limit has been reached.', recoverable: true });
      return { ...layout, containers: [...layout.containers, { container_id: crypto.randomUUID(), geometry: { x: 24 + layout.containers.length * 20, y: 48 + layout.containers.length * 20, width: 540, height: 440 } }] };
    });
  }
  function changeContainer(containerId: string, geometry: Wire.Geometry, intent: 'move' | 'resize') {
    edit(layout => {
      const container = layout.containers.find(item => item.container_id === containerId);
      if (!container) return layout;
      const requested = boundedGeometry({ ...geometry,
        width: intent === 'resize' ? geometry.width : container.geometry.width,
        height: intent === 'resize' ? geometry.height : container.geometry.height });
      const rendered = containerDisplay(requested);
      if (rendered.width < 1 || rendered.height < 1) return layout;
      return { ...layout, containers: layout.containers.map(item => item.container_id === containerId
        ? { ...item, geometry: { ...requested, x: rendered.x, y: rendered.y } } : item) };
    });
  }
  function cancelInteraction() { interaction = undefined; dragging = false; shift = false; }
  function begin(event: PointerEvent, kind: Interaction['kind'], id = '') {
    if (event.button !== 0 || (kind !== 'side' && !editable)) return;
    const panel = draft?.panels.find(item => item.panel_id === id);
    const container = draft?.containers.find(item => item.container_id === id);
    const geometry = panel
      ? (kind === 'resize' ? resizeOrigin(panel.geometry, panelDisplay(panel)) : panelDisplay(panel))
      : container
        ? (kind === 'container-resize' ? resizeOrigin(container.geometry, containerDisplay(container.geometry)) : containerDisplay(container.geometry))
        : { x: 0, y: 0, width: 1, height: 1 };
    if (panel) act(() => controller.selectSession(panel.session_id));
    event.preventDefault();
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    interaction = { kind, id, x: event.clientX, y: event.clientY, geometry: { ...geometry }, side: sideWidth };
    dragging = true;
    shift = event.shiftKey;
  }
  function move(event: PointerEvent) {
    if (!interaction) return;
    shift = event.shiftKey;
    const { kind, id, geometry, side, x, y } = interaction;
    const dx = event.clientX - x, dy = event.clientY - y;
    if (kind === 'side') { chrome({ sidebar_width: Math.max(240, Math.min(440, snapValue(side - (sidebars.arrangement.side === 'left' ? dx : -dx), event.shiftKey))) }); return; }
    const resizing = kind === 'resize' || kind === 'container-resize';
    const next = resizing
      ? { ...geometry, width: snapValue(Math.max(160, geometry.width + dx), event.shiftKey), height: snapValue(Math.max(120, geometry.height + dy), event.shiftKey) }
      : { ...geometry, x: snapValue(geometry.x + dx, event.shiftKey), y: snapValue(geometry.y + dy, event.shiftKey) };
    if (kind === 'container-move' || kind === 'container-resize') changeContainer(id, next, resizing ? 'resize' : 'move');
    else changePanel(id, next, resizing ? 'resize' : 'move');
  }
  function keyGeometry(event: KeyboardEvent, kind: 'move' | 'resize' | 'container-move' | 'container-resize', id: string) {
    const offsets: Record<string, readonly [number, number]> = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] };
    const offset = offsets[event.key];
    shift = event.shiftKey;
    if (!offset || !editable) return;
    event.preventDefault();
    const panel = draft?.panels.find(item => item.panel_id === id);
    const container = draft?.containers.find(item => item.container_id === id);
    const geometry = panel
      ? (kind === 'resize' ? resizeOrigin(panel.geometry, panelDisplay(panel)) : panelDisplay(panel))
      : container
        ? (kind === 'container-resize' ? resizeOrigin(container.geometry, containerDisplay(container.geometry)) : containerDisplay(container.geometry))
        : undefined;
    if (!geometry) return;
    const step = event.shiftKey ? 20 : 5;
    const resizing = kind === 'resize' || kind === 'container-resize';
    const next = resizing
      ? { ...geometry, width: snapValue(Math.max(1, geometry.width + offset[0] * step), event.shiftKey), height: snapValue(Math.max(1, geometry.height + offset[1] * step), event.shiftKey) }
      : { ...geometry, x: snapValue(geometry.x + offset[0] * step, event.shiftKey), y: snapValue(geometry.y + offset[1] * step, event.shiftKey) };
    if (container) changeContainer(id, next, resizing ? 'resize' : 'move'); else changePanel(id, next, resizing ? 'resize' : 'move');
  }
  function nestSelected(containerId: string) {
    if (!selectedPanel) return;
    changePanel(selectedPanel.panel_id, { ...selectedPanel.geometry }, 'nest', containerId === '' ? null : containerId);
  }
  function changeSurface(patch: Partial<{ fill: string; opacity: number }>) {
    if (!selectedPanel) return;
    surfaces = { ...surfaces, [selectedPanel.panel_id]: { ...selectedSurface, ...patch } };
  }
  function windowAction(action: 'minimize' | 'maximize' | 'close') {
    if (!native) return;
    const current = getCurrentWindow();
    const operation = action === 'minimize' ? current.minimize() : action === 'maximize' ? current.toggleMaximize() : current.close();
    void operation.catch(() => { if (!disposed) message = 'The native window action could not be completed.'; });
  }
  // Double-click on the title bar maximizes / restores. Starting a native drag on the first press can swallow the
  // browser's own dblclick, so the second press is recognised here by time and distance instead.
  let lastTitlePress = { at: 0, x: 0, y: 0 };
  function startWindowDrag(event: MouseEvent) {
    if (!native || event.button !== 0) return;
    event.preventDefault();
    const now = performance.now();
    const again = now - lastTitlePress.at < 500 && Math.hypot(event.clientX - lastTitlePress.x, event.clientY - lastTitlePress.y) < 6;
    lastTitlePress = again ? { at: 0, x: 0, y: 0 } : { at: now, x: event.clientX, y: event.clientY };
    if (again) { windowAction('maximize'); return; }
    void getCurrentWindow().startDragging().catch(() => { if (!disposed) message = 'Native window dragging is unavailable.'; });
  }
  async function applyAppearance() {
    if (!native) return;
    const sequence = ++appearanceSequence;
    appearancePending = true;
    try {
      await setAppearance({ background_kind: mode, color_rgba: background + 'ff', window_alpha: alpha });
      if (!disposed && sequence === appearanceSequence) { message = 'Native appearance applied.'; rememberCanvas(); }
    } catch {
      if (disposed || sequence !== appearanceSequence) return;
      mode = 'solid'; mediaUrl = '';
      message = 'Native appearance unavailable or unsupported — solid fallback active.';
      try { await setAppearance({ background_kind: 'solid', color_rgba: background + 'ff', window_alpha: 100 }); } catch { /* Keep the frontend solid fallback. */ }
    } finally { if (!disposed && sequence === appearanceSequence) appearancePending = false; }
  }
  async function choose(kind: 'image' | 'video') {
    if (!native || choosing) return;
    const sequence = ++mediaSequence;
    choosing = true;
    try {
      const asset = await openBackground(kind);
      if (disposed || sequence !== mediaSequence) return;
      if (asset.status === 'cancelled') { message = 'Background selection cancelled.'; backgroundNote = 'No file chosen; the background is unchanged.'; return; }
      if (asset.kind !== kind) throw new Error();
      const url = selectedAssetUrl(asset);
      mediaUrl = url; mode = kind;
      backgroundNote = `Loading ${asset.display_name ?? 'the file'}…`;
      await applyAppearance();
      if (!disposed && sequence === mediaSequence && mediaUrl === url) backgroundNote = `Using ${asset.display_name ?? 'the file'}${asset.width ? ` (${asset.width}×${asset.height})` : ''} at ${alpha}% opacity. It shows behind every tab; use the alpha slider to change how strong it is.`;
    } catch (e) {
      if (!disposed && sequence === mediaSequence) {
        mode = 'solid'; mediaUrl = '';
        const why = e instanceof Error && e.message ? displayText(e.message) : 'The file could not be validated.';
        message = 'Selected media could not be validated — solid fallback active.';
        backgroundNote = `${why} The solid background is in use.`;
        void applyAppearance();
      }
    } finally { if (!disposed && sequence === mediaSequence) choosing = false; }
  }
  function changeBackgroundMode(requested: BackgroundKind) {
    if (requested === 'image' || requested === 'video') { void choose(requested); return; }
    ++mediaSequence;
    mode = requested; mediaUrl = ''; backgroundNote = '';
    void applyAppearance();
  }
  function mediaFailed() {
    video?.pause();
    mode = 'solid'; mediaUrl = '';
    message = 'Media decode or playback failed — solid fallback active.';
    backgroundNote = 'The file could not be displayed (it may be damaged or not a real image or video). The solid background is in use.';
    if (native) void setAppearance({ background_kind: 'solid', color_rgba: background + 'ff', window_alpha: 100 }).catch(() => {});
  }

  onMount(() => {
    disposed = false;
    void Promise.all([ghost.init(), settingsStore.load()]).then(() => { if (!disposed) return restoreWindow(); }).catch(() => { windowRestored = true; });
    const stopController = mountDesktopController(controller);
    const measure = () => {
      if (!canvas) return;
      const bounds = canvas.getBoundingClientRect();
      viewport = { width: Math.max(0, bounds.width), height: Math.max(0, bounds.height) };
    };
    const observer = new ResizeObserver(measure);
    if (canvas) observer.observe(canvas);
    measure();
    pageVisible = !document.hidden;
    const visibility = () => { pageVisible = !document.hidden; };
    document.addEventListener('visibilitychange', visibility);
    // Free-form terminals (floating windows, session links): read the saved arrangement and keep the list fresh.
    void canvasTerminals.load();
    void canvasTerminals.refresh();
    const terminalPoll = setInterval(() => { if (!document.hidden) void canvasTerminals.refresh(); }, 3000);
    const stopAgents = agentStore.start();
    // tmux-style prefix key in terminals (keymap.ts): one listener for the whole window.
    const stopKeys = keyControl.install();
    // Whether Workbench has the keyboard: when another window does (or a see-through click went to the desktop), the
    // whole window turns gray, so keys are not typed into the wrong place by mistake.
    appFocused = document.hasFocus();
    const focusIn = (): void => { appFocused = true; };
    const focusOut = (): void => { appFocused = false; };
    window.addEventListener('focus', focusIn);
    window.addEventListener('blur', focusOut);
    // A session opened in a terminal lives on the Canvas: go there.
    agentStore.onOpened(() => { showInspector = false; showTerminals = false; if (active !== 'agents') chrome({ active_module: 'agents' }); });
    if (native) void runtimeInfo().then(value => {
      if (disposed) return;
      if (value.interface_version !== 1) { message = 'Unsupported native shell interface.'; return; }
      runtime = value;
      message = 'Native shell connected. Local test runner and the pinned read-only Codex profile; approvals and automatic handoffs are disabled.';
    }).catch(() => { if (!disposed) message = 'Native shell information is unavailable. No provider action was requested.'; });
    return () => {
      disposed = true;
      ++appearanceSequence; ++mediaSequence;
      agentStore.onOpened(null);
      cancelInteraction();
      stopController();
      observer.disconnect();
      document.removeEventListener('visibilitychange', visibility);
      clearInterval(terminalPoll);
      stopAgents();
      stopKeys();
      window.removeEventListener('focus', focusIn);
      window.removeEventListener('blur', focusOut);
      video?.pause();
    };
  });
</script>

{#snippet saveIcon()}
  <svg class="ico" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"><path d="M7 17.5h10.5V6.5" /><path d="M3.5 3.5h9l2 2v9h-11z" fill="var(--control)" /><path d="M5.5 3.5v3.5h5.5V3.5" /><rect x="5.5" y="10" width="7" height="4.5" /></svg>
{/snippet}
{#snippet refreshIcon()}
  <svg class="ico" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M16.2 8.5A6.5 6.5 0 0 0 4.4 6.3" /><path d="M4.2 3v3.6h3.6" /><path d="M3.8 11.5a6.5 6.5 0 0 0 11.8 2.2" /><path d="M15.8 17v-3.6h-3.6" /></svg>
{/snippet}
{#snippet layoutActions()}
  <div class="layout-actions">
    <span class="badge" title={layoutStatusText} aria-label={layoutStatusText}>{layoutBadge}</span>
    <button type="button" class="icon-btn" class:pending={workspaceState.layout.dirty || workspaceState.layout.attempt !== null} aria-label={workspaceState.layout.attempt ? 'Retry exact layout save' : 'Save layout'} title={workspaceState.layout.attempt ? 'Retry the exact layout save' : 'Save layout'} disabled={!native || workspaceState.mutation_pending || !canSaveLayout(workspaceState.layout)} onclick={() => void perform(() => controller.saveLayout())}>{@render saveIcon()}</button>
    <button type="button" class="icon-btn" aria-label="Refresh" title="Refresh from the saved layout and history" disabled={!native} onclick={() => act(() => controller.reconcile())}>{@render refreshIcon()}</button>
  </div>
{/snippet}
{#snippet moduleButton(item: { id: Wire.ModuleName; label: string; icon: string })}
  <button title={item.label} aria-label={railName(item.label)} class:active={active === item.id} aria-current={active === item.id ? 'page' : undefined} onclick={() => { showTerminals = false; showInspector = false; chrome({ active_module: item.id }); }}><span class="nav-icon">{item.icon}</span><span class="nav-label">{item.label}</span>{#if item.id === 'tasks'}<small>RUNS</small>{:else if item.id === 'usage'}<small>RUN USAGE</small>{:else if item.id === 'approvals'}<small>EVENT METADATA</small>{:else if item.id === 'diagnostics'}<small>WORKSPACE HEALTH</small>{/if}</button>
{/snippet}
{#snippet barControls(name: BarName)}
  {@const bar = sidebars.bars[name]}
  {@const what = name === 'rail' ? 'module list' : sidebars.arrangement.panel === 'files' ? 'files panel' : 'sessions panel'}
  {#if bar.stage !== 'full'}
    <button type="button" class="bar-grip" aria-label={`Move the ${what}: drag it to the top or bottom edge, or use the up and down arrow keys`} title="Drag to the top or bottom edge to dock it (or use the up and down arrow keys)"
      onpointerdown={event => dockStart(event, name)} onpointermove={dockMove} onpointerup={dockEnd} onpointercancel={() => { dockDrag = null; }} onkeydown={event => dockKey(event, name)}>⠿</button>
    <button type="button" class="bar-step" aria-label={`Expand the ${what}`} title="Expand" onclick={() => sidebars.expand(name)}>+</button>
  {/if}
  {#if bar.stage !== 'flat'}<button type="button" class="bar-step" aria-label={`Collapse the ${what}`} title="Collapse" onclick={() => sidebars.collapse(name)}>–</button>{/if}
  {#if bar.stage !== 'flat' && bar.dock === 'side'}<button type="button" class="bar-step" aria-label="Swap sides" title="Swap sides: the module list and the panel trade places" onclick={() => sidebars.swap()}>⇄</button>{/if}
{/snippet}
<svelte:window onkeydown={event => { shift = event.shiftKey; if (!ghost.registered && native && event.ctrlKey && event.altKey && event.code === 'KeyG') { event.preventDefault(); void ghost.toggle(); } }} onkeyup={event => shift = event.shiftKey} onblur={cancelInteraction} />
<div class="workbench" class:unfocused={!appFocused} data-theme={theme}>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <header class="titlebar" class:autohide={autoHide} class:above={signedOut} class:shown={barShown || barFocused} onpointerenter={barEnter} onpointerleave={barLeave} onfocusin={() => { barFocused = true; }} onfocusout={() => { barFocused = false; barLeave(); }}>
    <button class="title-drag" aria-label="Drag desktop window; double-click to maximize or restore" onmousedown={startWindowDrag}>
      <svg class="brandmark" class:dev={devBuild} viewBox="0 0 256 256" aria-hidden="true"><rect x="3" y="3" width="250" height="250" /><path d={logoPath} /></svg><strong>AGENT WORKBENCH</strong><span class="divider">/</span><span>{native ? 'Desktop workspace' : 'Browser preview'}</span>
    </button>
    {#if devBuild}<span class="badge dev" title="A development build: its own data folder and window, separate from the normal build">DEV BUILD</span>{/if}
    <span class="badge" title={codexReady ? 'Codex is available (read-only, no tools)' : 'Codex is not available: only the local test runner can run'}>{codexReady ? 'CODEX READY' : 'CODEX UNAVAILABLE'}</span>
    {#if native}<div class="window-controls"><button aria-label="Sign out" title="Sign out: show the start screen (terminals and agents keep running)" onclick={() => setSignedOut(true)}><svg viewBox="0 0 20 20" width="16" height="16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="square"><path d="M8 4H4v12h4" /><path d="M8 10h9" /><path d="M13.5 6.5L17 10l-3.5 3.5" /></svg></button><button aria-label="Open properties" title="Properties: terminals, agents and appearance settings" onclick={() => { showProperties = true; }}>⚙</button><button aria-label="Reset window to the default size, centered" title="Reset: back to a normal, smaller, centered window (also when it was only resized to full-screen size)" onclick={() => void resetWindow().catch(() => { if (!disposed) message = 'The window could not be reset.'; })}>↻</button><button aria-label="Minimize window" onclick={() => windowAction('minimize')}>—</button><button aria-label="Maximize or restore window" onclick={() => windowAction('maximize')}>□</button><button class="close-window" aria-label="Close window normally" onclick={() => windowAction('close')}>×</button></div>{/if}
  </header>
  {#if autoHide}<div class="titlebar-hot" aria-hidden="true" onpointerenter={edgeEnter} onpointerleave={edgeLeave}></div>{/if}
  <!-- The chosen background sits behind the whole body (sidebars included); each surface lets it through by its own opacity. -->
  <div class="shell" bind:this={shellEl} style={`grid-template-columns:${grid.columns};grid-template-rows:${grid.rows};--side-width:${sideWidth}px;--canvas-color:${background};--canvas-alpha:${ghostActive ? 0 : mode === 'solid' ? 100 : mode === 'transparent' ? alpha : 0}%;--media-opacity:${alpha / 100};--view-bg:${ghostActive ? 'transparent' : mode === 'solid' ? 'var(--bg)' : mode === 'transparent' ? `color-mix(in srgb, var(--bg) ${alpha}%, transparent)` : 'transparent'};--sidebar1-opacity:${theme === 'contrast' ? 100 : side1}%;--sidebar2-opacity:${theme === 'contrast' ? 100 : side2}%`}>
        {#if mediaUrl && mode === 'image' && !ghostActive}<img class="media" src={mediaUrl} alt="Selected local background" onerror={mediaFailed} />{/if}
        {#if mediaUrl && mode === 'video' && !ghostActive}<video class="media" bind:this={video} src={mediaUrl} muted loop playsinline onerror={mediaFailed}><track kind="captions" /></video>{/if}
    {#if dockDrag}<div class="dock-zones" aria-hidden="true"><div class="dz top" class:on={dockDrag.zone === 'top'}>Dock to the top</div><div class="dz bottom" class:on={dockDrag.zone === 'bottom'}>Dock to the bottom</div></div>{/if}
    <nav class="rail" class:mirrored={sidebars.arrangement.side === 'right'} onpointerdown={event => holdStart(event, 'rail')} onpointermove={holdMove} onpointerup={holdEnd} onpointercancel={holdAbort} class:compact={sidebars.bars.rail.stage === 'compact'} class:flat={sidebars.bars.rail.stage === 'flat'} class:h={grid.rail.horizontal} style:grid-area={grid.rail.area} aria-label="Workbench modules">
      <div class="bar-ctl">{@render barControls('rail')}</div>
      {#if sidebars.bars.rail.stage === 'full'}<p class="eyebrow">WORKSPACE</p>{/if}
      {@render moduleButton(modules[0])}
      <button title="Inspector" aria-label={railName('Inspector')} class:active={showInspector} aria-current={showInspector ? 'page' : undefined} onclick={() => { showInspector = true; showTerminals = false; }}><span class="nav-icon">I</span><span class="nav-label">Inspector</span></button>
      <button title="Terminals" aria-label={railName('Terminals')} class:active={showTerminals} aria-current={showTerminals ? 'page' : undefined} onclick={() => { showTerminals = true; showInspector = false; }}><span class="nav-icon">&gt;_</span><span class="nav-label">Terminals</span></button>
      <!-- Panels: open beside the workspace on the other side; clicking the one showing closes it. -->
      <hr class="rail-sep" />
      <button title="Sessions panel: open or close it beside the workspace" aria-label="Sessions panel" class="panel-btn" class:open={sidebars.arrangement.panel === 'sessions'} aria-pressed={sidebars.arrangement.panel === 'sessions'} onclick={() => sidebars.togglePanel('sessions')}><span class="nav-icon">S</span><span class="nav-label">Sessions</span></button>
      <button title="Files panel: open or close it beside the workspace" aria-label="Files panel" class="panel-btn" class:open={sidebars.arrangement.panel === 'files'} aria-pressed={sidebars.arrangement.panel === 'files'} onclick={() => sidebars.togglePanel('files')}><span class="nav-icon">F</span><span class="nav-label">Files</span></button>
      <hr class="rail-sep" />
      {#each modules.slice(1) as item}{@render moduleButton(item)}{/each}
      <div class="rail-bottom">{#if sidebars.arrangement.panel !== 'sessions'}{@render layoutActions()}{/if}<small>Codex is read-only with no tools. Approvals and automatic handoffs are disabled.</small></div>
    </nav>
    <main style:grid-area={grid.main}>
      {#if !native}<div class="notice">Desktop-only unavailable: this browser preview does not create sessions, runs or native persistence.</div>{:else if workspaceState.status !== 'ready'}<div class="notice">Execution: {workspaceState.status}. Previously loaded history is not evidence of current execution availability.</div>{/if}
      {#if error}<div class="notice error" role="status"><strong>{displayText(error.code)}</strong>: {displayText(error.user_message)} <small>{error.recoverable ? 'Recoverable' : 'Not recoverable'}</small></div>{/if}
      {#if layoutProblem}<div class="notice error" role="status"><strong>{displayText(layoutProblem.code)}</strong>: {displayText(layoutProblem.user_message)} Stored coordinates and intent are preserved; no replacement layout is created.</div>{/if}
      {#if workspaceState.layout.conflicted}<div class="notice conflict"><span>Refresh the authoritative base, then explicitly reapply preserved intent. Reapply does not save.</span><button disabled={!editable || workspaceState.mutation_pending || workspaceState.layout.attempt !== null || !workspaceState.layout.dirty || !draft || !workspaceState.layout.base || workspaceState.layout.base.revision <= draft.revision} onclick={() => act(() => controller.reapplyLayout())}>Reapply intent to refreshed base</button></div>{/if}
      {#if workspaceState.layout.attempt}<div class="notice metadata">Pending layout request {workspaceState.layout.attempt.request.request_id} · expected revision {workspaceState.layout.attempt.request.expected_revision}. Save retries use this exact captured request; later staged edits remain separate.</div>{/if}
      <div class="workspace-content">
      <!-- Above every view (the mode applies on every tab), never catching the mouse. -->
      {#if ghostActive}<div class="ghost-note" aria-hidden="true">SEE-THROUGH · {ghost.hotkey} TO RESTORE</div>{/if}
      {#if showTerminals}<TerminalsView />{/if}
      {#if showInspector}<InspectorView />{/if}
      <!-- Hidden whenever another view is in front (views can be see-through: the canvas must not show through them). -->
      <div class="canvas" class:background-hidden={active !== 'agents' || showTerminals || showInspector} class:grid={snapGrid} inert={active !== 'agents' || showTerminals || showInspector} bind:this={canvas} >
        <div class="canvas-caption">WORKSPACE <span>{snapGrid ? '20 PX SNAP' : 'DRAG + SHIFT: 20 PX SNAP · ARROWS: MOVE / RESIZE'}</span></div>
        {#if draft}
          {#each draft.containers as container, index (container.container_id)}
            <section class="nest" style={rectangle(containerDisplay(container.geometry))} aria-label={`Root container ${index + 1}`}>
              <div class="container-header"><button disabled={!editable} class="drag-handle" aria-label={`Move root container ${index + 1}; arrow keys move, Shift snaps`} onpointerdown={event => begin(event, 'container-move', container.container_id)} onpointermove={move} onpointerup={cancelInteraction} onpointercancel={cancelInteraction} onlostpointercapture={cancelInteraction} onkeydown={event => keyGeometry(event, 'container-move', container.container_id)} onkeyup={() => shift = false}>:: Container {index + 1}</button><span class="badge">ROOT · DEPTH 1 MAX</span></div>
              <small class="container-id">{container.container_id}</small>
              <button class="resize" disabled={!editable} aria-label={`Resize root container ${index + 1}; use arrow keys, Shift snaps`} onpointerdown={event => begin(event, 'container-resize', container.container_id)} onpointermove={move} onpointerup={cancelInteraction} onpointercancel={cancelInteraction} onlostpointercapture={cancelInteraction} onkeydown={event => keyGeometry(event, 'container-resize', container.container_id)} onkeyup={() => shift = false}>↘</button>
            </section>
          {/each}
          {#each draft.panels as panel (panel.panel_id)}
            <SessionPanel {panel} geometry={panelDisplay(panel)} session={sessions.find(session => session.session_id === panel.session_id)} runs={sessionRuns(panel.session_id)} selectedRunId={workspaceState.selected_run_by_session[panel.session_id]} active={workspaceState.selected_session_id === panel.session_id} transcript={workspaceState.selected_session_id === panel.session_id ? workspaceState.transcript : null} progress={workspaceState.progress} refreshing={workspaceState.in_flight} editable={editable} mutationPending={workspaceState.mutation_pending} canCancel={ready && !hasCancel} fill={surfaces[panel.panel_id]?.fill ?? '#172534'} opacity={surfaces[panel.panel_id]?.opacity ?? 96} dragging={dragging && interaction?.id === panel.panel_id} onFocus={() => act(() => controller.selectSession(panel.session_id))} onCycle={() => cycle(panel.panel_id)} onSelectRun={runId => selectRun(panel.session_id, runId)} onCancelRun={cancelRun} onEarlier={() => act(() => controller.loadEarlier())} onLive={() => act(() => controller.returnToLive())} onBegin={(event, kind) => begin(event, kind, panel.panel_id)} onMove={move} onEnd={cancelInteraction} onKey={(event, kind) => keyGeometry(event, kind, panel.panel_id)} onKeyUp={() => shift = false} terminals={canvasTerminals.terminals} link={linkOf(canvasTerminals.state, panel.session_id)} onLink={terminal => canvasTerminals.commit(terminal === null ? unlink(canvasTerminals.state, panel.session_id) : linkTerminal(canvasTerminals.state, panel.session_id, terminal))} onFlip={on => canvasTerminals.commit(flip(canvasTerminals.state, panel.session_id, on))} />
          {/each}
        {:else}<div class="canvas-empty"><h2>{native ? 'No editable authoritative layout' : 'Desktop app required'}</h2><p>{native ? 'Refresh to obtain a valid native layout. Stored warnings are never overwritten with a fallback layout.' : 'Native session storage and fake execution are unavailable in this preview.'}</p></div>{/if}
        {#if active === 'agents' && !showTerminals && !showInspector}<FloatingTerminals />{/if}
        <div class="canvas-footer">{draft?.panels.length ?? 0}/16 PANELS · {draft?.containers.length ?? 0}/8 ROOT CONTAINERS <span>{dragging ? 'EDITING DRAFT ONLY' : 'VIEWPORT CLAMPING DOES NOT SAVE COORDINATES'}</span>{#if draft && draft.panels.length === 0}<small class="canvas-hint">No session panels open. Open a session from the roster; opening a panel does not start a run.</small>{/if}</div>
      </div>
      {#if active === 'tasks'}
        <RunBoard {sessions} {runs} loaded={workspaceState.snapshot !== null} available={ready} canInspect={native && workspaceState.snapshot !== null} refreshing={workspaceState.in_flight} onInspect={inspectRun} />
      {:else if active === 'usage'}
        <UsageOverview {sessions} {runs} loaded={workspaceState.snapshot !== null} available={ready} canInspect={native && workspaceState.snapshot !== null} refreshing={workspaceState.in_flight} onInspect={inspectRun} />
      {:else if active === 'approvals'}
        <EventTimeline {sessions} {runs} selectedRunId={selectedRun?.run_id} window={eventWindow} loaded={workspaceState.snapshot !== null} available={ready} canRead={native && workspaceState.snapshot !== null} refreshing={workspaceState.in_flight} onSelect={selectRun} onEarlier={() => act(() => controller.loadEarlier())} onLive={() => act(() => controller.returnToLive())} />
      {:else if active === 'diagnostics' && diagnosticMetadata}
        <WorkspaceDiagnostics data={diagnosticMetadata} refreshing={workspaceState.in_flight} canRefresh={native && workspaceState.status !== 'stopped'} onRefresh={() => act(() => controller.reconcile())} />
      {/if}
      </div>
    </main>
    {#if grid.splitter}<button style:grid-area={grid.splitter} class="splitter" aria-label="Resize inspector; left arrow widens, right arrow narrows" title="Drag to resize inspector" onpointerdown={event => begin(event, 'side')} onpointermove={move} onpointerup={cancelInteraction} onpointercancel={cancelInteraction} onlostpointercapture={cancelInteraction} onkeydown={event => { if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') { event.preventDefault(); const widen = sidebars.arrangement.side === 'left' ? 'ArrowLeft' : 'ArrowRight'; chrome({ sidebar_width: Math.max(240, Math.min(440, sideWidth + (event.key === widen ? 20 : -20))) }); } }}></button>{/if}
    {#if sidebars.arrangement.panel !== null}
    <aside aria-label={sidebars.arrangement.panel === 'files' ? 'Files panel' : 'Session roster and inspector'} class:mirrored={sidebars.arrangement.side === 'right'} onpointerdown={event => holdStart(event, 'aside')} onpointermove={holdMove} onpointerup={holdEnd} onpointercancel={holdAbort} class:full={sidebars.bars.aside.stage === 'full'} class:compact={sidebars.bars.aside.stage === 'compact'} class:flat={sidebars.bars.aside.stage === 'flat'} class:h={grid.aside.horizontal} style:grid-area={grid.aside.area}>
      {#if sidebars.bars.aside.stage !== 'full'}
        <div class="strip"><div class="bar-ctl">{@render barControls('aside')}</div>{#if sidebars.arrangement.panel === 'files'}<span class="strip-title">Files</span>{:else}<span class="strip-title">Sessions</span><span class="strip-sum" title={`${sessions.length} of ${limits.workspace_sessions} sessions`}>{sessions.length}</span>
          {@render layoutActions()}{/if}</div>
      {:else if sidebars.arrangement.panel === 'files'}
      <div class="aside-scroll">
        <div class="inspector-heading"><h2>Files</h2><span class="bar-ctl">{@render barControls('aside')}</span></div>
        <FilesPanel onShowTerminals={() => { showTerminals = true; showInspector = false; }} />
      </div>
      {:else}
      <div class="aside-scroll">
      <div class="inspector-heading"><h2>Sessions</h2><span class="bar-ctl">{@render barControls('aside')}</span></div>
      <Section id="new" title="New session" summary={sessions.length >= limits.workspace_sessions ? "limit reached" : ""}>
        <form onsubmit={event => { event.preventDefault(); if (ready && validLabel && !hasCreate && !workspaceState.mutation_pending) void createSession(); }}><label>Session label<input type="text" maxlength="256" bind:value={label} autocomplete="off" /></label><label>Agent<select bind:value={agent} onchange={agentChanged} aria-label="Session agent"><option value="fake">Local test runner</option><option value="codex" disabled={!codexReady}>Codex (gpt-6.1-sol){codexReady ? '' : ' — unavailable'}</option></select></label><button class="wide" type="submit" disabled={!ready || !validLabel || hasCreate || workspaceState.mutation_pending || sessions.length >= limits.workspace_sessions}>{agent === 'codex' && codexReady ? 'Create Codex session' : 'Create session'}</button></form>
        <p class="hint codex-status" data-codex={codexAvailability}>Codex: {codexAvailability === 'ready' ? 'ready (pinned CLI verified; read-only, no tools)' : codexAvailability === 'quarantined' ? 'quarantined — cleanup could not be confirmed; execution stopped until restart' : 'not configured (pinned Codex CLI not found or changed)'}</p>
      </Section>
      <Section id="roster" title="Sessions" summary={`${sessions.length} of ${limits.workspace_sessions}`} open={false}>
        <p class="hint">Create and open are separate from sending a run. Sessions and runs cannot be deleted in this slice.</p>
        <ul class="session-roster">{#each sessions as session (session.session_id)}<li><button class:active={session.session_id === workspaceState.selected_session_id} class="wide" onclick={() => openSession(session.session_id)}><strong>{displayText(session.label)}</strong><small>{session.provider_id === 'codex' ? 'CODEX' : 'TEST'} · {sessionRuns(session.session_id).length} runs · {sessionRuns(session.session_id)[0]?.state ?? 'idle'} · open / focus</small></button></li>{/each}</ul>
        {#if sessions.length === 0}<p class="hint">No durable sessions loaded.</p>{/if}
      </Section>
      <Section id="composer" title={codexSession ? 'Prompt to Codex' : 'Prompt'} summary={selectedSession ? displayText(selectedSession.label) : 'select a session'}>
        <p class="selected-label">{selectedSession ? displayText(selectedSession.label) : 'Select a session first'}</p>
        <label>Selected / historical run<select value={selectedRunId ?? ''} disabled={!selectedSession || sessionRuns(selectedSession.session_id).length === 0} onchange={event => { if (selectedSession) selectRun(selectedSession.session_id, event.currentTarget.value); }}><option value="" disabled>No run selected</option>{#each selectedSession ? sessionRuns(selectedSession.session_id) : [] as run (run.run_id)}<option value={run.run_id}>#{run.enqueue_order} · {run.state} · {run.run_id.slice(0, 8)}</option>{/each}</select></label>
        {#if codexSession}
        <form class="codex-composer" onsubmit={event => { event.preventDefault(); if (ready && codexReady && selectedSession && codexPromptValid && codexConfirmed && !hasCodexStart && !workspaceState.mutation_pending) void sendCodex(); }}>
          <label>Codex prompt — sent to the real Codex CLI<textarea bind:value={prompt} maxlength="1024" rows="4" disabled={!selectedSession} autocomplete="off" spellcheck="false"></textarea></label><p class="hint">{promptBytes}/{limits.codex_prompt_bytes} UTF-8 bytes. Read-only sandbox, no tools, no approvals, 45 s limit. Usage is reported by Codex, not a bill.</p>
          <label class="confirm"><input type="checkbox" bind:checked={carryContext} disabled={carriable.length === 0} /> Remember earlier turns of this session ({carriable.length} available)</label>
          <p class="hint">{#if contextRunIds.length > 0}Sends this prompt with the prompts and replies of the last {contextRunIds.length} succeeded {contextRunIds.length === 1 ? 'turn' : 'turns'} of this session, trimmed to {limits.codex_context_bytes} UTF-8 bytes in total (oldest dropped first). Codex keeps no memory itself; Workbench supplies it.{#if inFlight} A turn still in progress is not included until it succeeds.{/if}{:else}Each turn is a fresh Codex conversation; nothing earlier is sent.{/if}</p>
          {#if contextRunIds.length > 0}
            <div class="button-row"><button type="button" disabled={!codexPromptValid} onclick={() => void showContext()}>Show what will be sent</button></div>
            {#if previewError}<p class="hint">{previewError}</p>{/if}
            {#if previewShown}
              <p class="hint">{previewShown.carried} of {contextRunIds.length} earlier {contextRunIds.length === 1 ? 'turn' : 'turns'} included · {previewShown.bytes}/{limits.codex_context_bytes} bytes{previewShown.trimmed ? ' · trimmed to fit' : ''}. This exact text is sent to Codex:</p>
              <pre class="context-preview">{previewShown.text}</pre>
            {/if}
          {/if}
          <label class="confirm"><input type="checkbox" bind:checked={codexConfirmed} disabled={!codexReady} /> I confirm sending this prompt{contextRunIds.length > 0 ? ` and ${contextRunIds.length} earlier ${contextRunIds.length === 1 ? 'turn' : 'turns'} of this session` : ''} to Codex (gpt-6.1-sol). This may use my Codex account.</label>
          <div class="button-row"><button type="submit" disabled={!ready || !codexReady || !selectedSession || !codexPromptValid || !codexConfirmed || hasCodexStart || workspaceState.mutation_pending || runs.length >= limits.workspace_runs}>Send to Codex</button><button type="button" disabled={!ready || !selectedRun || !cancelable || hasCancel || workspaceState.mutation_pending} onclick={() => { if (selectedRun) cancelRun(selectedRun.run_id); }}>Cancel selected run</button></div>
        </form>
        {:else}
        <form onsubmit={event => { event.preventDefault(); if (ready && selectedSession && validPrompt && !hasStart && !workspaceState.mutation_pending) void send(); }}>
          <label>Prompt (local test runner)<textarea bind:value={prompt} maxlength="65536" rows="4" disabled={!selectedSession} autocomplete="off" spellcheck="false"></textarea></label><p class="hint">{promptBytes}/{limits.prompt_bytes} UTF-8 bytes. Retained uncertain inputs are private; retry buttons resend the exact original request.</p>
          <label>Test scenario<select bind:value={scenario} disabled={!selectedSession}><option value="success">Success</option><option value="failure">Startup failure</option><option value="wait_for_cancel">Wait for explicit cancellation</option></select></label>
          <div class="button-row"><button type="submit" disabled={!ready || !selectedSession || !validPrompt || hasStart || workspaceState.mutation_pending || runs.length >= limits.workspace_runs}>Send test run</button><button type="button" disabled={!ready || !selectedRun || !cancelable || hasCancel || workspaceState.mutation_pending} onclick={() => { if (selectedRun) cancelRun(selectedRun.run_id); }}>Cancel selected run</button></div>
        </form>
        {/if}
      </Section>
      <Section id="runs" title="Run details" summary={`${runs.length} of ${limits.workspace_runs} runs`} open={false}>
        <p class="hint">{runs.length}/{limits.workspace_runs} retained runs. No real provider, process, network, approval or automatic handoff action.</p>
        {#if selectedRun}<p class="hint">State: {selectedRun.state}. Input tokens: {selectedRun.usage.input_tokens ?? 'unknown'}; output: {selectedRun.usage.output_tokens ?? 'unknown'}{selectedRun.usage.estimated ? ' (estimated)' : ''}.</p>{/if}
        {#if !selectedPanel && workspaceState.transcript}<div class="standalone-transcript"><button onclick={() => act(() => controller.returnToLive())}>Return to live tail</button><SessionTranscript run={workspaceState.transcript.summary} events={workspaceState.transcript.events} omission={omission(workspaceState.transcript)} refreshing={workspaceState.in_flight} progress={workspaceState.progress} onEarlier={() => act(() => controller.loadEarlier())} /></div>{/if}
      </Section>
      {#if workspaceState.attempts.length > 0}
      <Section id="attempts" title="Unresolved mutation attempts" summary={`${workspaceState.attempts.length} of 8`} attention={true}><p class="hint">These metadata entries do not expose prompts. Nothing here is automatically repeated.</p><ul class="attempt-list">{#each workspaceState.attempts as attempt (attempt.key)}<li><strong>{displayText(attempt.command)}</strong><small>Key: {displayText(attempt.key)}</small><code>{attempt.request_id}</code><button disabled={!native || !workspaceState.snapshot || workspaceState.mutation_pending || attempt.pending} onclick={() => void retry(attempt.key)}>{attempt.pending ? 'Request pending' : 'Retry exact mutation'}</button></li>{/each}</ul></Section>
      {/if}
      <Section id="layout" title="Layout draft" summary={`${plural(draft?.panels.length ?? 0, 'panel')} · ${plural(draft?.containers.length ?? 0, 'container')}`} open={false}>
        <button class="wide" disabled={!editable || (draft?.containers.length ?? 0) >= 8} onclick={addContainer}>Add root container · draft only</button>
        <label>Selected panel nesting<select value={selectedPanel?.container_id ?? ''} disabled={!editable || !selectedPanel} onchange={event => nestSelected(event.currentTarget.value)}><option value="">Canvas root · depth 0</option>{#each draft?.containers ?? [] as container, index (container.container_id)}<option value={container.container_id}>Root container {index + 1} · depth 1</option>{/each}</select></label>
        <p class="hint">Only root containers can parent panels. Self-parenting, panel parents, cross-kind IDs and deeper container nesting are rejected. Nesting is an explicit selector action.</p>
        <button class="wide" disabled={!editable || !selectedPanel} onclick={() => { if (selectedPanel) cycle(selectedPanel.panel_id); }}>Cycle selected panel · {selectedPanel?.stage ?? 'none'}</button>
        <p class="hint">Full → compact → block → restored full geometry. Drag handles or focus them and use arrows; Shift snaps to 20 pixels. Resize handles also support arrows. Moving or nesting preserves stored sizes. Resizing edits stored geometry even when the display is clamped.</p>
        <label class="inline">Transient panel surface<input type="color" value={selectedSurface.fill} disabled={!selectedPanel} oninput={event => changeSurface({ fill: event.currentTarget.value })} /></label>
        <label>Transient surface opacity <output>{selectedSurface.opacity}%</output><input type="range" min="20" max="100" value={selectedSurface.opacity} disabled={!selectedPanel} oninput={event => changeSurface({ opacity: Number(event.currentTarget.value) })} /></label>
        <p class="hint">Opacity affects the surface, never text or controls. Color, opacity and media are not layout fields.</p>
      </Section>
      <p class="hint safety">Codex runs are read-only with no tools and need explicit per-run confirmation. Approvals and automatic handoffs stay disabled.</p>
      </div>
      <div class="aside-foot">{@render layoutActions()}</div>
      {/if}
    </aside>
    {/if}
  </div>
  {#snippet appearanceSettings()}
      <section class="prop-group" aria-label="Native background"><h4>Native background</h4>
        <label>Mode<select value={mode} disabled={!native || choosing || appearancePending} onchange={event => changeBackgroundMode(event.currentTarget.value as BackgroundKind)}><option value="solid">Solid</option><option value="transparent">Desktop transparency</option><option value="image">Local image…</option><option value="video">Local video…</option></select></label>
        <label class="inline">Solid fallback color<input type="color" bind:value={background} onchange={() => void applyAppearance()} /></label>
        <label>Canvas / media alpha <output>{alpha}%</output><input type="range" min="0" max="100" bind:value={alpha} aria-label="Canvas / media alpha" disabled={!native || appearancePending} onchange={() => void applyAppearance()} /></label>
        <button class="wide" disabled={!native || choosing || appearancePending} onclick={() => void choose(mode === 'video' ? 'video' : 'image')}>Choose local {mode === 'video' ? 'video' : 'image'}…</button>
        {#if backgroundNote}<p class="hint" role="status">{backgroundNote}</p>{/if}
        <p class="hint">Native picker and validated backend URL only. Images ≤8 MiB / 16 MP; videos ≤64 MiB. The background shows behind every tab. Video is muted and pauses while hidden or minimized. Unsupported transparency or media uses solid fallback.</p>
        <p class="hint">The mode (solid or desktop transparency), the colour and the alpha are remembered for the next start, and so is the see-through canvas ({ghost.hotkey}) if it was on. A chosen image or video is not remembered yet: pick it again after a restart.</p>
      </section>
      <section class="prop-group" aria-label="Title bar"><h4>Title bar</h4>
        <label class="check"><input type="checkbox" checked={settingsStore.value.window.titlebar_autohide} disabled={settingsStore.saving}
          onchange={event => { const on = event.currentTarget.checked; void settingsStore.update(s => ({ ...s, window: { ...s.window, titlebar_autohide: on } })); }} />
          Hide the title bar until the cursor rests on the top edge</label>
        <p class="hint">On (the default): the title bar slides away and comes back when you rest the cursor at the very top of the window. Off: it always shows.</p>
      </section>
      <section class="prop-group" aria-label="Sidebars"><h4>Sidebars</h4>
        <label>Sidebar 1 opacity <output>{side1}%</output><input type="range" min="0" max="100" bind:value={side1} onchange={saveSidebars} aria-label="Sidebar 1 opacity" /></label>
        <label>Sidebar 2 opacity <output>{side2}%</output><input type="range" min="0" max="100" bind:value={side2} onchange={saveSidebars} aria-label="Sidebar 2 opacity" /></label>
        <p class="hint">Sidebar 1 is the module list on the left, Sidebar 2 the sessions and controls on the right. Lower values let the background behind them show through; 100% is solid. The high-contrast theme always keeps them solid.</p>
      </section>
      <section class="prop-group" aria-label="Workspace chrome"><h4>Workspace chrome</h4><label>Theme<select value={theme} onchange={event => chrome({ theme: event.currentTarget.value as Wire.ThemeName })}><option value="dark">Dark</option><option value="light">Light</option><option value="system">System</option><option value="contrast">High contrast</option></select></label><p class="hint">Desktop theme, module and inspector width changes are staged until Save layout. Browser and unavailable-layout chrome is transient only.</p></section>
  {/snippet}
  <ResumeDialog />
  <KeyHint />
  {#if showProperties}<PropertiesWindow onClose={() => { showProperties = false; }} appearance={appearanceSettings} />{/if}
  {#if signedOut}<StartScreen terminals={canvasTerminals.terminals} onSignIn={() => setSignedOut(false)} />{/if}
  <footer class="statusbar" role="status"><span class="status-dot"></span><span>{message}</span><span class="status-right">I0 V1 · {native ? workspaceState.status.toUpperCase() : 'DESKTOP ONLY'}</span></footer>
</div>
