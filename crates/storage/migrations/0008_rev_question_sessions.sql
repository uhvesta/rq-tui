CREATE TABLE IF NOT EXISTS rev_question_sessions (
  annotation_id    TEXT PRIMARY KEY REFERENCES annotations(id) ON DELETE CASCADE,
  session_id       TEXT NOT NULL UNIQUE,
  model_id         TEXT NOT NULL,
  reasoning_effort TEXT,
  context_tier     TEXT,
  state            TEXT NOT NULL DEFAULT 'ready',
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS rev_question_sessions_state
  ON rev_question_sessions(state, updated_at);
