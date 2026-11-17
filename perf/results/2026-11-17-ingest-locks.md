# The ingest path's in-process locks, before and after (2026-11-17)

Development observations on a shared, heavily loaded laptop, **not** production
capacity. Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1; PostgreSQL 18.6 in Docker (local
stack). The machine's load average was 10–40 throughout (other projects' test suites),
so request latencies below are dominated by the machine. The lock counters are not:
they measure only the collector's own critical sections.

**What changed** (D-026). `4d0dcf7` timed every in-process lock. Ingest admitted each
batch's names under one mutex for all projects, held while each name was formatted
and looked up. `3c5c225` gives each project its own registry behind an `RwLock`.
Names are formatted before any lock is taken. A batch whose names are all known only
takes the registry shared. The exclusive lock is taken only for a name the project has
not seen.

## 1. In-process, at the in-flight cap (`crates/aiops-server/tests/locks.rs`)

The real router and the collector's six-pool store, on a multi-thread runtime.
Batches are 500 events (50 operations of the reference shape, so about 4 names per
event, all known after the warm-up), at concurrency 1, 16 and 64. 64 is the
collector's in-flight cap. Three runs each, every request answered 200. Raw lines are
in `perf/results/locks/`.

`cargo test -p aiops-server --release --test locks -- --ignored --nocapture measure_ingest_locks`

| | concurrency | requests | time waiting on name locks, total (3 runs) | worst single wait | exclusive takes |
|---|---|---|---|---|---|
| before `4d0dcf7` | 1 | 16 | 0.002–0.004 ms | 0.5 µs | 32 |
| | 16 | 64 | 0.195–7.010 ms | 170 µs–5.6 ms | 128 |
| | 64 | 128 | 6.5–27.5 ms | 2.2–8.0 ms | 256 |
| after `3c5c225` | 1 | 16 | 0.002–0.004 ms | 0.3 µs | 0 |
| | 16 | 64 | 0.008–0.012 ms | 0.5 µs | 0 |
| | 64 | 128 | 0.020–0.032 ms | 0.6–12 µs | 0 |

The registry was held for 25–35 µs per batch before (shared now: 7–12 µs, nobody
waits for it). Before, at 64 in flight, waiting for the registry was 0.03–0.11% of
the requests' total time (23–26 s across 128 requests). It was never the bottleneck:
the requests' p50 of about 200 ms is the 16-connection ingest pool and PostgreSQL. The
credential cache and the rate limiter hold their locks for under 1 µs and wait under
3 µs, so they stay as they are.

Request latencies are **not** compared across the two columns. The "after" runs
started at load 27–37 against 10–18 for "before", and their request times are 3–5×
worse for that reason alone (the name locks' own times fell).

An earlier attempt at this measurement ran under `#[sqlx::test]`. That uses a
current-thread runtime, where no lock can be contended. It measured nothing and is not
reported. Its first version also sent no `Content-Length`, so half of the 64 concurrent
requests were shed by the body budget, which charges the full 1 MiB cap to a body of
unknown length.

## 2. Short HTTP smokes (`perf/run-ingest.sh <name> 500 30s 1`)

The 1,000,000-event run's shape (500 batches/s × 10 events) for 30 s, release
collector, k6 on the same machine. One run each, kept as they came out. Files:
`2026-11-17-ingest-locks-before.md` (`4d0dcf7`) and `…-locks-after.md` (`0c420a1`,
which has `3c5c225`).

| | before `4d0dcf7` | after `0c420a1` |
|---|---|---|
| Load average at start | 34.56 | 28.88 |
| Events sent / stored | 149,880 / 149,880 | 149,800 / 149,470 |
| Shed with an answer (503) | 0 | 33 batches (330 events) |
| Dropped iterations | 12 | 20 |
| Ingest p50 / p95 / p99 (ms) | 1.83 / 8.4 / 60.7 | 3.5 / 64.8 / 120.9 |
| Name-lock acquisitions | 29,980 exclusive | 14,947 shared + 14,951 map, 1 exclusive |
| Time waiting on name locks, total | 1.53 ms | 1.33 ms |
| Worst single wait on a name lock | 120 µs | 4.3 µs |
| Name-lock hold, total | 34.1 ms | 12.4 ms |

With 10-event batches the registry was never contended enough to matter: 1.5 ms of
waiting across 15,000 requests. The "after" smoke was slower and shed 33 batches. The
lock counters rule the locks out as the cause (their waits and holds fell), so this
difference is the machine. Neither smoke says the ingest path is fast. Both miss the
thresholds the million run also missed, and that miss is still open. The lock work
removed a contention point that grows with batch size and concurrency (section 1), not
the cause of the million run's p95.

What this leaves for the next full run (the operator's): `GET /v1/collector/stats` now
carries `locks`, and `perf/run-ingest.sh` records it, so every run shows whether a lock
is in the way.
