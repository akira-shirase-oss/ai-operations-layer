# Ingestion run: locks-after (2026-11-17)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `0c420a1` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average (1/5/15 min) before | 28.88 26.11 26.34 |
| … after ingest / after query | 32.64 27.30 26.76 / 25.38 26.11 26.35 |
| Collector | release build, one process, PostgreSQL 18.6 (Docker, local stack), 16 ingest connections |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 500 batches/s × 1 operation(s) × 10 events for 30s |

## Ingest (`perf/k6/ingest.js`, exit 99)

| Metric | Value |
|---|---|
| Batches (iterations) | 14980 |
| Events sent | 149800 |
| Events accepted (k6 count) | 149470 |
| **Events lost** (sent − stored, per answer) | 330 |
| Batches rate-limited / shed | 0 / 33 |
| Dropped iterations (k6 could not offer the load) | 20 |
| HTTP failures (rate) | 0.0 |
| Latency p50 / p95 / p99 / max (ms) | 3.5 / 64.81 / 120.87 / 211.13 |
| Achieved request rate (/s) | 499.32 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 149470 |
| Operations stored | 14947 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 92979200 bytes (622 bytes/event) |
| `operations` table + indexes | 7528448 bytes |
| Database total | 109729471 bytes |
| Collector RSS after the runs | 16480 KiB |
| Collector counters | `{"batches":14947,"events_accepted":149470,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":33,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":14951,"wait_ns_total":883114,"wait_ns_max":1417,"hold_ns_total":4061354,"hold_ns_max":702125},"names_shared":{"acquisitions":14947,"wait_ns_total":451958,"wait_ns_max":4334,"hold_ns_total":8370915,"hold_ns_max":88750},"names_exclusive":{"acquisitions":1,"wait_ns_total":0,"wait_ns_max":0,"hold_ns_total":3917,"hold_ns_max":3917},"ingest_key_cache":{"acquisitions":14992,"wait_ns_total":2274782,"wait_ns_max":698375,"hold_ns_total":7874166,"hold_ns_max":155375},"ingest_rate_limit":{"acquisitions":14947,"wait_ns_total":1235265,"wait_ns_max":30875,"hold_ns_total":3396050,"hold_ns_max":74250},"token_cache":{"acquisitions":3009,"wait_ns_total":406503,"wait_ns_max":3791,"hold_ns_total":2567794,"hold_ns_max":39250},"query_rate_limit":{"acquisitions":3001,"wait_ns_total":249169,"wait_ns_max":4834,"hold_ns_total":1152963,"hold_ns_max":20250},"project_cache":{"acquisitions":3001,"wait_ns_total":266809,"wait_ns_max":5292,"hold_ns_total":1315343,"hold_ns_max":13208}}}` |

## Query over what was stored (`perf/k6/query.js`, exit 99, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 3.85 / 8.98 / 21.77 |
| List-by-request p50 / p95 / p99 (ms) | 1.61 / 4.53 / 8.96 |
| Checks passed (rate) | 1.0 |
| HTTP failures (rate) | 0.0 |

## Threshold verdicts

```
✗ 'rate==1' rate=99.77%
✗ 'count==0' count=20
✗ 'count==0' count=330
✓ 'p(95)<100' p(95)=64.81ms
✓ 'p(99)<250' p(99)=120.86ms
✗ 'rate==0' rate=0.22%
✗ 200
↳  99% — ✓ 14947 / ✗ 33
✗ every event stored
↳  99% — ✓ 14947 / ✗ 33
✗ 'rate==1' rate=99.91%
✓ 'p(95)<150' p(95)=4.52ms
✓ 'p(95)<150' p(95)=8.98ms
✗ 'rate==0' rate=0.06%
✗ timeline 200
↳  99% — ✓ 1499 / ✗ 2
✗ ten events, nothing missing
↳  99% — ✓ 1499 / ✗ 2
✓ list 200
```
