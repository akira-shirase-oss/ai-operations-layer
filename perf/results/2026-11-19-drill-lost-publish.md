# Drill: publish lost-response → reconcile + retry (2026-11-19) — live run recorded

The contract's hardest recovery case (`docs/END_GOAL.md`, `docs/DRILL.md`'s
`publish_lost_response` class): the publisher records the article, the response is lost,
and the operator must not rerun blind. The A3 answer is the control path (D-029..D-033):
reconcile reads the authority's own record; a retry resubmits under the business key; the
host dedups the effect.

| | |
|---|---|
| Commit | `9b058c9` (this file's commit is one child ahead) |
| Durable Runner | `v1.0.0` (`9889d6615c91`, pinned rev `15debd2a` is an ancestor), built into `.scratch/dr-src/target/` |
| Machine | Darwin arm64, load averages 9.8 / 14.3 / 13.1 — a busy development machine; timings are development observations, not capacity |
| Stack | external PostgreSQL (`AIOPS_PG_URL`, the coordinator's stack); temp DB `aiops_drill_<ts>` created and dropped |
| Status | **Live run PASSED.** Six cases, ~29 s, all children killed by process-group cleanup; all five ports verified free at exit. |

## Live evidence (this run)

Harness: `.scratch/drill-live.py` — real `aiops-server`, `reference-host`,
`aiops-dr-adapter`, `dr-article-worker` and `durable-server` binaries, each its own
process group; transcript in `.scratch/drill-live/transcript.txt`; control totals read
from the drill database before it was dropped.

| Case | What happened | Proof |
|---|---|---|
| A. Refhost `publish_lost_response` | POST → 504 `status:"unknown"`; op `no_finish_observed`, 8 events | Authority read: `times_published:1, times_attempted:1` |
| A. reconcile `{published:true}` | `reconciled`, `resolution:effect_present`, snapshot stored | control `b6d6a958…` |
| A. reconcile `{published:false}` | **409 `conflicting_request`** — a control row exists for the same (op, action, key) with a different body | replay-idempotency, not a blind accept |
| A. retry `{published:false}` | `denied`, `denied_reason:effect_confirmed` — the authority's answer decides, not the trace | control `82da08cf…`; `times_published` stayed 1 |
| B. Refhost `publish_rejected` | POST → 502; authority 404 `published:false`; op `failed/rejected` declared | reconcile `effect_absent` (control `e202ee53…`) |
| B. retry | `authorized` → drive → `confirmed`, `submitted_ref 63fdf92b…` | publication `times_published:1` — one effect |
| C. Crash window (refhost) | retry `authorized` at 14:44:09.319; **SIGKILL** before submit; restart | re-drive → `confirmed`, `submitted_ref 994b4f48…`, `attempts:1`; `times_published:1` — the commit survived the crash and drove exactly once |
| D. DR `publish_lost_response` | run 1 `done` (4 steps); collector op `no_finish_observed`, 10 events | reconcile `effect_present` (control `341bddc8…`); retry `denied`, `effect_confirmed` (`72f56099…`); `times_published:1` |
| E. DR `publish_rejected` | run `failed:step:3` (runner's own retries exhausted); authority 404 | reconcile `effect_absent`; retry `authorized` → `submitted`, **`submitted_ref:article/drill-live-e1-r797debf7`** — a NEW job under a NEW op_key, old key in input (D-031); `times_published:1` |
| F. DR crash window | retry `authorized`; **SIGKILL** before submit; restart | re-drive → `submitted`, `submitted_ref:article/drill-live-f1-r15432146`; `times_published:1` — one job submitted, one effect |

Database totals at teardown: **11 `audit_log` `control.*` rows (3 `denied`), 10
operations** — every authorization and denial audited.

## Automated proofs (in-repo, unchanged from the earlier entry)

| Proof | Where | What it asserts |
|---|---|---|
| Lost response + retry → `times_published == 1` | `fixtures/reference-host/tests/failure_cases.rs` (`publishing_is_idempotent_under_the_business_key`) | The authority dedups the effect across sequential and concurrent retries |
| Reconcile answers at the authority | `crates/aiops-server/tests/control.rs` (`reconcile_reads_the_authority_and_stores_its_answer`) | `effect_present`/`effect_absent`/`still_uncertain` committed with snapshot |
| Retry → submit → observe → `confirmed` | `retry_authorizes_then_the_executor_submits_and_confirms` | Full drive, one effect |
| Crash between commit and submit re-drives exactly once | `a_crash_before_submit_redrives_exactly_once` | Boot re-drive; `submitted` re-drive is attempt #2, still one effect |
| Concurrent duplicate retry → one submit, one effect | `a_repeated_retry_is_one_submit_and_one_effect` | Replay identity bounds the duplicates |
| DR: new `op_key`, key in input, one effect | `a_dr_retry_submits_a_new_job_with_the_key_in_its_input`, `a_dr_retry_resubmitted_is_still_one_effect` | D-031's semantics |
| Denials (`effect_confirmed`, `expected_state_mismatch`, `authority_unavailable`, `no_adapter`) | `retry_denies_*` | The authority's answer, not the trace, decides |

## Notes

- The DR `publish_lost_response` case exposes the honest-telemetry invariant end to end:
  the runner's journal said `done` while the collector's operation showed
  `no_finish_observed` — the adapter declares outcomes only from the runner's own answer,
  so a lost ack leaves the op uncertain rather than fabricated complete.
- Sections C and F prove the crash window on both adapters: the control record is
  committed before submission, the boot re-drive resumes it, and the business-key dedup
  keeps the effect at exactly one.
- The k6 control-surface load run is tracked separately (perf workflow run
  44544883621) — it gates p95/p99, not correctness.
