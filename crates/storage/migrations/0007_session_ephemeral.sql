ALTER TABLE sessions
  ADD COLUMN ephemeral INTEGER NOT NULL DEFAULT 0
  CHECK (ephemeral IN (0, 1));

CREATE INDEX IF NOT EXISTS sessions_work_item_ephemeral
  ON sessions(work_item_id, ephemeral, active);
