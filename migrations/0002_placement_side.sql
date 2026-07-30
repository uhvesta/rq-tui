ALTER TABLE placements
ADD COLUMN side TEXT NOT NULL DEFAULT 'new'
CHECK (side IN ('old', 'new'));
