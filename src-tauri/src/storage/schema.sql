CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    singleton INTEGER NOT NULL UNIQUE CHECK (singleton = 1),
    record_json TEXT NOT NULL,
    layout_json TEXT NOT NULL,
    layout_revision INTEGER NOT NULL CHECK (layout_revision BETWEEN 0 AND 9007199254740991),
    last_enqueue INTEGER NOT NULL CHECK (last_enqueue BETWEEN 0 AND 9007199254740991)
) STRICT;

CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    snapshot_json TEXT NOT NULL,
    UNIQUE (workspace_id, session_id)
) STRICT;

CREATE TABLE runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('queued', 'starting', 'running', 'waiting_input', 'cancel_requested', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    last_seq INTEGER NOT NULL CHECK (last_seq BETWEEN 1 AND 9007199254740991),
    enqueue_order INTEGER NOT NULL CHECK (enqueue_order BETWEEN 1 AND 9007199254740991),
    snapshot_json TEXT NOT NULL,
    prompt TEXT NOT NULL,
    scenario TEXT NOT NULL CHECK (scenario IN ('success', 'failure', 'wait_for_cancel')),
    ordinary_count INTEGER NOT NULL CHECK (ordinary_count BETWEEN 1 AND 20000),
    reserved_count INTEGER NOT NULL CHECK (reserved_count BETWEEN 0 AND 32),
    text_bytes INTEGER NOT NULL CHECK (text_bytes BETWEEN 0 AND 10485760),
    truncation_mask INTEGER NOT NULL CHECK (truncation_mask BETWEEN 0 AND 7),
    cancel_count INTEGER NOT NULL CHECK (cancel_count BETWEEN 0 AND 1),
    terminal_count INTEGER NOT NULL CHECK (terminal_count BETWEEN 0 AND 1),
    CHECK (last_seq = ordinary_count + reserved_count),
    FOREIGN KEY (workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    UNIQUE (workspace_id, enqueue_order),
    UNIQUE (workspace_id, session_id, run_id)
) STRICT;

CREATE UNIQUE INDEX one_active_run ON runs(workspace_id)
WHERE state NOT IN ('queued', 'succeeded', 'failed', 'cancelled', 'interrupted');
CREATE INDEX queued_order ON runs(workspace_id, state, enqueue_order);

CREATE TABLE events (
    event_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL CHECK (seq BETWEEN 1 AND 9007199254740991),
    event_json TEXT NOT NULL,
    FOREIGN KEY (workspace_id, session_id, run_id) REFERENCES runs(workspace_id, session_id, run_id) ON DELETE RESTRICT,
    UNIQUE (run_id, seq)
) STRICT;

CREATE TABLE receipts (
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    command_name TEXT NOT NULL CHECK (command_name IN ('create_session', 'start_fake_run', 'cancel_run', 'save_layout')),
    request_id TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    result_json TEXT NOT NULL,
    emergency INTEGER NOT NULL CHECK (emergency IN (0, 1)),
    CHECK (emergency = 0 OR command_name = 'cancel_run'),
    PRIMARY KEY (workspace_id, command_name, request_id)
) STRICT;
