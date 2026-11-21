-- Every revocation-path trigger fires under ANY session role (review
-- round 6, B2): a writer running `session_replication_role = replica`
-- silences an 'O' (origin) trigger — a revoke written from such a
-- session would commit without ever notifying the channel. 'A' (always)
-- fires regardless, so the channel's guarantees no longer depend on the
-- writer's session.
ALTER TABLE api_tokens ENABLE ALWAYS TRIGGER api_tokens_revoked_notify;
ALTER TABLE api_tokens ENABLE ALWAYS TRIGGER api_tokens_deleted_notify;
ALTER TABLE api_tokens ENABLE ALWAYS TRIGGER api_tokens_truncated_notify;
ALTER TABLE ingest_keys ENABLE ALWAYS TRIGGER ingest_keys_revoked_notify;
ALTER TABLE ingest_keys ENABLE ALWAYS TRIGGER ingest_keys_deleted_notify;
ALTER TABLE ingest_keys ENABLE ALWAYS TRIGGER ingest_keys_truncated_notify;
ALTER TABLE projects ENABLE ALWAYS TRIGGER projects_opt_in_notify;
ALTER TABLE revocation_heartbeat ENABLE ALWAYS TRIGGER revocation_heartbeat_notify;
