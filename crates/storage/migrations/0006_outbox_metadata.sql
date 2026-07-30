ALTER TABLE chat_outbox
  ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat'
  CHECK (kind IN ('chat', 'correction'));

ALTER TABLE chat_outbox
  ADD COLUMN lane TEXT NOT NULL DEFAULT 'main'
  CHECK (lane IN ('main', 'side'));
