# Drill: PostgreSQL restarts during ingest (2026-11-17, rerun) — PASS

Development observation on a shared laptop (review finding 8).

| | |
|---|---|
| Commit | `724cee8` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1; load average before: 30.07 31.05 31.98 |
| Load | k6, 100 batches/s × 10 events for 30 s, against the release collector |
| Restart | `docker compose restart postgres` from 02:14:16 to 02:14:19 UTC |
| Batches | 2985 |
| Answered 503 (retryable) | 184 |
| **Answered another 5xx** | **0** |
| Events sent / acknowledged by the collector | 29850 / 28010 |
| **Events in the database afterwards** | **28010** (≥ acknowledged; a 503 batch may still have committed — re-sending it is idempotent) |
| "internal error" lines in the collector log | 0 |
| `/readyz` after the run (no collector restart) | 200 |
