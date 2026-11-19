# Ingestion run: smoke-preflight (2026-11-19)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `edcebbd-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 23.67 37.01 37.13 |
| … after ingest / after query | 24.76 36.60 36.98 / 17.43 33.69 35.91 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 30 batches/s × 1 operation(s) × 10 events for 15s |

## Ingest (`perf/k6/ingest.js`, exit 0)

| Metric | Value |
|---|---|
| Batches (iterations) | 451 |
| Events sent | 4510 |
| Events accepted (k6 count) | 4510 |
| **Events lost** (sent − stored, per answer) | 0 |
| Batches rate-limited / shed | 0 / 0 |
| Dropped iterations (k6 could not offer the load) | 0 |
| HTTP failures (rate) | 0 |
| Latency p50 / p95 / p99 / max (ms) | 5.21 / 9.09 / 15.61 / 34.11 |
| Achieved request rate (/s) | 30.04 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 4510 |
| Operations stored | 451 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 2908160 bytes (645 bytes/event) |
| `operations` table + indexes | 344064 bytes |
| Database total | 11859647 bytes |
| Collector RSS after the runs | unavailable (process tools denied here) |
| Collector counters | `{"batches":451,"events_accepted":4510,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":452,"wait_ns_total":20734,"wait_ns_max":291,"hold_ns_total":195301,"hold_ns_max":1792},"names_shared":{"acquisitions":451,"wait_ns_total":12631,"wait_ns_max":292,"hold_ns_total":582077,"hold_ns_max":99834},"names_exclusive":{"acquisitions":1,"wait_ns_total":41,"wait_ns_max":41,"hold_ns_total":3084,"hold_ns_max":3084},"ingest_key_cache":{"acquisitions":454,"wait_ns_total":96843,"wait_ns_max":583,"hold_ns_total":335709,"hold_ns_max":2208},"ingest_rate_limit":{"acquisitions":451,"wait_ns_total":23290,"wait_ns_max":375,"hold_ns_total":171821,"hold_ns_max":3042},"token_cache":{"acquisitions":4669,"wait_ns_total":910529,"wait_ns_max":41167,"hold_ns_total":3899612,"hold_ns_max":74875},"query_rate_limit":{"acquisitions":4661,"wait_ns_total":535683,"wait_ns_max":13083,"hold_ns_total":1365316,"hold_ns_max":58959},"project_cache":{"acquisitions":4660,"wait_ns_total":558314,"wait_ns_max":3708,"hold_ns_total":1820261,"hold_ns_max":28916}}}` |

## Query over what was stored (`perf/k6/query.js`, exit 0, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 4.13 / 8.3 / 15.04 |
| List-by-request p50 / p95 / p99 (ms) | 1.77 / 3.73 / 7.15 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |

## First failures (verbatim — status 0 means the request never left k6)

```

```

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'count==0' count=0
✓ 'p(95)<100' p(95)=9.08ms
✓ 'p(99)<250' p(99)=15.61ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✓ 'p(95)<500' p(95)=2.94ms
✓ 'p(95)<150' p(95)=4.09ms
✓ 'p(95)<150' p(95)=3.73ms
✓ 'p(95)<150' p(95)=8.29ms
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
✓ filtered 200
✓ compare 200
```
