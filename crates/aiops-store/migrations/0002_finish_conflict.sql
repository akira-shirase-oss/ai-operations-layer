-- Review finding 2: an operation whose hosts declared different outcomes is
-- `conflicting` in the list as in the timeline; the first finish received no longer
-- stands for the operation on its own.
ALTER TABLE operations ADD COLUMN finish_conflict boolean NOT NULL DEFAULT false;
