# Control-surface run: million-ci-2 (2026-11-20)

Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity).

| | |
|---|---|
| Commit | `76fc524` |
| Machine | AMD EPYC 7763 64-Core Processor, 4 cores, 15 GB, Linux 7.0.0-1017-azure x86_64 |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:5432 (external, AIOPS_PG_URL), 16 ingest connections |
| CI run | https://github.com/akira-shirase-oss/ai-operations-layer/actions/runs/44763875623 |
| Load average (1/5/15 min) before | 2.33, 0.82, 0.30 |
| … after | 2.58, 2.64, 2.45 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 300 control req/s for 3400s + 100 ingest batches/s concurrently |
| Credentials | 79 admin tokens, 17 read tokens, a 600-id revoke pool |

## The mix (`perf/k6/control.js`, exit 0)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 1.42 | 5.66 | 9.4 | 38.33 |
| controls (recovery reads) | 0.55 | 4.03 | 7.29 | 27.04 |
| credentials | 9.64 | 17.58 | 24.65 | 66.81 |
| denied (audited 403) | 0.76 | 4.11 | 6.31 | 114.12 |
| revoke (allowed→audited 404) | 1.25 | 6.08 | 10.1 | 98.84 |

| Metric | Value |
|---|---|
| Requests (total) | 1360081 |
| Control requests (rate achieved) | 299.64 |
| Rate-limited by the control bucket (429) | 124979 |
| Audit pages walked | 408116 |
| Checks passed (rate) | 1 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 0 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 3400010 |
| **Events lost** | 0 |
| Batches retried after no reply | 0 |
| Ingest p50 / p95 / p99 (ms) | 2.31 / 6.82 / 9.71 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 3400010 |
| Operations stored | 340001 |
| Audit rows (the log the walk read) | 121555 |
| … denials among them | 120955 |
| Tokens revoked by the run | 600 |
| Collector RSS after the run | 20072 KiB |
| Collector counters | `{"batches":340001,"events_accepted":3400010,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":124979,"denied":70815,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":340002,"wait_ns_total":21921088,"wait_ns_max":59051,"hold_ns_total":93054240,"hold_ns_max":521668},"names_shared":{"acquisitions":340001,"wait_ns_total":13070588,"wait_ns_max":54712,"hold_ns_total":313338674,"hold_ns_max":2823584},"names_exclusive":{"acquisitions":1,"wait_ns_total":50,"wait_ns_max":50,"hold_ns_total":5390,"hold_ns_max":5390},"ingest_key_cache":{"acquisitions":340230,"wait_ns_total":26249522,"wait_ns_max":96260,"hold_ns_total":280926392,"hold_ns_max":1019702},"ingest_rate_limit":{"acquisitions":340001,"wait_ns_total":16076715,"wait_ns_max":234540,"hold_ns_total":116224605,"hold_ns_max":230773},"token_cache":{"acquisitions":1039247,"wait_ns_total":85268505,"wait_ns_max":1020634,"hold_ns_total":1068879489,"hold_ns_max":1082690},"query_rate_limit":{"acquisitions":1,"wait_ns_total":40,"wait_ns_max":40,"hold_ns_total":1282,"hold_ns_max":1282},"project_cache":{"acquisitions":211990,"wait_ns_total":12962010,"wait_ns_max":83116,"hold_ns_total":82516525,"hold_ns_max":297377}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<150' p(95)=5.66ms
✓ 'p(99)<300' p(99)=9.4ms
✓ 'p(95)<150' p(95)=4.03ms
✓ 'p(99)<300' p(99)=7.28ms
✓ 'p(95)<200' p(95)=17.58ms
✓ 'p(99)<400' p(99)=24.64ms
✓ 'p(95)<200' p(95)=4.11ms
✓ 'p(99)<400' p(99)=6.31ms
✓ 'p(95)<250' p(95)=6.07ms
✓ 'p(99)<500' p(99)=10.1ms
✓ 'p(95)<100' p(95)=6.82ms
✓ 'p(99)<250' p(99)=9.71ms
✓ 'rate==0' rate=0.00%
✓ controls answered
✓ revoke answered
✓ ingest 200
✓ every event stored
✓ denied answered
✓ audit page answered
✓ audit page is a page
✓ credentials answered
```
