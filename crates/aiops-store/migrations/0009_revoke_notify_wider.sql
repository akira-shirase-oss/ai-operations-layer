-- Wider revocation coverage (review S3): 0008 fires only on
-- `UPDATE OF revoked_at`. A DELETE or TRUNCATE on a credential table and an
-- `opt_in_attribute_keys` change on a project are credential-affecting
-- writes a listener must not miss — DELETEs carry the dropped digest,
-- TRUNCATE and opt-in withdrawal carry `clear` (the ingest-key cache's
-- positive entries hold opt_in_attribute_keys, so a project's withdrawal
-- can only be honored by emptying that map).
--
-- `clear` is a payload sibling of the digest protocol in
-- crates/aiops-server/src/notify.rs; it is rare (DDL / admin ops) and cheap
-- relative to leaking a withdrawn opt-in for a cache TTL.

CREATE OR REPLACE FUNCTION aiops_notify_token_delete() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('aiops_credential_revoked', encode(OLD.token_hash, 'hex'));
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION aiops_notify_ingest_key_delete() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('aiops_credential_revoked', encode(OLD.key_hash, 'hex'));
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION aiops_notify_credential_clear() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('aiops_credential_revoked', 'clear');
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER api_tokens_deleted_notify
    AFTER DELETE ON api_tokens
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_token_delete();

CREATE TRIGGER ingest_keys_deleted_notify
    AFTER DELETE ON ingest_keys
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_ingest_key_delete();

CREATE TRIGGER api_tokens_truncated_notify
    AFTER TRUNCATE ON api_tokens
    FOR EACH STATEMENT EXECUTE FUNCTION aiops_notify_credential_clear();

CREATE TRIGGER ingest_keys_truncated_notify
    AFTER TRUNCATE ON ingest_keys
    FOR EACH STATEMENT EXECUTE FUNCTION aiops_notify_credential_clear();

-- A project's opt-in set rides inside the cached ingest-key record — its
-- withdrawal must empty that cache now, not at TTL (review S3).
CREATE TRIGGER projects_opt_in_notify
    AFTER UPDATE OF opt_in_attribute_keys ON projects
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_credential_clear();
