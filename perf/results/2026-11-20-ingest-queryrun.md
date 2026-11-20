# Ingestion run: queryrun (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `e40a1d6` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 5.74 12.05 25.55 |
| … after ingest / after query | 10.16 12.64 25.44 / 8.86 12.09 24.80 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 30 batches/s × 1 operation(s) × 10 events for 20s |

## Ingest (`perf/k6/ingest.js`, exit 0)

| Metric | Value |
|---|---|
| Batches (iterations) | 601 |
| Events sent | 6010 |
| Events accepted (k6 count) | 6010 |
| **Events lost** (sent − stored, per answer) | 0 |
| Batches rate-limited / shed | 0 / 0 |
| Dropped iterations (k6 could not offer the load) | 0 |
| HTTP failures (rate) | 0 |
| Latency p50 / p95 / p99 / max (ms) | 7.0 / 18.48 / 25.73 / 37.1 |
| Achieved request rate (/s) | 30.04 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 6010 |
| Operations stored | 601 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 3809280 bytes (634 bytes/event) |
| `operations` table + indexes | 417792 bytes |
| Database total | 12834495 bytes |
| Collector RSS after the runs | unavailable (process tools denied here) |
| Collector counters | `{"batches":601,"events_accepted":6010,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":602,"wait_ns_total":57948,"wait_ns_max":458,"hold_ns_total":317424,"hold_ns_max":17791},"names_shared":{"acquisitions":601,"wait_ns_total":38631,"wait_ns_max":500,"hold_ns_total":942425,"hold_ns_max":8375},"names_exclusive":{"acquisitions":1,"wait_ns_total":125,"wait_ns_max":125,"hold_ns_total":2542,"hold_ns_max":2542},"ingest_key_cache":{"acquisitions":604,"wait_ns_total":155370,"wait_ns_max":959,"hold_ns_total":768510,"hold_ns_max":7875},"ingest_rate_limit":{"acquisitions":601,"wait_ns_total":99669,"wait_ns_max":666,"hold_ns_total":500700,"hold_ns_max":197666},"token_cache":{"acquisitions":4678,"wait_ns_total":636446,"wait_ns_max":5958,"hold_ns_total":3067771,"hold_ns_max":10166},"query_rate_limit":{"acquisitions":4668,"wait_ns_total":356257,"wait_ns_max":1542,"hold_ns_total":890697,"hold_ns_max":14625},"project_cache":{"acquisitions":4667,"wait_ns_total":465592,"wait_ns_max":834,"hold_ns_total":1260508,"hold_ns_max":7792}}}` |

## Query over what was stored (`perf/k6/query.js`, exit 0, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 3.31 / 5.23 / 6.95 |
| List-by-request p50 / p95 / p99 (ms) | 1.49 / 2.68 / 3.3 |
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
✓ 'p(95)<100' p(95)=18.47ms
✓ 'p(99)<250' p(99)=25.72ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✓ 'p(95)<500' p(95)=2.46ms
✓ 'p(95)<150' p(95)=3.34ms
✓ 'p(95)<150' p(95)=2.68ms
✓ 'p(95)<150' p(95)=5.22ms
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
✓ filtered 200
✓ compare 200
```
