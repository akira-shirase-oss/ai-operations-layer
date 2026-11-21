-- Trigger-path heartbeat and a bounded catch-up (review N-C, N-H).
--
-- The heartbeat used to be `SELECT pg_notify(…)` on the serving pool: it
-- proved the LISTEN connection was live but said nothing about the
-- triggers — a backend running `session_replication_role = replica`
-- delivered heartbeats while every real revocation NOTIFY silently never
-- fired. The beat now UPDATEs a one-row table whose trigger emits on the
-- same channel, so an observed beat proves the trigger path end to end.
-- The write runs on the watcher's own connection (application_name
-- `aiops-revocation-writer`), never a tenant pool.

CREATE TABLE revocation_heartbeat (
    id   bool PRIMARY KEY DEFAULT true CHECK (id),
    beat timestamptz NOT NULL DEFAULT now()
);

INSERT INTO revocation_heartbeat DEFAULT VALUES;

CREATE OR REPLACE FUNCTION aiops_notify_revocation_heartbeat() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('aiops_credential_revoked', 'heartbeat');
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER revocation_heartbeat_notify
    AFTER UPDATE ON revocation_heartbeat
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_revocation_heartbeat();

-- The catch-up read after a listener gap only asks for revocations inside
-- the credential cache's max entry age (ttl + stale): a row revoked
-- longer ago cannot still be cached, and the unbounded `revoked_at IS NOT
-- NULL` scan grew with every revoke (review N-C). The partial index keeps
-- the read at window size.
CREATE INDEX api_tokens_revoked_at_recent
    ON api_tokens (revoked_at)
    WHERE revoked_at IS NOT NULL;
