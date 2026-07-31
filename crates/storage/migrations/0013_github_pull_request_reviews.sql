-- Overall reviews are distinct from inline review threads. They preserve the
-- review decision/body needed to render the PR discussion and to reconcile a
-- locally journaled review request after an interrupted transport response.
CREATE TABLE IF NOT EXISTS pull_request_reviews (
  version_id   TEXT NOT NULL REFERENCES pull_request_snapshots(version_id) ON DELETE CASCADE,
  node_id      TEXT NOT NULL,
  ordinal      INTEGER NOT NULL CHECK (ordinal >= 0),
  database_id  INTEGER,
  author       TEXT NOT NULL,
  body         TEXT NOT NULL,
  state        TEXT NOT NULL,
  commit_sha   TEXT,
  submitted_at TEXT,
  url          TEXT NOT NULL,
  PRIMARY KEY (version_id, node_id),
  UNIQUE (version_id, ordinal)
);

CREATE INDEX IF NOT EXISTS pull_request_reviews_version_state
  ON pull_request_reviews(version_id, state, submitted_at);
