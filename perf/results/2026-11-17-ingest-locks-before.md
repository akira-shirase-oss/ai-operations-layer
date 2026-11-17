# Ingestion run: locks-before (2026-11-17)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `4d0dcf7` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average (1/5/15 min) before | 34.56 25.92 26.30 |
| … after ingest / after query | 29.10 25.51 26.14 / 30.94 26.40 26.44 |
| Collector | release build, one process, PostgreSQL 18.6 (Docker, local stack), 16 ingest connections |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 500 batches/s × 1 operation(s) × 10 events for 30s |

## Ingest (`perf/k6/ingest.js`, exit 99)

| Metric | Value |
|---|---|
| Batches (iterations) | 14988 |
| Events sent | 149880 |
| Events accepted (k6 count) | 149880 |
| **Events lost** (sent − stored, per answer) | 0 |
| Batches rate-limited / shed | 0 / 0 |
| Dropped iterations (k6 could not offer the load) | 12 |
| HTTP failures (rate) | 0 |
| Latency p50 / p95 / p99 / max (ms) | 1.83 / 8.4 / 60.69 / 146.52 |
| Achieved request rate (/s) | 499.58 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 149880 |
| Operations stored | 14988 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 93044736 bytes (621 bytes/event) |
| `operations` table + indexes | 7561216 bytes |
| Database total | 109868735 bytes |
| Collector RSS after the runs | 11792 KiB |
| Collector counters | `{"batches":14988,"events_accepted":149880,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":0,"locks":{"names":{"acquisitions":29980,"wait_ns_total":1532580,"wait_ns_max":120250,"hold_ns_total":34099219,"hold_ns_max":300500},"ingest_key_cache":{"acquisitions":15000,"wait_ns_total":1049571,"wait_ns_max":15750,"hold_ns_total":4309972,"hold_ns_max":30542},"ingest_rate_limit":{"acquisitions":14988,"wait_ns_total":875907,"wait_ns_max":64833,"hold_ns_total":1664808,"hold_ns_max":19125},"token_cache":{"acquisitions":3011,"wait_ns_total":554026,"wait_ns_max":39000,"hold_ns_total":3000902,"hold_ns_max":95084},"query_rate_limit":{"acquisitions":3003,"wait_ns_total":348816,"wait_ns_max":5125,"hold_ns_total":1119167,"hold_ns_max":37875},"project_cache":{"acquisitions":3003,"wait_ns_total":389923,"wait_ns_max":15125,"hold_ns_total":1265254,"hold_ns_max":14875}}}` |

## Query over what was stored (`perf/k6/query.js`, exit 0, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 4.78 / 69.73 / 111.69 |
| List-by-request p50 / p95 / p99 (ms) | 1.96 / 28.91 / 57.08 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✗ 'count==0' count=12
✓ 'count==0' count=0
✓ 'p(95)<100' p(95)=8.4ms
✓ 'p(99)<250' p(99)=60.68ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✓ 'p(95)<150' p(95)=28.91ms
✓ 'p(95)<150' p(95)=69.73ms
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
```
