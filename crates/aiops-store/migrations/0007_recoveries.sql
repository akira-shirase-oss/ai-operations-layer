-- Controlled recovery (A3, D-030). One row per plan: what the person saw, what the runner
-- said, what the effect checks said, the key a resubmit uses and, once a person confirms,
-- who approved it and the new run. A replayed plan (same request_key) or a second plan
-- for the same derived key is the same row, never a second resubmit.
CREATE TABLE recoveries (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects (id),
    operation_id uuid NOT NULL,
    request_key text NOT NULL CHECK (length(request_key) BETWEEN 8 AND 64),
    planned_by text NOT NULL,
    runner text NOT NULL,
    run_id bigint NOT NULL,
    business_key text NOT NULL,
    expected_state text NOT NULL,
    observed_state text NOT NULL,
    effect text NOT NULL CHECK (effect IN ('not_landed', 'landed', 'unknown')),
    effect_detail text,
    recovery_number int NOT NULL CHECK (recovery_number > 0),
    derived_key text NOT NULL,
    status text NOT NULL CHECK (status IN ('planned', 'blocked', 'submitting', 'submitted')),
    approved_by text,
    approved_at timestamptz,
    new_run_id bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, request_key),
    UNIQUE (project_id, runner, derived_key)
);
CREATE INDEX recoveries_by_operation ON recoveries (project_id, operation_id, created_at);
