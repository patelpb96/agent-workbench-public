-- Copy-rehearsal migration only. Caller disables foreign_keys before BEGIN,
-- checks all legacy contents first, and checks FK integrity after COMMIT.
CREATE TABLE runs_v2 (
    run_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'starting', 'running', 'waiting_input', 'cancel_requested', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    last_seq INTEGER NOT NULL CHECK (last_seq BETWEEN 1 AND 9007199254740991),
    enqueue_order INTEGER NOT NULL CHECK (enqueue_order BETWEEN 1 AND 9007199254740991),
    snapshot_json TEXT NOT NULL,
    prompt TEXT NOT NULL,
    scenario TEXT,
    ordinary_count INTEGER NOT NULL CHECK (ordinary_count BETWEEN 1 AND 20000),
    reserved_count INTEGER NOT NULL CHECK (reserved_count BETWEEN 0 AND 32),
    text_bytes INTEGER NOT NULL CHECK (text_bytes BETWEEN 0 AND 10485760),
    truncation_mask INTEGER NOT NULL CHECK (truncation_mask BETWEEN 0 AND 7),
    cancel_count INTEGER NOT NULL CHECK (cancel_count BETWEEN 0 AND 1),
    terminal_count INTEGER NOT NULL CHECK (terminal_count BETWEEN 0 AND 1),
    provider_id TEXT NOT NULL CHECK (provider_id IN ('fake', 'codex')),
    profile_id TEXT,
    CHECK ((provider_id = 'fake' AND scenario IS NOT NULL AND scenario IN ('success', 'failure', 'wait_for_cancel') AND profile_id IS NULL AND length(CAST(prompt AS BLOB)) BETWEEN 1 AND 65536)
        OR (provider_id = 'codex' AND scenario IS NULL AND profile_id IS NOT NULL AND profile_id = 'codex_text_v1' AND length(CAST(prompt AS BLOB)) BETWEEN 1 AND 1024)),
    CHECK (last_seq = ordinary_count + reserved_count),
    FOREIGN KEY (workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    UNIQUE (workspace_id, enqueue_order),
    UNIQUE (workspace_id, session_id, run_id)
) STRICT;
INSERT INTO runs_v2 SELECT run_id, workspace_id, session_id, state, last_seq, enqueue_order,
    snapshot_json, prompt, scenario, ordinary_count, reserved_count, text_bytes,
    truncation_mask, cancel_count, terminal_count, 'fake', NULL FROM runs;
DROP TABLE runs;
ALTER TABLE runs_v2 RENAME TO runs;
CREATE UNIQUE INDEX one_active_run ON runs(workspace_id)
WHERE state NOT IN ('queued', 'succeeded', 'failed', 'cancelled', 'interrupted');
CREATE INDEX queued_order ON runs(workspace_id, state, enqueue_order);
CREATE TABLE receipts_v2 (
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    command_name TEXT NOT NULL CHECK (command_name IN ('create_session', 'start_fake_run', 'cancel_run', 'save_layout', 'create_session_v2', 'start_fake_run_v2', 'start_codex_run_v2', 'cancel_run_v2', 'save_layout_v2')),
    request_id TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    result_json TEXT NOT NULL,
    emergency INTEGER NOT NULL CHECK (emergency IN (0, 1)),
    CHECK (emergency = 0 OR command_name IN ('cancel_run', 'cancel_run_v2')),
    PRIMARY KEY (workspace_id, command_name, request_id)
) STRICT;
INSERT INTO receipts_v2 SELECT * FROM receipts;
DROP TABLE receipts;
ALTER TABLE receipts_v2 RENAME TO receipts;
PRAGMA user_version = 2;
