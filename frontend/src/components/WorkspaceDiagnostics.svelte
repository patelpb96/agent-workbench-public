<script lang="ts">
  import { limits } from '../execution-v2/generated.js';
  import type { DiagnosticsMetadata } from './diagnostics-metadata.js';
  interface Props {
    data: DiagnosticsMetadata;
    refreshing: boolean;
    canRefresh: boolean;
    onRefresh: () => void;
  }
  let { data, refreshing, canRefresh, onRefresh }: Props = $props();
  const shown = (value: number | string | null) => value ?? 'unavailable';
  const flag = (value: boolean) => value ? 'yes' : 'no';
</script>

<section class="workspace-diagnostics" aria-label="Workspace diagnostic metadata" aria-busy={refreshing}>
  <header>
    <span class="badge simulated">READ-ONLY METADATA</span>
    <h2>Workspace health &amp; diagnostics</h2>
    <p>Loaded UI/native summary metadata, not a raw log viewer or a process/performance health certificate. No prompts, paths, event payloads, private retry keys or request bodies are displayed.</p>
    <p class="authority">{!data.loaded ? 'No native workspace snapshot loaded.' : data.status === 'ready' ? 'Loaded native summary · availability flag shown separately below.' : 'Last loaded metadata only · current execution availability is unverified.'}</p>
    <button disabled={!canRefresh} onclick={onRefresh}>Refresh workspace summaries</button>
    <p class="quiet">Read reconciliation runs in the background. Refresh never repeats a mutation; routine read activity is exposed through aria-busy, not flashing status rows.</p>
  </header>
  <div class="diagnostic-grid">
    <section aria-label="Connection metadata"><h3>Connection</h3><dl>
      <dt>Frontend status</dt><dd data-metric="status">{data.status}</dd>
      <dt>Execution availability</dt><dd data-metric="execution">{data.execution}</dd>
      <dt>Listener attached</dt><dd>{flag(data.listener_ready)}</dd><dt>UI marked visible</dt><dd>{flag(data.visible)}</dd>
      <dt>Mutation request in flight</dt><dd>{flag(data.mutation_pending)}</dd>
      <dt>Last error code</dt><dd data-metric="last_error_code">{data.last_error_code ?? 'none'}</dd>
      <dt>Workspace ID</dt><dd><code data-metric="workspace_id">{data.workspace_id ?? 'unavailable'}</code></dd>
      <dt>Shell OS label</dt><dd>{data.shell?.os ?? 'unavailable'}</dd><dt>Shell app version</dt><dd>{data.shell?.app_version ?? 'unavailable'}</dd>
    </dl></section>
    <section aria-label="Loaded workspace counts"><h3>Loaded workspace counts</h3><dl>
      <dt>Sessions</dt><dd><span data-metric="sessions">{shown(data.sessions)}</span> / {limits.workspace_sessions}</dd>
      <dt>Retained runs</dt><dd><span data-metric="runs">{shown(data.runs)}</span> / {limits.workspace_runs}</dd>
      <dt>Queued</dt><dd data-metric="queued">{shown(data.queued)}</dd>
      <dt>Active / cancellation requested</dt><dd data-metric="active">{shown(data.active)}</dd>
      <dt>Terminal</dt><dd data-metric="terminal">{shown(data.terminal)}</dd>
    </dl><p>Counts derive from the loaded native run states, not CPU activity or inferred success.</p></section>
    <section aria-label="Layout intent metadata"><h3>Layout intent</h3><dl>
      <dt>Last loaded base revision</dt><dd data-metric="saved_revision">{shown(data.layout.saved_revision)}</dd>
      <dt>Draft revision</dt><dd>{shown(data.layout.draft_revision)}</dd>
      <dt>Unsaved draft</dt><dd data-metric="layout_dirty">{flag(data.layout.dirty)}</dd><dt>Conflict</dt><dd>{flag(data.layout.conflicted)}</dd>
      <dt>Draft panels</dt><dd data-metric="draft_panels">{shown(data.layout.draft_panels)}</dd><dt>Draft root containers</dt><dd data-metric="draft_containers">{shown(data.layout.draft_containers)}</dd>
      <dt>Warning code</dt><dd>{data.layout.warning_code ?? 'none'}</dd><dt>Problem code</dt><dd>{data.layout.problem_code ?? 'none'}</dd>
    </dl>
      {#if data.layout.pending_save}<p>Unresolved layout save <code>{data.layout.pending_save.request_id}</code> · expected revision {data.layout.pending_save.expected_revision}.</p>{/if}
      <p>Warnings preserve stored layout and intent. Only explicit Save persists edits; this view never repairs or replaces layout.</p>
    </section>
    <section aria-label="History window metadata"><h3>Selected history window</h3>
      {#if data.history}<dl>
        <dt>Run ID</dt><dd><code data-metric="history_run_id">{data.history.run_id}</code></dd>
        <dt>History loaded</dt><dd>{flag(data.history.loaded)}</dd><dt>View</dt><dd>{data.history.view}</dd>
        <dt>Loaded first sequence</dt><dd>{shown(data.history.first_seq)}</dd><dt>Loaded last sequence</dt><dd>{shown(data.history.last_seq)}</dd>
        <dt>Retained event rows</dt><dd><span data-metric="history_rows">{data.history.count}</span> / {limits.visible_events}</dd>
        <dt>Native high-water</dt><dd>{data.history.known_last_seq}</dd><dt>Summary sequence</dt><dd>{data.history.summary_seq}</dd>
        <dt>History problem code</dt><dd>{data.history.problem_code ?? 'none'}</dd>
      </dl>{:else}<p>No selected run history window.</p>{/if}
      <p>High-water and summary sequence are not loading cursors. One existing bounded transcript buffer; no additional raw log cache.</p>
    </section>
  </div>
  <section class="attempts" aria-label="Unresolved request metadata">
    <h3>Unresolved mutation attempts · <span data-metric="attempts">{data.attempts.length}</span> / 8</h3>
    {#if data.attempts.length === 0}<p>No retained unresolved mutation attempts.</p>{/if}
    <ul>{#each data.attempts as attempt}<li><strong>{attempt.command}</strong><code>{attempt.request_id}</code><span>{attempt.pending ? 'Request pending' : 'Reply unresolved'}</span></li>{/each}</ul>
    <p>Metadata only. Any exact retry is an explicit action in the inspector, never a diagnostics or Refresh action.</p>
  </section>
  <footer><h3>Not measured or enabled</h3><p>Worker mailbox depth, CPU/GPU/memory, real process liveness, raw logs, database paths, provider quotas and billing are not exposed here. Real providers, genuine approvals, automatic handoffs and automatic retry remain disabled.</p></footer>
</section>

<style>
  .workspace-diagnostics{position:absolute;inset:0;z-index:10;overflow:auto;background:var(--view-bg,var(--bg));color:var(--text);padding:24px}
  header{max-width:900px}h2{margin:14px 0 8px}p{color:var(--muted);line-height:1.7;font-size:11px}.authority{border-inline-start:2px solid var(--line);padding-inline-start:10px}button{font-size:11px}
  .diagnostic-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,320px),1fr));gap:12px;margin-top:20px}
  .diagnostic-grid>section{padding:16px;border:1px solid var(--line);background:var(--surface);min-width:0}h3{font-size:13px;font-weight:500}
  dl{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:10px;font-size:11px}dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere}code{font-size:10px;overflow-wrap:anywhere}
  .attempts,footer{border-top:1px solid var(--line);padding-top:12px;margin-top:24px}.attempts ul{list-style:none;padding:0}.attempts li{display:flex;flex-wrap:wrap;gap:10px;border:1px solid var(--line);padding:12px;margin:8px 0;font-size:11px}.attempts strong{font-weight:500}.attempts span{color:var(--muted)}
</style>
