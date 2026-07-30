CREATE TABLE IF NOT EXISTS prune_operations (
  operation_id TEXT PRIMARY KEY,
  work_item_id TEXT NOT NULL,
  export_first INTEGER NOT NULL CHECK (export_first IN (0, 1)),
  phase TEXT NOT NULL CHECK (phase IN ('remote_pending', 'local_pending', 'failed')),
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- A journal row remains until local cleanup has completed and explicitly clears
-- it, so a Work Item can have at most one resumable prune operation.
CREATE UNIQUE INDEX IF NOT EXISTS prune_operations_one_unfinished_per_work_item
  ON prune_operations(work_item_id);

-- Deliberately no foreign key to work_items: this snapshot must survive the
-- cascading deletion of its source Work Item until remote cleanup is complete.
CREATE TABLE IF NOT EXISTS prune_targets (
  operation_id TEXT NOT NULL REFERENCES prune_operations(operation_id) ON DELETE CASCADE,
  target_key TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('persistent', 'side')),
  session_id TEXT,
  side_operation_id TEXT,
  parent_id TEXT,
  state TEXT NOT NULL CHECK (state IN ('pending', 'deleted')),
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (operation_id, target_key)
);

CREATE INDEX IF NOT EXISTS prune_targets_operation_state
  ON prune_targets(operation_id, state, target_key);
