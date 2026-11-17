# The Durable Runner host and adapter (next milestone: A2, then A3)

Status: **A2 built (observe only); A3 planned** (2026-11-17). What follows
was re-read from Durable Runner's public contract at A2 start (OQ7). The
answer is D-027. The one change Durable Runner needed, handler-call
headers naming the run, step and attempt, is its own D-050 (`9d240a7`).

## Why Durable Runner

It is the one system in the portfolio that already runs multi-step work
with attempts, per-attempt deadlines and its own reconcile of a vanished
handler reply, under a business idempotency key (`op_key` on
`POST /jobs`), and that answers "what state is this run in" from its own
authority (`GET /jobs/{id}`: `Queued`, `Running`, `RetryWaiting`,
`Reconciling`, `Done`, `Failed`, `Cancelled`). That is exactly the
evidence/authority split this layer needs, in a real product.

## A2 — Durable Runner as the first real host (observe only) — built

- **Where the events come from.** Durable Runner calls `aiops-dr-adapter`
  (`crates/aiops-durable-runner`) as each allowlisted handler
  (`--handler <route>=http://<adapter>/h/<route>`). The adapter forwards
  every call unchanged to the real handler, here
  `fixtures/dr-article-worker`, which knows nothing of this layer. Neither
  the runner nor the handlers are instrumented. The adapter never changes
  what the runner decides:
  - the handler's answer goes back unchanged;
  - an unreachable handler is answered 502 (the runner fails that attempt
    either way);
  - a connection lost after sending reaches the runner as a reply it cannot
    read, so it reconciles as it would have;
  - the adapter waits past the runner's step deadline, so the runner's own
    timeout decides.
- **Correlation (OQ7, D-027).** Each call carries the runner's
  `x-durable-run-id`, `x-durable-step` and `x-durable-attempt`. The
  operation ID is derived from the runner's label and the run ID, and the
  step IDs from the operation and the step index. So attempts reported by
  different adapter instances land on one step. The job input is not read
  for correlation.
- **What is reported.** Each attempt is a step under the runner's own
  attempt number. Each reconcile probe is a step of its own
  (`<route>.reconcile`, a child of the step it reconciles). A call the
  runner gave up on is `uncertain: timeout_after_send` whatever the handler
  said later, with that said in the message. The operation's finish comes
  only from `GET /jobs/{id}` (read-only key): `done` → succeeded,
  `failed:step:N` → failed (the class of the last failed attempt seen
  there), `cancelled` → failed with code `cancelled`. A run the runner no
  longer knows is left unfinished.
- **Authority beside evidence.** The console shows the runner's answer in
  its own card (`GET …/operations/{id}/authority`), labeled and timed. The
  collector asks only after the link verifies by derivation.
- **Proven** (A2 row in `docs/ROADMAP.md`): unit and HTTP tests in the
  adapter crate, in-process tests of the authority read, and four Chromium
  journeys against the real runner in `scripts/console-e2e.sh`.
- **The drill (D-024, D-028)** runs here (`scripts/drill.py seed --host
  durable-runner`). Its first dry run opened OQ8.
- **Gate:** a second developer tells a real failure from missing telemetry
  on this workflow without guessing. Not yet run: it needs that person.

## A3 — the Durable Runner adapter (controlled recovery)

The adapter is the only component that may cause an external action, and
only inside Durable Runner's ownership contract:

| Control | What the adapter does | What it never does |
|---|---|---|
| Inspect | `GET /jobs/{id}` (the authority) before any decision | infer state from the trace |
| Reconcile | wait while the run is `Reconciling`; read the publisher (the effect's authority) for an `uncertain` publish | treat a missing span as "did not run" |
| Retry | `POST /jobs` with the **same** `op_key` (the business key) only when the authority says the run is terminal and the effect is absent | submit a new `op_key`; offer a generic "run again" |
| Cancel | `POST /jobs/{id}/cancel` (stops scheduling; never undoes) | claim a cancel undid an effect |

- **Every control call** carries actor scope (`operate`), the expected run
  state read from the authority (refused if it changed), and the business
  key; the decision and its audit row commit together in PostgreSQL before
  the adapter calls Durable Runner; the outcome is observed through both
  the trace and the authority.
- **Credentials.** Durable Runner's API key is all-or-nothing and its reads
  are open, so this layer is the actor-scope boundary for the controls it
  exposes. The adapter alone holds the key (one per adapter deployment),
  it never reaches the console or logs, and it is treated as an operator
  credential.
- **Found at A2 (must be settled before A3 builds Retry):** an `op_key`
  returns its first run. `POST /jobs` with the same `op_key` after a
  terminal failure returns that failed run, and nothing re-executes. So
  "retry under the same business key" cannot rerun a failed Durable Runner
  job. A rerun is a new job under a new `op_key`, and the business key must
  then be carried by the effect's own authority (the publisher's record)
  rather than by the runner's dedup.
- **A known edge:** Durable Runner expires terminal history after seven
  days, and an `op_key` reused after that re-executes. The adapter refuses
  a retry when the run is no longer inspectable rather than resubmitting
  into a possibly duplicated effect; the refusal is audited.

## Failure cases the adapter milestone must test

- Durable Runner down or `503`: controls refuse with a retryable answer;
  nothing is recorded as done.
- The run state changed between read and act: refused (expected state).
- A replayed control call: a no-op (same business key, same expected
  state).
- An unauthorized retry (no `operate`, another project): denied and
  audited.
- A lost publish response: reconcile against the publisher first; the
  business key prevents a duplicate publish.
