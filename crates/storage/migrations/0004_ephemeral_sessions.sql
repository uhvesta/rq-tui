CREATE TABLE IF NOT EXISTS ephemeral_sessions (
  operation_id TEXT PRIMARY KEY,
  work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
  owner_id     TEXT NOT NULL,
  parent_id    TEXT,
  side_id      TEXT UNIQUE,
  state        TEXT NOT NULL CHECK (
    state IN ('intent', 'opening', 'active', 'cleanup_pending', 'deleting')
  ),
  last_error   TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS ephemeral_sessions_work_item_created
  ON ephemeral_sessions(work_item_id, created_at, operation_id);

CREATE TABLE IF NOT EXISTS ephemeral_session_leases (
  work_item_id TEXT PRIMARY KEY REFERENCES work_items(id) ON DELETE CASCADE,
  owner_id     TEXT NOT NULL,
  heartbeat_ms INTEGER NOT NULL
);
