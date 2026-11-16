# Ingestion run: smoke-2 (2026-11-16)

Development observation on a shared laptop, **not** production capacity. Same shape as
`2026-11-16-ingest-smoke.md`, after commit `a3f7cf1` (project lookups cached, query pool
4 → 8). The machine was even busier (load average ~69 on 12 cores). The query path went
from timeline p95 1.04 s to 24 ms and list p95 692 ms to 9.9 ms, and its thresholds pass.
Ingest stored every event, but its p95 still misses the 100 ms target; on a machine this
loaded that cannot be separated from scheduling noise, so a quiet-machine run is owed
before the ingest path is optimized further.

| | |
|---|---|
| Commit | `a3f7cf1` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average (1/5/15 min) before | 69.07 56.76 65.15 |
| … after ingest / after query | 75.32 59.09 65.68 / 62.64 57.83 64.96 |
| Collector | release build, one process, PostgreSQL 18.6 (Docker, local stack), 16 ingest connections |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 100 batches/s × 1 operation(s) × 10 events for 30s |

## Ingest (`perf/k6/ingest.js`, exit 99)

| Metric | Value |
|---|---|
| Batches (iterations) | 3001 |
| Events sent | 30010 |
| Events accepted (k6 count) | 30010 |
| **Events lost** (sent − stored, per answer) | 0 |
| Batches rate-limited / shed | 0 / 0 |
| Dropped iterations (k6 could not offer the load) | 0 |
| HTTP failures (rate) | 0 |
| Latency p50 / p95 / p99 / max (ms) | 12.7 / 153.1 / 238.66 / 333.81 |
| Achieved request rate (/s) | 99.04 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 30010 |
| Operations stored | 3001 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 18513920 bytes (617 bytes/event) |
| `operations` table + indexes | 1613824 bytes |
| Database total | 28833471 bytes |
| Collector RSS after the runs | 12208 KiB |
| Collector counters | `{"batches":3001,"events_accepted":30010,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":0,"denied":0,"store_unavailable":0}` |

## Query over what was stored (`perf/k6/query.js`, exit 0, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 4.34 / 23.96 / 59.06 |
| List-by-request p50 / p95 / p99 (ms) | 2.12 / 9.86 / 22.43 |
| Checks passed (rate) | 1 |
| HTTP failures (rate) | 0 |

## Threshold verdicts

```
✓ 'rate==1' rate=100.00%
✓ 'count==0' count=0
✓ 'count==0' count=0
✗ 'p(95)<100' p(95)=153.1ms
✓ 'p(99)<250' p(99)=238.65ms
✓ 'rate==0' rate=0.00%
✓ 200
✓ every event stored
✓ 'rate==1' rate=100.00%
✓ 'p(95)<150' p(95)=9.86ms
✓ 'p(95)<150' p(95)=23.96ms
✓ 'rate==0' rate=0.00%
✓ timeline 200
✓ ten events, nothing missing
✓ list 200
```
