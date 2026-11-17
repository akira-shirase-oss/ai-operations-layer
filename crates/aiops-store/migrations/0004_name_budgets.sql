-- Review finding 11: names are budgeted per ingest key within the project, swept when
-- unused for 90 days, and an over-budget name is stored as `_overflow` and counted.
ALTER TABLE known_names
    ADD COLUMN ingest_key_id uuid,
    ADD COLUMN last_seen timestamptz NOT NULL DEFAULT now();
CREATE INDEX known_names_by_seen ON known_names (last_seen);
ALTER TABLE events ADD COLUMN overflowed_names smallint NOT NULL DEFAULT 0;
