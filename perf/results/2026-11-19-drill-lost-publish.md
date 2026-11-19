# Drill: publish lost-response → reconcile + retry (2026-11-19) — automated proofs; live run owed

The contract's hardest recovery case (`docs/END_GOAL.md`, DRILL.md's `publish_lost_response`
class): the publisher records the article, the response is lost, and the operator must not
rerun blind. The A3 answer is the control path (D-029..D-033): reconcile reads the
authority's own record; a retry resubmits under the business key; the host dedups the effect.

| | |
|---|---|
| Commit | `d0e8ee1` plus this file's commit |
| Machine | development machine (proofs compile-verified here; executed on CI — no local PostgreSQL) |
| Status | **The live-stack run is still owed.** The proofs below are in-repo automated tests; this file records which exist and what each guarantees, so a live run confirms rather than discovers. |

## The scenario

1. `publish_lost_response` injected: the publisher's `publish` records the effect, the
   response is lost; telemetry shows `uncertain`.
2. `POST …/reconcile {business_key, expected_state:{published:true}}` → the collector asks
   `GET /publications/{key}` on the host (not the trace) → `reconciled`,
   `resolution: effect_present`, the answer snapshot stored.
3. The same with `published:false` asserted → `expected_state_mismatch`, denied, audited.
4. `POST …/retry {business_key, expected_state:{published:false}}` on an op whose publish
   truly never landed → `authorized`, executor submits `POST /articles/retry`, observes
   the authority, `confirmed`. A second drive (the crash window) re-submits → the host
   dedups: `times_published` stays 1 (attempts count separately).
5. Durable Runner variant (D-031): retry submits a NEW job under a NEW `op_key`, the
   business key in its input; the worker publishes under the key, deduped — two runs,
   one effect.

## Automated proofs (in-repo)

| Proof | Where | What it asserts |
|---|---|---|
| Lost response + retry → `times_published == 1` | `fixtures/reference-host/tests/failure_cases.rs` (`publishing_is_idempotent_under_the_business_key`, updated this branch) | The authority dedups the effect across sequential and concurrent retries |
| Reconcile answers at the authority | `crates/aiops-server/tests/control.rs` (`reconcile_reads_the_authority_and_stores_its_answer`) | `effect_present`/`effect_absent`/`still_uncertain` committed with snapshot |
| Retry → submit → observe → `confirmed` | `retry_authorizes_then_the_executor_submits_and_confirms` | Full drive, one effect |
| Crash between commit and submit re-drives exactly once | `a_crash_before_submit_redrives_exactly_once` | Boot re-drive; `submitted` re-drive is attempt #2, still one effect |
| Concurrent duplicate retry → one submit, one effect | `a_repeated_retry_is_one_submit_and_one_effect` | Replay identity bounds the duplicates |
| DR: new `op_key`, key in input, one effect | `a_dr_retry_submits_a_new_job_with_the_key_in_its_input`, `a_dr_retry_resubmitted_is_still_one_effect` | D-031's semantics |
| Denials (`effect_confirmed`, `expected_state_mismatch`, `authority_unavailable`, `no_adapter`) | `retry_denies_*` | The authority's answer, not the trace, decides |

## What a live run still has to show

- The real reference host binary (not the in-process stub) with `AIOPS_CONTROL_REFERENCE_HOST`
  wired: seed a `publish_lost_response`, reconcile, then a denied retry (effect present),
  with `curl` transcripts. Same for the Durable Runner path against the real runner +
  worker (`AIOPS_CONTROL_DURABLE_RUNNER*`).
- The `perf/k6/control.js` load run on a seeded op set.
- Both need the shared stack (PostgreSQL + ports); CI runs the test battery on push,
  the stack run is a coordinator/quiet-machine step.
