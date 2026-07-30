CREATE TABLE IF NOT EXISTS chat_outbox (
  id           TEXT PRIMARY KEY,
  work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
  text         TEXT NOT NULL,
  created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS chat_outbox_work_item_created
  ON chat_outbox(work_item_id, created_at, id);
