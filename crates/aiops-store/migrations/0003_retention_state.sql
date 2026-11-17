-- Review finding 10: the cutoff before which raw events may have been deleted by the
-- retention sweep, so a timeline can say "expired" instead of "never received" or
-- "proven missing". One row; it only moves forward.
CREATE TABLE retention_state (
    id smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    events_swept_before timestamptz NOT NULL
);
