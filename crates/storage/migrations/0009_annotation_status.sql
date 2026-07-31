ALTER TABLE annotations
ADD COLUMN status TEXT NOT NULL DEFAULT 'active';

ALTER TABLE annotations
ADD COLUMN status_reason TEXT;

ALTER TABLE annotations
ADD COLUMN status_changed_at TEXT;

CREATE INDEX IF NOT EXISTS annotations_repo_status
ON annotations(repo_id, status);
