-- Review U2: the host's `error_code` on the declared finish (schema v1 already carries it),
-- kept beside the outcome so the list can word a cancelled run as cancelled. Like the
-- outcome, the first finish received is kept.
ALTER TABLE operations ADD COLUMN finish_code text;
