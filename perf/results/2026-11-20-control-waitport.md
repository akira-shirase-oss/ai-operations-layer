# Control-surface run: waitport (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `7e2ba48-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 11.27 43.40 44.87 |
| … after | 9.07 40.82 43.90 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 5 control req/s for 15s + 2 ingest batches/s concurrently |
| Credentials | 5 admin tokens, 2 read tokens, a 3-id revoke pool |

## The mix (`perf/k6/control.js`, exit 0)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 1.99 | 6.26 | 7.94 | 8.66 |
| controls (recovery reads) | 1.75 | 4.92 | 5.96 | 6.22 |
| credentials | 5.2 | 11.27 | 11.74 | 11.85 |
| denied (audited 403) | 4.76 | 31.76 | 39.65 | 41.63 |
| revoke (allowed→audited 404) | 10.85 | 10.85 | 10.85 | 10.85 |

| Metric | Value |
|---|---|
| Requests (total) | 187 |
| Control requests (rate achieved) | 3.89 |
| Rate-limited by the control bucket (429) | 0 |
| Audit pages walked | 37 |
| Checks passed (rate) | 1 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 0 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 310 |
| **Events lost** | 0 |
| Ingest p50 / p95 / p99 (ms) | 9.71 / 26.11 / 38.82 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 310 |
| Operations stored | 31 |
| Audit rows (the log the walk read) | 71 |
| … denials among them | 70 |
| Tokens revoked by the run | 1 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Collector counters | `{"batches":31,"events_accepted":310,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":18,"denied":70,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":32,"wait_ns_total":3623,"wait_ns_max":375,"hold_ns_total":19129,"hold_ns_max":1625},"names_shared":{"acquisitions":31,"wait_ns_total":2127,"wait_ns_max":333,"hold_ns_total":54371,"hold_ns_max":4583},"names_exclusive":{"acquisitions":1,"wait_ns_total":0,"wait_ns_max":0,"hold_ns_total":3500,"hold_ns_max":3500},"ingest_key_cache":{"acquisitions":34,"wait_ns_total":8667,"wait_ns_max":625,"hold_ns_total":37962,"hold_ns_max":3333},"ingest_rate_limit":{"acquisitions":31,"wait_ns_total":5377,"wait_ns_max":708,"hold_ns_total":23959,"hold_ns_max":2208},"token_cache":{"acquisitions":181,"wait_ns_total":49792,"wait_ns_max":1125,"hold_ns_total":299622,"hold_ns_max":27500},"query_rate_limit":{"acquisitions":1,"wait_ns_total":375,"wait_ns_max":375,"hold_ns_total":1042,"hold_ns_max":1042},"project_cache":{"acquisitions":20,"wait_ns_total":4126,"wait_ns_max":708,"hold_ns_total":20415,"hold_ns_max":2667}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'p(95)<150' p(95)=6.26ms
✓ 'p(99)<300' p(99)=7.93ms
✓ 'p(95)<150' p(95)=4.92ms
✓ 'p(99)<300' p(99)=5.96ms
✓ 'p(95)<200' p(95)=11.27ms
✓ 'p(99)<400' p(99)=11.73ms
✓ 'p(95)<200' p(95)=31.76ms
✓ 'p(99)<400' p(99)=39.65ms
✓ 'p(95)<250' p(95)=10.84ms
✓ 'p(99)<500' p(99)=10.84ms
✓ 'p(95)<100' p(95)=26.1ms
✓ 'p(99)<250' p(99)=38.81ms
✓ 'rate==0' rate=0.00%
✓ credentials answered
✓ ingest 200
✓ every event stored
✓ audit page answered
✓ audit page is a page
✓ controls answered
✓ denied answered
✓ revoke answered
```
