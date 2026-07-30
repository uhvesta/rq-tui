PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS work_items (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  workspace_root TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL,
  last_opened_at TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
  id            TEXT PRIMARY KEY,
  work_item_id  TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
  parent_id     TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  active        INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS repos (
  id                 TEXT PRIMARY KEY,
  work_item_id       TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
  name               TEXT NOT NULL,
  path               TEXT NOT NULL,
  remote_pr_url      TEXT,
  pr_meta_json       TEXT,
  base_branch        TEXT,
  base_branch_source TEXT NOT NULL DEFAULT 'auto',
  last_activity_at   TEXT
);

CREATE TABLE IF NOT EXISTS contexts (
  work_item_id        TEXT PRIMARY KEY REFERENCES work_items(id) ON DELETE CASCADE,
  title               TEXT,
  what                TEXT,
  why                 TEXT,
  how                 TEXT,
  considerations      TEXT,
  alternatives        TEXT,
  source              TEXT NOT NULL,
  attached_to_session INTEGER NOT NULL DEFAULT 0,
  delivery_state      TEXT NOT NULL DEFAULT 'draft'
);

CREATE TABLE IF NOT EXISTS versions (
  id               TEXT PRIMARY KEY,
  repo_id          TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  version_num      INTEGER NOT NULL,
  kind             TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  head_sha         TEXT NOT NULL,
  worktree_path    TEXT,
  last_opened_at   TEXT
);

CREATE TABLE IF NOT EXISTS annotations (
  id               TEXT PRIMARY KEY,
  repo_id          TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind             TEXT NOT NULL,
  file_path        TEXT NOT NULL,
  anchor_snippet   TEXT NOT NULL,
  anchor_hash      TEXT NOT NULL,
  anchor_start_offset INTEGER NOT NULL DEFAULT 0,
  anchor_line_count   INTEGER NOT NULL DEFAULT 1,
  text             TEXT,
  submitted        INTEGER NOT NULL DEFAULT 0,
  delivery_state   TEXT NOT NULL DEFAULT 'draft',
  created_at       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS placements (
  annotation_id TEXT NOT NULL REFERENCES annotations(id) ON DELETE CASCADE,
  version_id    TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
  line_start    INTEGER NOT NULL,
  line_end      INTEGER NOT NULL,
  outdated      INTEGER NOT NULL DEFAULT 0,
  ambiguous     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (annotation_id, version_id)
);

CREATE TABLE IF NOT EXISTS ask_messages (
  id             TEXT PRIMARY KEY,
  annotation_id  TEXT NOT NULL REFERENCES annotations(id) ON DELETE CASCADE,
  seq            INTEGER NOT NULL,
  role           TEXT NOT NULL,
  text           TEXT NOT NULL,
  sent           INTEGER NOT NULL DEFAULT 0,
  delivery_state TEXT NOT NULL DEFAULT 'pending',
  ts             TEXT NOT NULL,
  UNIQUE (annotation_id, seq)
);

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS sessions_one_active_per_work_item
  ON sessions(work_item_id) WHERE active = 1;
CREATE INDEX IF NOT EXISTS versions_repo_version
  ON versions(repo_id, version_num);
CREATE INDEX IF NOT EXISTS placements_version
  ON placements(version_id);
CREATE INDEX IF NOT EXISTS annotations_repo_submitted
  ON annotations(repo_id, submitted);
CREATE INDEX IF NOT EXISTS ask_messages_annotation_seq
  ON ask_messages(annotation_id, seq);
CREATE INDEX IF NOT EXISTS work_items_last_opened
  ON work_items(last_opened_at);
