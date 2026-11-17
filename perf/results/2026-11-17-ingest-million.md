# Ingestion run: million (2026-11-17)

Development observation on a shared laptop, **not** production capacity.

| | |
|---|---|
| Commit | `630f533` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average (1/5/15 min) before | 6.34 5.14 5.20 |
| … after ingest / after query | 27.02 13.98 8.76 / 23.58 14.38 9.08 |
| Collector | release build, one process, PostgreSQL 18.6 (Docker, local stack), 16 ingest connections |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 500 batches/s × 1 operation(s) × 10 events for 200s |

## Ingest (`perf/k6/ingest.js`, exit 99)

| Metric | Value |
|---|---|
| Batches (iterations) | 99963 |
| Events sent | 999630 |
| Events accepted (k6 count) | 963050 |
| **Events lost** (sent − stored, per answer) | 36580 |
| Batches rate-limited / shed | 0 / 3658 |
| Dropped iterations (k6 could not offer the load) | 38 |
| HTTP failures (rate) | 0.04 |
| Latency p50 / p95 / p99 / max (ms) | 2.56 / 152.2 / 228.77 / 408.77 |
| Achieved request rate (/s) | 499.78 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 963050 |
| Operations stored | 96305 |
| Operations not complete (≠ 10 events or no finish) | 0 |
| `events` table + indexes | 597794816 bytes (621 bytes/event) |
| `operations` table + indexes | 47931392 bytes |
| Database total | 656332479 bytes |
| Collector RSS after the runs | 11424 KiB |
| Collector counters | `{"batches":96305,"events_accepted":963050,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":3657,"rate_limited":0,"denied":0,"store_unavailable":0,"auth_shed":13}` |

## Query over what was stored (`perf/k6/query.js`, exit 99, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | 5.82 / 12.69 / 49.12 |
| List-by-request p50 / p95 / p99 (ms) | 2.5 / 6.24 / 21.17 |
| Checks passed (rate) | 0.97 |
| HTTP failures (rate) | 0.02 |

## Threshold verdicts

```
✗ 'rate==1' rate=96.34%
✗ 'count==0' count=38
✗ 'count==0' count=36580
✗ 'p(95)<100' p(95)=152.19ms
✓ 'p(99)<250' p(99)=228.77ms
✗ 'rate==0' rate=3.65%
✗ 200
↳  96% — ✓ 96305 / ✗ 3658
✗ every event stored
↳  96% — ✓ 96305 / ✗ 3658
✗ 'rate==1' rate=97.20%
✓ 'p(95)<150' p(95)=6.23ms
✓ 'p(95)<150' p(95)=12.68ms
✗ 'rate==0' rate=2.10%
✗ timeline 200
↳  95% — ✓ 1439 / ✗ 62
✗ ten events, nothing missing
↳  95% — ✓ 1439 / ✗ 62
✓ list 200
```

## Notes (added by hand, 2026-11-17)

**Verdict: miss, kept.** The thresholds failed: 3,658 batches (36,580 events) were shed,
so `events_lost` is 36,580 against `count==0`; ingest p95 152 ms against `p(95)<100`;
38 iterations dropped, so 999,630 of the planned 1,000,000 events were offered. Nothing
the collector accepted went missing: 963,050 events accepted and 963,050 stored, all
96,305 stored operations complete. The shed batches were refused with an answer, not
silently dropped. The query run then failed 62 of 1,501 timeline reads (2.1%).

**The machine was not quiet for most of the run.** It started at load 6.34/5.14/5.20
(the 5- and 15-minute averages under 6). About two minutes in, other work started on
the machine (a Go test suite in another repository, three test binaries at ~100% CPU
each, plus fseventsd). The 1-minute load reached 28 and the 5-minute 14. Samples
(UTC, 1/5/15 min), taken outside the script every minute:

| Time | Load |
|---|---|
| 11:25:19 | 6.64 5.18 5.22 |
| 11:26:19 | 6.71 5.49 5.33 |
| 11:27:19 | 15.88 8.49 6.47 |
| 11:28:19 | 28.01 12.96 8.25 |
| 11:29:15 (end) | 23.58 14.38 9.08 |

**An earlier attempt left no result.** The same command at 11:20:30–11:24:24 UTC (load
at most 7.91/5.03/5.17) ran both k6 stages, but the script stopped at its last step: the
collector-stats read got 403, because `67bc0de` moved that endpoint behind the admin
scope. Its exit trap then dropped the database and the k6 summaries, so none of its
numbers were recorded. `630f533` fixed the script (an admin-scope token for that one
read), and this file is the first run with it. Neither run is a capacity claim.
