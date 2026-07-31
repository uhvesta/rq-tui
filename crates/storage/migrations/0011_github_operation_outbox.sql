-- A GitHub request is recorded before it is sent.  Its annotation rows are a
-- fixed snapshot so completion and recovery can never affect other pending
-- feedback in the same workspace or repository.
CREATE TABLE IF NOT EXISTS github_operations (
  operation_id    TEXT PRIMARY KEY,
  repo_id         TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  version_id      TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
  kind            TEXT NOT NULL CHECK (kind IN ('review', 'reply')),
  request_json    TEXT NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  state           TEXT NOT NULL CHECK (state IN ('prepared', 'sent', 'unknown', 'failed')),
  last_error      TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS github_operation_annotations (
  operation_id TEXT NOT NULL REFERENCES github_operations(operation_id) ON DELETE CASCADE,
  annotation_id TEXT NOT NULL REFERENCES annotations(id) ON DELETE RESTRICT,
  ordinal      INTEGER NOT NULL CHECK (ordinal >= 0),
  PRIMARY KEY (operation_id, annotation_id),
  UNIQUE (operation_id, ordinal)
);

CREATE INDEX IF NOT EXISTS github_operations_unresolved
  ON github_operations(state, created_at, operation_id)
  WHERE state IN ('prepared', 'unknown');
CREATE INDEX IF NOT EXISTS github_operation_annotations_annotation
  ON github_operation_annotations(annotation_id, operation_id);
