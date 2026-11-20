-- Cross-process credential revocation (D-034, amended 2026-11-20): a
-- revoke lands as a row update in ANY process — the admin CLI included —
-- and every running collector must evict it before the next request, not
-- when its credential cache ages out (the Payment Sandbox's D-102 model).
-- Each trigger NOTIFYs the revoked digest on `aiops_credential_revoked`;
-- serving peers LISTEN and drop exactly that cache entry.
--
-- Two functions, not one: plpgsql binds NEW.field references against the
-- rowtype of the table the trigger fires on, and a function shared across
-- tables resolves those references against whichever table compiled the
-- cached plan first — the second table then fails with "record new has no
-- field". One tiny function per table is the correct shared nothing.
CREATE OR REPLACE FUNCTION aiops_notify_token_revocation() RETURNS trigger AS $$
BEGIN
    IF OLD.revoked_at IS NULL AND NEW.revoked_at IS NOT NULL THEN
        PERFORM pg_notify('aiops_credential_revoked', encode(NEW.token_hash, 'hex'));
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION aiops_notify_ingest_key_revocation() RETURNS trigger AS $$
BEGIN
    IF OLD.revoked_at IS NULL AND NEW.revoked_at IS NOT NULL THEN
        PERFORM pg_notify('aiops_credential_revoked', encode(NEW.key_hash, 'hex'));
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER api_tokens_revoked_notify
    AFTER UPDATE OF revoked_at ON api_tokens
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_token_revocation();

CREATE TRIGGER ingest_keys_revoked_notify
    AFTER UPDATE OF revoked_at ON ingest_keys
    FOR EACH ROW EXECUTE FUNCTION aiops_notify_ingest_key_revocation();
