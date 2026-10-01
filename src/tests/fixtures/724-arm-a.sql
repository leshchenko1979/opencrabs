-- #724 ARM A — the FATAL shape (live client DB, uv=45).
--
-- COPIED VERBATIM from the value-blind fixture that measured this defect:
--   <home>/projects/miidas/files/724-migration-replay-fixture.sql
--   (ARM A section; whole-file md5 74e167b95eb44bca8dc2945cb61b398b)
-- Schema objects and the user_version stamp only — zero INSERTs, zero rows, no
-- client data. It is committed so the parity test runs on the MEASURED shape
-- rather than on a reconstruction: rebuilding the shape from the current
-- migration list cannot reproduce which migrations the v0.5.3 list had already
-- applied, and that difference is the entire point of the arm.
-- ======================================================================
-- ARM A — the FATAL shape (live client DB, uv=45)
-- Boots to: duplicate column name: repo_remote. exit=1.
-- objects: 26 tables, 31 indexes
-- ======================================================================
PRAGMA user_version = 45;

-- [table] a2a_context_sessions
CREATE TABLE a2a_context_sessions (
    context_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL               -- Unix timestamp
);

-- [table] a2a_tasks
CREATE TABLE a2a_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    context_id TEXT,
    state TEXT NOT NULL DEFAULT 'submitted',  -- submitted, working, completed, failed, canceled
    data TEXT NOT NULL,                        -- Full Task JSON blob
    created_at INTEGER NOT NULL,               -- Unix timestamp
    updated_at INTEGER NOT NULL                -- Unix timestamp
);

-- [table] attachments
CREATE TABLE attachments (
    id TEXT PRIMARY KEY NOT NULL,
    message_id TEXT NOT NULL,
    type TEXT NOT NULL,  -- 'image', 'file', 'text'
    mime_type TEXT,
    path TEXT,
    size_bytes INTEGER,
    created_at INTEGER NOT NULL,

    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

-- [table] background_tasks
CREATE TABLE background_tasks (
    id         TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    label      TEXT NOT NULL,
    command    TEXT NOT NULL,
    cwd        TEXT NOT NULL,
    started_at INTEGER NOT NULL
);

-- [table] brain_verify_events
CREATE TABLE brain_verify_events (
    id TEXT PRIMARY KEY,
    file_name TEXT NOT NULL,
    event_type TEXT NOT NULL DEFAULT 'pass',
    violations TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

-- [table] channel_messages
CREATE TABLE channel_messages (
    id TEXT PRIMARY KEY,
    channel TEXT NOT NULL,
    channel_chat_id TEXT NOT NULL,
    channel_chat_name TEXT,
    sender_id TEXT NOT NULL,
    sender_name TEXT NOT NULL,
    content TEXT NOT NULL,
    message_type TEXT NOT NULL DEFAULT 'text',
    platform_message_id TEXT,
    created_at INTEGER NOT NULL
, thread_id TEXT, topic_name TEXT);

-- [table] cron_job_runs
CREATE TABLE cron_job_runs (
    id              TEXT PRIMARY KEY NOT NULL,
    job_id          TEXT NOT NULL REFERENCES cron_jobs(id) ON DELETE CASCADE,
    job_name        TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'running',  -- running, success, error
    content         TEXT,
    error           TEXT,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cost            REAL NOT NULL DEFAULT 0.0,
    provider        TEXT,
    model           TEXT,
    started_at      TEXT NOT NULL,
    completed_at    TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- [table] cron_jobs
CREATE TABLE cron_jobs (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    cron_expr   TEXT NOT NULL,           -- standard cron expression (e.g. "0 9 * * *")
    timezone    TEXT NOT NULL DEFAULT 'UTC',
    prompt      TEXT NOT NULL,           -- the message/instruction to execute
    provider    TEXT,                    -- override provider (NULL = use default)
    model       TEXT,                    -- override model (NULL = use default)
    thinking    TEXT NOT NULL DEFAULT 'off', -- 'off', 'on', 'budget'
    auto_approve INTEGER NOT NULL DEFAULT 1, -- auto-approve tool calls
    deliver_to  TEXT,                    -- channel to deliver results (e.g. "telegram:123456")
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run_at TEXT,                    -- ISO 8601 timestamp of last execution
    next_run_at TEXT,                    -- ISO 8601 timestamp of next scheduled run
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
, deliver_api_key TEXT, profile_name TEXT, trigger_cmd TEXT, trigger_on TEXT DEFAULT 'non_empty', set_goal INTEGER NOT NULL DEFAULT 0, goal_template TEXT);

-- [table] feedback_ledger
CREATE TABLE feedback_ledger (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT    NOT NULL,
    event_type      TEXT    NOT NULL,   -- 'tool_success', 'tool_failure', 'user_correction', 'provider_error', 'context_compaction', 'improvement_applied'
    dimension       TEXT    NOT NULL,   -- what was observed: tool name, provider name, etc.
    value           REAL    NOT NULL DEFAULT 1.0,  -- numeric signal (1.0 = success, 0.0 = failure, duration_ms, etc.)
    metadata        TEXT,               -- JSON blob with extra context
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- [table] files
CREATE TABLE "files" (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    path TEXT NOT NULL,
    content TEXT,  -- Optional file content
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL, size INTEGER,  -- New field

    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

-- [table] goal_state
CREATE TABLE goal_state (
    id         TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    goal_text  TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'paused', 'completed', 'failed')),
    turns_used                   INTEGER NOT NULL DEFAULT 0,
    max_turns                    INTEGER NOT NULL DEFAULT 20,
    consecutive_parse_failures   INTEGER NOT NULL DEFAULT 0,
    judge_verdict                TEXT,
    judge_reason                 TEXT,
    channel                      TEXT,
    channel_chat_id              TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- [table] messages
CREATE TABLE "messages" (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    sequence INTEGER NOT NULL,  -- New field for message ordering
    created_at INTEGER NOT NULL,
    token_count INTEGER,  -- Simplified from separate input/output tokens
    cost REAL, input_tokens INTEGER, thinking TEXT, cache_creation_tokens INTEGER, cache_read_tokens INTEGER, duration_secs INTEGER,

    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

-- [table] notify_queue
CREATE TABLE notify_queue (
    id           TEXT PRIMARY KEY NOT NULL,
    session_id   TEXT NOT NULL,
    context_text TEXT NOT NULL,
    display_text TEXT NOT NULL,
    origin       TEXT NOT NULL,
    bg_meta      TEXT,
    created_at   INTEGER NOT NULL
);

-- [table] pending_followups
CREATE TABLE pending_followups (
    token           TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    options_json    TEXT NOT NULL,
    host_message_id INTEGER,
    host_html       TEXT,
    host_rich       INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
, host_markdown TEXT);

-- [table] pending_requests
CREATE TABLE pending_requests (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    user_message TEXT NOT NULL,
    channel      TEXT NOT NULL DEFAULT 'tui',
    status       TEXT NOT NULL DEFAULT 'PROCESSING',
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch())
, channel_chat_id TEXT, origin TEXT NOT NULL DEFAULT 'user', channel_thread_id TEXT);

-- [table] phantom_events
CREATE TABLE phantom_events (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL DEFAULT '',
    provider TEXT,
    model TEXT,
    detected_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    resolved INTEGER NOT NULL DEFAULT 0,
    retry_count INTEGER NOT NULL DEFAULT 0,
    tools_after_retry INTEGER NOT NULL DEFAULT 0
);

-- [table] plan_cards
CREATE TABLE plan_cards (
    session_id TEXT PRIMARY KEY NOT NULL,
    chat_id    INTEGER NOT NULL,
    thread_id  INTEGER,
    message_id INTEGER NOT NULL,
    signature  TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- [table] projects
CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
, repo_remote TEXT);

-- [table] recent_paths
CREATE TABLE recent_paths (
    working_directory TEXT NOT NULL,
    path              TEXT NOT NULL,
    last_accessed     INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (working_directory, path)
);

-- [table] session_bindings
CREATE TABLE session_bindings (
    session_id TEXT PRIMARY KEY,
    channel TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    thread_id INTEGER,
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

-- [table] session_seen_skills
CREATE TABLE session_seen_skills (
    session_id TEXT NOT NULL,
    slug       TEXT NOT NULL,
    seen_at    INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    epoch      INTEGER NULL,
    -- No separate index on session_id: the primary key's implicit index is
    -- already leftmost-prefixed on it, so every per-session lookup and the
    -- orphan prune both ride that one. A second index would serve no read
    -- and cost a write on every mark_seen.
    PRIMARY KEY (session_id, slug)
);

-- [table] sessions
CREATE TABLE "sessions" (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT,  -- Made optional
    model TEXT,  -- Made optional
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER,  -- Replaced is_archived with timestamp
    token_count INTEGER NOT NULL DEFAULT 0,  -- Renamed from total_tokens
    total_cost REAL NOT NULL DEFAULT 0.0
, provider_name TEXT, working_directory TEXT, category TEXT, auto_title_attempted INTEGER NOT NULL DEFAULT 0, project_id TEXT REFERENCES projects(id) ON DELETE SET NULL);

-- [table] streaming_recoveries
CREATE TABLE streaming_recoveries (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL DEFAULT '',
    provider TEXT,
    model TEXT,
    recovered_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    tool_count INTEGER NOT NULL DEFAULT 1
);

-- [table] tool_executions
CREATE TABLE tool_executions (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    session_id TEXT NOT NULL DEFAULT '',
    tool_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
, provider TEXT, model TEXT, duration_ms INTEGER);

-- [table] usage_ledger
CREATE TABLE usage_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,        -- which session incurred this usage (informational, not FK)
    model TEXT NOT NULL DEFAULT '',   -- model used
    token_count INTEGER NOT NULL DEFAULT 0,
    cost REAL NOT NULL DEFAULT 0.0,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
, provider TEXT NOT NULL DEFAULT '');

-- [table] whatsapp_newsletter_cursors
CREATE TABLE whatsapp_newsletter_cursors (
    wa_jid         TEXT PRIMARY KEY,
    last_server_id INTEGER NOT NULL DEFAULT 0,
    last_ts        INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

-- [index] idx_a2a_tasks_state
CREATE INDEX idx_a2a_tasks_state ON a2a_tasks(state);

-- [index] idx_a2a_tasks_updated
CREATE INDEX idx_a2a_tasks_updated ON a2a_tasks(updated_at DESC);

-- [index] idx_attachments_message_id
CREATE INDEX idx_attachments_message_id ON attachments(message_id);

-- [index] idx_background_tasks_session
CREATE INDEX idx_background_tasks_session ON background_tasks(session_id);

-- [index] idx_brain_verify_events_at
CREATE INDEX idx_brain_verify_events_at ON brain_verify_events(created_at);

-- [index] idx_channel_messages_chat
CREATE INDEX idx_channel_messages_chat ON channel_messages(channel, channel_chat_id);

-- [index] idx_channel_messages_thread
CREATE INDEX idx_channel_messages_thread ON channel_messages(channel, channel_chat_id, thread_id);

-- [index] idx_channel_messages_time
CREATE INDEX idx_channel_messages_time ON channel_messages(created_at);

-- [index] idx_cron_job_runs_job_id
CREATE INDEX idx_cron_job_runs_job_id ON cron_job_runs(job_id);

-- [index] idx_cron_job_runs_started_at
CREATE INDEX idx_cron_job_runs_started_at ON cron_job_runs(started_at);

-- [index] idx_feedback_ledger_created
CREATE INDEX idx_feedback_ledger_created    ON feedback_ledger(created_at DESC);

-- [index] idx_feedback_ledger_dimension
CREATE INDEX idx_feedback_ledger_dimension  ON feedback_ledger(dimension);

-- [index] idx_feedback_ledger_event_type
CREATE INDEX idx_feedback_ledger_event_type ON feedback_ledger(event_type);

-- [index] idx_feedback_ledger_session
CREATE INDEX idx_feedback_ledger_session    ON feedback_ledger(session_id);

-- [index] idx_files_path
CREATE INDEX idx_files_path ON files(path);

-- [index] idx_files_session_id
CREATE INDEX idx_files_session_id ON files(session_id);

-- [index] idx_goal_state_active
CREATE INDEX idx_goal_state_active ON goal_state(session_id, state);

-- [index] idx_goal_state_session
CREATE INDEX idx_goal_state_session ON goal_state(session_id);

-- [index] idx_messages_session_id
CREATE INDEX idx_messages_session_id ON messages(session_id, sequence ASC);

-- [index] idx_notify_queue_session
CREATE INDEX idx_notify_queue_session ON notify_queue(session_id);

-- [index] idx_pending_followups_session
CREATE INDEX idx_pending_followups_session
    ON pending_followups(session_id);

-- [index] idx_phantom_events_detected_at
CREATE INDEX idx_phantom_events_detected_at ON phantom_events(detected_at);

-- [index] idx_phantom_events_session
CREATE INDEX idx_phantom_events_session ON phantom_events(session_id);

-- [index] idx_projects_name
CREATE INDEX idx_projects_name ON projects(name);

-- [index] idx_projects_repo_remote
CREATE INDEX idx_projects_repo_remote ON projects(repo_remote);

-- [index] idx_recent_paths_wd_time
CREATE INDEX idx_recent_paths_wd_time
    ON recent_paths (working_directory, last_accessed DESC);

-- [index] idx_session_bindings_channel
CREATE INDEX idx_session_bindings_channel
    ON session_bindings(channel, updated_at);

-- [index] idx_sessions_archived
CREATE INDEX idx_sessions_archived ON sessions(archived_at DESC);

-- [index] idx_sessions_project_id
CREATE INDEX idx_sessions_project_id ON sessions(project_id);

-- [index] idx_sessions_updated_at
CREATE INDEX idx_sessions_updated_at ON sessions(updated_at DESC);

-- [index] idx_streaming_recoveries_at
CREATE INDEX idx_streaming_recoveries_at ON streaming_recoveries(recovered_at);

