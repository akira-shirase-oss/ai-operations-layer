# Drill: PostgreSQL restarts during ingest (2026-11-17) — FAIL (the collector did not recover)

Development observation on a shared laptop (review finding 8).

| | |
|---|---|
| Commit | `e16ea6e-dirty` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1; load average before: 22.89 27.34 37.15 |
| Load | k6, 100 batches/s × 10 events for 30 s, against the release collector |
| Restart | `docker compose restart postgres` from 02:00:53 to 02:00:55 UTC |
| Batches | 1007 |
| Answered 503 (retryable) | 191 |
| **Answered another 5xx** | **0** |
| Events sent / acknowledged by the collector | 10070 / 8160 |
| **Events in the database afterwards** | **8160** (≥ acknowledged; a 503 batch may still have committed — re-sending it is idempotent) |
| "internal error" lines in the collector log | 0 |
| `/readyz` after the run (no collector restart) | 503 |
