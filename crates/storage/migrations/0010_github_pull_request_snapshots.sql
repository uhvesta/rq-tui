-- A snapshot belongs to a local immutable review version.  Refreshing a PR
-- writes a new local version, so older descriptions and review-thread state
-- remain available in history instead of being overwritten in place.
CREATE TABLE IF NOT EXISTS pull_request_snapshots (
  version_id TEXT PRIMARY KEY REFERENCES versions(id) ON DELETE CASCADE,
  node_id    TEXT NOT NULL,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  url        TEXT NOT NULL,
  author     TEXT NOT NULL,
  head_sha   TEXT NOT NULL,
  base_sha   TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pull_request_review_threads (
  version_id        TEXT NOT NULL REFERENCES pull_request_snapshots(version_id) ON DELETE CASCADE,
  node_id           TEXT NOT NULL,
  ordinal           INTEGER NOT NULL CHECK (ordinal >= 0),
  path              TEXT NOT NULL,
  line              INTEGER,
  original_line     INTEGER,
  side              TEXT NOT NULL CHECK (side IN ('LEFT', 'RIGHT')),
  is_outdated       INTEGER NOT NULL CHECK (is_outdated IN (0, 1)),
  is_resolved       INTEGER NOT NULL CHECK (is_resolved IN (0, 1)),
  viewer_can_reply  INTEGER NOT NULL CHECK (viewer_can_reply IN (0, 1)),
  PRIMARY KEY (version_id, node_id),
  UNIQUE (version_id, ordinal)
);

CREATE TABLE IF NOT EXISTS pull_request_review_comments (
  version_id      TEXT NOT NULL,
  node_id         TEXT NOT NULL,
  thread_node_id  TEXT NOT NULL,
  ordinal         INTEGER NOT NULL CHECK (ordinal >= 0),
  database_id     INTEGER,
  author          TEXT NOT NULL,
  body            TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  url             TEXT NOT NULL,
  reply_to        TEXT,
  PRIMARY KEY (version_id, node_id),
  UNIQUE (version_id, thread_node_id, ordinal),
  FOREIGN KEY (version_id, thread_node_id)
    REFERENCES pull_request_review_threads(version_id, node_id)
    ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS pull_request_snapshots_node
  ON pull_request_snapshots(node_id);
CREATE INDEX IF NOT EXISTS pull_request_review_threads_location
  ON pull_request_review_threads(version_id, path, line, original_line);
CREATE INDEX IF NOT EXISTS pull_request_review_comments_thread
  ON pull_request_review_comments(version_id, thread_node_id, ordinal);
