<script lang="ts">
  import type { PanelStage, RunEvent, RunSnapshot } from '../execution-v2/generated.js';
  import type { Omission } from '../execution/reconcile.js';
  import type { RunProgress } from '../execution/client.js';
  import { boundedEvents, displayText, eventText } from '../execution/reconcile.js';

  interface Props {
    run: RunSnapshot;
    events: readonly RunEvent[];
    omission: Omission;
    stage?: PanelStage;
    refreshing?: boolean;
    progress?: RunProgress | null;
    onEarlier?: () => void;
  }

  let { run, events, omission, stage = 'full', refreshing = false, progress = null, onEarlier }: Props = $props();
  const stepLabels: Record<string, string> = {
    thread_started: 'Session opened',
    turn_started: 'Turn started',
    reasoning_observed: 'Reasoning',
    message_complete: 'Reply received'
  };
  const liveSteps = $derived(progress !== null && progress.run_id === run.run_id ? progress.steps : []);
  const visible = $derived(boundedEvents(events.filter(event =>
    event.workspace_id === run.workspace_id && event.session_id === run.session_id &&
    event.run_id === run.run_id && event.provider_id === run.provider_id)));
</script>

<!-- Routine background reads must not insert/remove rows above the events. -->
<section class="transcript" aria-label={run.provider_id === 'codex' ? 'Codex run transcript' : 'Simulated run transcript'} aria-busy={refreshing}>
  {#if run.provider_id === 'fake'}
    <header>
      <strong>SIMULATED · local fake provider</strong>
      <p>Durable summary state: {run.state} · Panel stage: {stage}</p>
      <p>
        Simulated usage: input {run.usage.input_tokens ?? 'unknown'},
        output {run.usage.output_tokens ?? 'unknown'}{run.usage.estimated ? ' (estimated)' : ''}.
        No paid provider execution.
      </p>
      {#if run.terminal_error !== null}
        <p role="status">SIMULATED error · {displayText(run.terminal_error.code)}: {displayText(run.terminal_error.user_message)}</p>
      {/if}
    </header>
    {#if omission.message !== null}
      <p class="omission" role="status">{omission.message}</p>
    {/if}
    {#if omission.before !== null && onEarlier !== undefined}
      <button type="button" onclick={onEarlier}>Load earlier history</button>
    {/if}
    {#if visible.length < events.length}
      <p class="omission">Additional content omitted from this bounded view. Use history paging to inspect earlier events.</p>
    {/if}
    <ol aria-label="Simulated events">
      {#each visible as event (event.event_id)}
        <li value={event.seq}>
          <span class="sequence">#{event.seq}</span>
          <pre>{eventText(event)}</pre>
        </li>
      {/each}
    </ol>
    {#if visible.length === 0}<p>No transcript events loaded in this view.</p>{/if}
  {:else if run.provider_id === 'codex'}
    <header>
      <strong>CODEX · {run.profile_id} · gpt-6.1-sol</strong>{#if run.profile_id === 'codex_text_v2'}<p>Sent with earlier turns of this session.</p>{/if}
      <p>Durable summary state: {run.state} · Panel stage: {stage}</p>
      <p>
        Reported usage: input {run.usage.input_tokens ?? 'unknown'}, output {run.usage.output_tokens ?? 'unknown'},
        cached {run.usage.cached_input_tokens ?? 'unknown'}, reasoning {run.usage.reasoning_output_tokens ?? 'unknown'}.
        Reported by Codex; not a bill.
      </p>
      {#if run.terminal_error !== null}
        <p role="status">Codex error · {displayText(run.terminal_error.code)}: {displayText(run.terminal_error.user_message)}</p>
      {/if}
    </header>
    {#if omission.message !== null}
      <p class="omission" role="status">{omission.message}</p>
    {/if}
    {#if omission.before !== null && onEarlier !== undefined}
      <button type="button" onclick={onEarlier}>Load earlier history</button>
    {/if}
    {#if visible.length < events.length}
      <p class="omission">Additional content omitted from this bounded view. Use history paging to inspect earlier events.</p>
    {/if}
    {#if liveSteps.length > 0}
      <p class="progress" role="status" aria-label="Live Codex progress">
        Live (not saved): {liveSteps.map(s => (stepLabels[s.step] ?? s.step) + (s.count > 1 ? ` ×${s.count}` : '')).join(' → ')}
      </p>
    {/if}
    <ol aria-label="Codex events">
      {#each visible as event (event.event_id)}
        <li value={event.seq} class:reply={event.kind === 'text_delta'}>
          <span class="sequence">#{event.seq}</span>
          <pre>{eventText(event)}</pre>
        </li>
      {/each}
    </ol>
    {#if visible.length === 0}<p>No transcript events loaded in this view.</p>{/if}
  {:else}
    <p>This provider is not supported by this workbench.</p>
  {/if}
</section>

<style>
  .transcript { min-width: 0; color: inherit; }
  header strong { display: block; font-weight: 700; }
  p { margin: 0.5rem 0; }
  ol { list-style: none; padding: 0; margin: 0.75rem 0; }
  li { margin: 0.5rem 0; }
  pre { font: inherit; white-space: pre-wrap; overflow-wrap: anywhere; margin: 0.25rem 0; }
  .sequence { font-size: 0.875rem; }
  .reply pre { font-size: 1rem; }
  .progress { font-size: 0.875rem; opacity: 0.85; }
  .omission { border-inline-start: 0.2rem solid currentColor; padding-inline-start: 0.75rem; }
  button { font: inherit; color: inherit; cursor: pointer; }
</style>
