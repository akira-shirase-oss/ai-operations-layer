-- Review finding 15: an operation with events from more than one source instance has no
-- single emitted count, so the list must not claim it complete from one source's count.
ALTER TABLE operations
    ADD COLUMN first_instance_id uuid,
    ADD COLUMN multi_source boolean NOT NULL DEFAULT false;
