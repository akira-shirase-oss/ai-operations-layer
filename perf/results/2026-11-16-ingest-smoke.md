# Ingestion run: smoke (2026-11-16)

Development observation on a shared laptop, **not** production capacity. The machine
was heavily loaded by other builds (load average 46–64 on 12 cores), so latency here says
more about the machine than the code. Ingest stored every event; the latency thresholds
missed. Kept as evidence, not re-run until green (the follow-up run is a separate file).

| | |
|---|---|
| Commit | `ae18159` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average (1/5/15 min) before | 46.08 46.57 64.50 |
| … after ingest / after query | 49.23 46.95 63.90 / 64.54 50.74 64.58 |
| Collector | release build, one process, PostgreSQL 18.6 (Docker, local stack), 16 ingest connections |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 100 batches/s × 1 operation(s) × 10 events for 30s |

## Ingest (`perf/k6/ingest.js`, exit 99)

| Metric | Value |
|---|---|
| Batches (iterations) | 3000 |
| Events sent | 30000 |
| Events accepted (k6 count) | 30000 |
| **Events lost** (sent − stored, per answer) | 0 |
| Batches rate-limited / shed | 0 / 0 |
| Dropped iterations (k6 could not offer the load) | 0 |
| HTTP failures (rate) | 0 |
| Latency p50 / p95 / p99 / max (ms) | 13.78 / 114.25 / 177.2 / 276.15 |
| Achieved request rate (/s) | 99.8 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 30000 |
| Operations stored | 3000 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 18472960 bytes (616 bytes/event) |
| `operations` table + indexes | 1564672 bytes |
| Database total | 28636863 bytes |
| Collector RSS after the runs | 12432 KiB |
| Collector counters | `{"batches":3000,"events_accepted":30000,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0}` |

## Query over what was stored (`perf/k6/query.js`, exit 99, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 347.33 / 1039.84 / 1143.72 |
| List-by-request p50 / p95 / p99 (ms) | 161.91 / 691.77 / 826.56 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'count==0' count=0
✗ 'p(95)<100' p(95)=114.24ms
✓ 'p(99)<250' p(99)=177.2ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✗ 'p(95)<150' p(95)=691.76ms
✗ 'p(95)<150' p(95)=1.03s
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
```
