-- Schema v1 of the AI Operations Layer's store (D-004, D-015, D-016, D-018).
-- PostgreSQL is the layer's only authority for what it stores. Telemetry here is
-- evidence, never authority for jobs, money or publishing (invariant 1).

CREATE TABLE projects (
    id uuid PRIMARY KEY,
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    -- Attribute keys this project opted in to beyond the default allowlist (D-006).
    opt_in_attribute_keys text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);

-- One per host or service; write-only to ingest; SHA-256 of the key (D-015).
CREATE TABLE ingest_keys (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects (id),
    name text NOT NULL,
    key_hash bytea NOT NULL UNIQUE CHECK (length(key_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    UNIQUE (project_id, name)
);

-- Console/API tokens: an actor, projects, scopes, an expiry (D-015).
CREATE TABLE api_tokens (
    id uuid PRIMARY KEY,
    actor text NOT NULL,
    token_hash bytea NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    scopes text[] NOT NULL,
    project_ids uuid[] NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz
);

-- Raw events: 14 days by receive time (D-016). The dedup key is (project, event_id);
-- event time and receive time are both kept (D-005); ingest_key_id is provenance.
CREATE TABLE events (
    project_id uuid NOT NULL,
    event_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    instance_id uuid NOT NULL,
    ingest_key_id uuid NOT NULL,
    seq bigint NOT NULL,
    op_seq integer NOT NULL,
    event_time timestamptz NOT NULL,
    received_at timestamptz NOT NULL,
    withheld_attributes smallint NOT NULL DEFAULT 0,
    redactions smallint NOT NULL DEFAULT 0,
    body jsonb NOT NULL,
    PRIMARY KEY (project_id, event_id)
);
CREATE INDEX events_by_operation ON events (project_id, operation_id, event_time);
CREATE INDEX events_by_received ON events (received_at);

-- Per-operation summaries (90 days, D-016), maintained only from newly stored events so
-- re-delivery never inflates a counter.
CREATE TABLE operations (
    project_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    name text,
    request_id text,
    release text,
    first_event_time timestamptz NOT NULL,
    last_event_time timestamptz NOT NULL,
    first_received_at timestamptz NOT NULL,
    last_received_at timestamptz NOT NULL,
    host_events integer NOT NULL DEFAULT 0,
    gap_reports integer NOT NULL DEFAULT 0,
    reported_dropped bigint NOT NULL DEFAULT 0,
    finish_outcome text CHECK (finish_outcome IN ('succeeded', 'failed', 'uncertain')),
    finish_detail text,
    events_emitted integer,
    failed_attempts integer NOT NULL DEFAULT 0,
    uncertain_attempts integer NOT NULL DEFAULT 0,
    cost_estimate_micros bigint NOT NULL DEFAULT 0,
    cost_currency text,
    PRIMARY KEY (project_id, operation_id)
);
CREATE INDEX operations_recent ON operations (project_id, first_event_time DESC, operation_id DESC);
CREATE INDEX operations_by_request ON operations (project_id, request_id) WHERE request_id IS NOT NULL;
CREATE INDEX operations_by_received ON operations (last_received_at);

-- One row per SDK process (D-018): the latest cumulative counters it reported, and what
-- the collector counted itself. Kept 90 days so expiring detail never hides loss.
CREATE TABLE source_instances (
    project_id uuid NOT NULL,
    instance_id uuid NOT NULL,
    ingest_key_id uuid NOT NULL,
    service text NOT NULL,
    sdk text NOT NULL,
    first_seen timestamptz NOT NULL,
    last_seen timestamptz NOT NULL,
    max_seq bigint NOT NULL,
    events_received bigint NOT NULL DEFAULT 0,
    events_refused bigint NOT NULL DEFAULT 0,
    dropped_queue_full bigint NOT NULL DEFAULT 0,
    dropped_send_failed bigint NOT NULL DEFAULT 0,
    dropped_rejected bigint NOT NULL DEFAULT 0,
    gap_overflow bigint NOT NULL DEFAULT 0,
    truncated_events bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, instance_id)
);
CREATE INDEX source_instances_by_seen ON source_instances (last_seen);

-- Distinct names a project introduced (bounded cardinality, D-014).
CREATE TABLE known_names (
    project_id uuid NOT NULL,
    name text NOT NULL,
    PRIMARY KEY (project_id, name)
);

-- Every authorization decision the collector audits (D-015): 365 days (D-016).
CREATE TABLE audit_log (
    id bigserial PRIMARY KEY,
    at timestamptz NOT NULL DEFAULT now(),
    actor text NOT NULL,
    token_id uuid,
    action text NOT NULL,
    project_slug text,
    target text,
    decision text NOT NULL CHECK (decision IN ('allowed', 'denied')),
    reason text NOT NULL
);
CREATE INDEX audit_log_by_time ON audit_log (at);
