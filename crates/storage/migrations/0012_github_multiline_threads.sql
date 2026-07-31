ALTER TABLE pull_request_review_threads ADD COLUMN start_line INTEGER;
ALTER TABLE pull_request_review_threads ADD COLUMN original_start_line INTEGER;
ALTER TABLE pull_request_review_threads ADD COLUMN start_side TEXT;
