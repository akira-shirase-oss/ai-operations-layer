# Ingestion run: queryrun2 (2026-11-20)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `e40a1d6-dirty` |
| Machine | Darwin arm64 — sysctl unavailable here (a sandbox); CPU/RAM not recorded |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:57432 (external, AIOPS_PG_URL), 16 ingest connections |
| Load average (1/5/15 min) before | 7.46 11.57 24.32 |
| … after ingest / after query | 6.51 11.02 23.75 / 29.42 15.60 24.94 |
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
| Latency p50 / p95 / p99 / max (ms) | 5.1 / 9.15 / 11.21 / 21.57 |
| Achieved request rate (/s) | 30.04 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 6010 |
| Operations stored | 601 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 3817472 bytes (635 bytes/event) |
| `operations` table + indexes | 417792 bytes |
| Database total | 12867263 bytes |
| Collector RSS after the runs | unavailable (process tools denied here) |
| Collector counters | `{"batches":601,"events_accepted":6010,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":602,"wait_ns_total":123343,"wait_ns_max":541,"hold_ns_total":487657,"hold_ns_max":21750},"names_shared":{"acquisitions":601,"wait_ns_total":151331,"wait_ns_max":62833,"hold_ns_total":1349499,"hold_ns_max":29875},"names_exclusive":{"acquisitions":1,"wait_ns_total":0,"wait_ns_max":0,"hold_ns_total":2750,"hold_ns_max":2750},"ingest_key_cache":{"acquisitions":604,"wait_ns_total":240352,"wait_ns_max":8584,"hold_ns_total":1258358,"hold_ns_max":98875},"ingest_rate_limit":{"acquisitions":601,"wait_ns_total":142280,"wait_ns_max":9583,"hold_ns_total":472643,"hold_ns_max":6459},"token_cache":{"acquisitions":4643,"wait_ns_total":854088,"wait_ns_max":52792,"hold_ns_total":4802012,"hold_ns_max":145375},"query_rate_limit":{"acquisitions":4631,"wait_ns_total":599257,"wait_ns_max":10500,"hold_ns_total":1358515,"hold_ns_max":17208},"project_cache":{"acquisitions":4630,"wait_ns_total":489292,"wait_ns_max":18708,"hold_ns_total":2114849,"hold_ns_max":74625}}}` |

## Query over what was stored (`perf/k6/query.js`, exit 0, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 4.54 / 69.52 / 96.31 |
| List-by-request p50 / p95 / p99 (ms) | 2.07 / 34.83 / 50.85 |
| Filtered list (status+release) p50 / p95 / p99 (ms) | 2.67 / 34.38 / 56.84 |
| Release comparison p50 / p95 / p99 (ms) | 1.83 / 18.65 / 28.13 |
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
✓ 'p(95)<100' p(95)=9.15ms
✓ 'p(99)<250' p(99)=11.2ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✓ 'p(95)<500' p(95)=18.65ms
✓ 'p(95)<150' p(95)=34.37ms
✓ 'p(95)<150' p(95)=34.83ms
✓ 'p(95)<150' p(95)=69.51ms
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
✓ filtered 200
✓ compare 200
```
