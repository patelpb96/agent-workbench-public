<script lang="ts">
  import SessionTranscript from './SessionTranscript.svelte';
  import TerminalPane from '../terminals/TerminalPane.svelte';
  import type { TerminalInfo } from '../terminals/client.js';
  import type { Link } from '../terminals/floating.js';
  import { displayText, omission } from '../execution/reconcile.js';
  import { runTransitions } from '../execution-v2/generated.js';
  import type { PanelLayout, RunSnapshot, SessionSnapshot } from '../execution-v2/generated.js';
  import type { RunProgress } from '../execution/client.js';
  import type { TranscriptState } from '../execution/reconcile.js';
  import type { DisplayGeometry } from '../execution/workspace-ports.js';

  interface Props {
    panel: PanelLayout;
    geometry: DisplayGeometry;
    session?: SessionSnapshot;
    runs: readonly RunSnapshot[];
    selectedRunId?: string;
    active: boolean;
    transcript: TranscriptState | null;
    progress?: RunProgress | null;
    refreshing: boolean;
    editable: boolean;
    mutationPending: boolean;
    canCancel: boolean;
    fill: string;
    opacity: number;
    dragging: boolean;
    onFocus: () => void;
    onCycle: () => void;
    onSelectRun: (runId: string) => void;
    onCancelRun: (runId: string) => void;
    onEarlier: () => void;
    onLive: () => void;
    onBegin: (event: PointerEvent, kind: 'move' | 'resize') => void;
    onMove: (event: PointerEvent) => void;
    onEnd: () => void;
    onKey: (event: KeyboardEvent, kind: 'move' | 'resize') => void;
    onKeyUp: () => void;
    /** Terminals that exist, the terminal this session is linked to, and the callbacks to change the link / flip the panel. */
    terminals?: readonly TerminalInfo[];
    link?: Link;
    onLink?: (terminal: string | null) => void;
    onFlip?: (on: boolean) => void;
  }
  let { panel, geometry, session, runs, selectedRunId, active, transcript, progress = null, refreshing, editable, mutationPending, canCancel, fill, opacity, dragging, onFocus, onCycle, onSelectRun, onCancelRun, onEarlier, onLive, onBegin, onMove, onEnd, onKey, onKeyUp, terminals = [], link, onLink, onFlip }: Props = $props();
  const linkedTerminal = $derived(link ? terminals.find(t => t.id === link.terminal) : undefined);
  const showTerminal = $derived(link?.flipped === true && linkedTerminal !== undefined);
  const selected = $derived(runs.find(run => run.run_id === selectedRunId && run.session_id === panel.session_id));
  const selectedTranscript = $derived(active && transcript?.summary.session_id === panel.session_id && transcript.summary.run_id === selectedRunId ? transcript : null);
  const status = $derived(selected?.state ?? 'idle');
  const cancelable = $derived(selected !== undefined && selected.state !== 'cancel_requested' && runTransitions[selected.state].length > 0);
  const title = $derived(session ? displayText(session.label) : 'Unavailable session');
</script>

<section class="agent" class:compact={panel.stage === 'compact'} class:block={panel.stage === 'block'} class:selected={active} class:dragging data-state={status} aria-label={`Session panel: ${title}; ${panel.stage}; ${status}`} style={`left:${geometry.x}px;top:${geometry.y}px;width:${geometry.width}px;height:${geometry.height}px;--panel-fill:${fill};--panel-opacity:${opacity / 100};`}>
  <div class="panel-surface"></div>
  <div class="panel-header">
    <button class="drag-handle" title={`${session?.provider_id === 'codex' ? 'CODEX' : 'TEST'} · ${title} · ${status}`} aria-label={`Focus ${title}; drag to move or use arrow keys; Shift snaps to 20 pixels`} onclick={onFocus} onfocus={onFocus} onpointerdown={event => onBegin(event, 'move')} onpointermove={onMove} onpointerup={onEnd} onpointercancel={onEnd} onlostpointercapture={onEnd} onkeydown={event => onKey(event, 'move')} onkeyup={onKeyUp}>
      <span class="grip" aria-hidden="true">::</span><strong>{panel.stage === 'block' ? 'S' : title}</strong>
    </button>
    {#if panel.stage !== 'block'}<span class="panel-state" title={`Durable selected run state: ${status}`}>{status}</span>{/if}
    <button class="collapse" disabled={!editable} onclick={onCycle} aria-label={panel.stage === 'full' ? 'Collapse panel to compact stage' : panel.stage === 'compact' ? 'Collapse panel to status block stage' : 'Restore panel full expanded geometry'} title="Full → compact → block → full">{panel.stage === 'block' ? '+' : '−'}</button>
  </div>
  {#if panel.stage === 'full'}
    <div class="panel-body">
      <div class="status-line">{#if session?.provider_id === 'codex'}<span class="badge">CODEX · READ-ONLY</span>{:else}<span class="badge simulated">TEST RUNNER</span>{/if}<span class="muted">Depth {panel.container_id === null ? '0' : '1'}/1</span></div>
      {#if onLink}
        <div class="terminal-link">
          <label>Terminal<select value={link?.terminal ?? ''} aria-label={`Terminal linked to ${title}`} onchange={event => onLink(event.currentTarget.value || null)}><option value="">None</option>{#each terminals as terminal (terminal.id)}<option value={terminal.id}>{displayText(terminal.label)} · {terminal.id}</option>{/each}{#if link && !linkedTerminal}<option value={link.terminal}>{link.terminal} (not running)</option>{/if}</select></label>
          {#if link}<button type="button" disabled={!linkedTerminal} onclick={() => onFlip?.(!link.flipped)}>{link.flipped ? 'Show transcript' : 'Show terminal'}</button>{/if}
        </div>
      {/if}
      {#if showTerminal && linkedTerminal}
        <div class="panel-terminal"><TerminalPane session={linkedTerminal} focused={active} onFocus={onFocus} onError={() => undefined} onNotice={() => undefined} /></div>
      {:else}
      <label>Selected / historical run<select value={selectedRunId ?? ''} disabled={runs.length === 0} onchange={event => onSelectRun(event.currentTarget.value)}><option value="" disabled>No runs yet</option>{#each runs as run (run.run_id)}<option value={run.run_id}>#{run.enqueue_order} · {run.state} · {run.run_id.slice(0, 8)}</option>{/each}</select></label>
      {#if active}
        {#if selectedTranscript}
          <div class="transcript-controls"><span class="badge">{selectedTranscript.view === 'history' ? 'HISTORY VIEW' : 'LIVE TAIL'}</span><button onclick={onLive} disabled={selectedTranscript.view === 'live' && selectedTranscript.loaded && !selectedTranscript.problem}>Return to live tail</button><button disabled={!canCancel || mutationPending || !cancelable} onclick={() => { if (selected) onCancelRun(selected.run_id); }}>Cancel this run</button></div>
          {#if selectedTranscript.problem}<p class="notice error" role="status">{displayText(selectedTranscript.problem.code)}: {displayText(selectedTranscript.problem.user_message)}</p>{/if}
          <SessionTranscript run={selectedTranscript.summary} events={selectedTranscript.events} omission={omission(selectedTranscript)} stage={panel.stage} {refreshing} {progress} onEarlier={onEarlier} />
        {:else if selected}<p class="muted">Selected run history is being prepared. Summary sequence is not evidence of loaded transcript events.</p>
        {:else if session?.provider_id === 'codex'}<h2>Ready for an explicit Codex run</h2><p class="muted">Use the inspector composer, confirm, and send. Focusing this panel never starts work.</p>
        {:else}<h2>Ready for a prompt</h2><p class="muted">Use the Prompt section of the sessions panel to send one. Focusing this panel never starts work.</p>{/if}
      {:else}
        <h2>Summary only</h2><p class="muted">{runs.length} retained runs. Selected state: {status}. This inactive panel retains no transcript event buffer.</p>
        {#if selected}<p class="muted">{selected.provider_id === 'codex' ? 'Reported' : 'Simulated'} usage: input {selected.usage.input_tokens ?? 'unknown'}, output {selected.usage.output_tokens ?? 'unknown'}{selected.usage.estimated ? ' (estimated)' : ''}.</p>{/if}
        <button onclick={onFocus}>Focus selected run transcript</button>
      {/if}
      {/if}
    </div>
    <button class="resize" disabled={!editable} aria-label={`Resize ${title}; use arrow keys; Shift snaps to 20 pixels`} onpointerdown={event => onBegin(event, 'resize')} onpointermove={onMove} onpointerup={onEnd} onpointercancel={onEnd} onlostpointercapture={onEnd} onkeydown={event => onKey(event, 'resize')} onkeyup={onKeyUp}>↘</button>
  {/if}
</section>
