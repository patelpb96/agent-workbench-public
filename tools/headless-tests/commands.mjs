// One test-transport allowlist shared by Node and the isolated browser facade.
// Production DTOs/validation/dispatch remain authoritative in the Rust coordinator.
export const executionCommands = Object.freeze([
  'bootstrap_workspace_v2', 'create_session_v2', 'start_fake_run_v2', 'start_codex_run_v2',
  'cancel_run_v2', 'get_workspace_snapshot_v2', 'list_run_events_v2', 'get_run_progress_v2', 'save_layout_v2'
]);
