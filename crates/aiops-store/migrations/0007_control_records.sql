-- A3 controlled recovery (invariant 4, D-007): the layer's authoritative record of one
-- operator's intent to act on one operation. The record, its authorization decision and
-- its audit row commit in one transaction; the external call happens only after that
-- commit and is idempotent under the business key (D-030, D-033). A control record is
-- evidence plus decision — never a command the telemetry could have issued itself.
CREATE TABLE control_records (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects (id),
    -- The operation the control concerns (its own row can age out earlier than this
    -- record; the record is kept).
    operation_id uuid NOT NULL,
    action text NOT NULL CHECK (action IN ('reconcile', 'retry')),
    business_key text NOT NULL,
    -- The adapter-typed assertion the caller signed (<= 4 KiB, validated by the adapter;
    -- D-032). No secrets: the field list is fixed per adapter.
    expected_state jsonb NOT NULL,
    actor text NOT NULL,
    token_id uuid NOT NULL,
    -- The domain state machine's status name (aiops_domain::control).
    status text NOT NULL,
    -- A reconcile's stored answer; NULL until reconciled or while a retry decides.
    resolution text CHECK (resolution IN ('effect_present', 'effect_absent', 'still_uncertain')),
    -- {source, answer, read_at}: what the authority said when we asked.
    authority_read jsonb,
    -- The new work's own identity (the resubmitted operation id; for the Durable Runner
    -- its new op_key, which with business_key is the mapping — D-031).
    submitted_ref text,
    -- v1: the record is the approval; the authorizing actor is the approver, written
    -- with the decision (a denied record has no approver) — D-029.
    approved_by text,
    approved_at timestamptz,
    -- Reserved for two-person sign-off — a later contract, not v1.
    second_approver text,
    deny_reason text,
    -- Submit attempts; the re-drive budget ends them at a bounded count.
    attempts integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- The replay identity: a repeated call with the same body returns this record; a
    -- different body on the same key is an explicit conflict.
    UNIQUE (project_id, operation_id, action, business_key)
);
-- The executor's work queue and boot re-drive: rows a crash could have stranded.
CREATE INDEX control_records_resumable ON control_records (status)
    WHERE status IN ('requested', 'reconciling', 'authorized', 'submitted');
CREATE INDEX control_records_by_operation
    ON control_records (project_id, operation_id, created_at DESC);
