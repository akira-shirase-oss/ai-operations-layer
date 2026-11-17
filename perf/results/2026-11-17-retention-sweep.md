# Retention sweep throughput (2026-11-17)

Development observation on a shared laptop, not production capacity (review finding 5).

| | |
|---|---|
| Commit | `db1d1ed` |
| Machine | Apple M2 Pro, 12 cores, 32 GB, macOS 27.2.1 |
| Load average before / after | 28.12 52.32 54.00 / 31.20 51.84 53.80 |
| PostgreSQL | 18.6 in Docker (local stack), default settings plus `shared_buffers=256MB` |
| Rows | 1000000 expired (received 15 days ago) + 100000 fresh; loaded in 34 s |
| `events` before | 1100000 rows, 660512768 bytes |
| Sweep (`aiops-server sweep`: 5,000-row DELETEs, 20 per call, 50 ms between calls) | `{"events":1000000,"operations":0,"instances":0,"audit":0,"calls":11,"complete":true,"seconds":15.442}` |
| **Deleted per second** | **64758** |
| `events` after | 100000 rows; expired rows left: 0 |

The guide's reference load writes 1,000 events/s (3.6 million an hour). One scheduled run
now keeps deleting while expired rows remain, so the sweep keeps up as long as its
deletion rate exceeds the write rate; the rate above is the margin on this machine.
Before the fix a run deleted at most 100,000 rows per table and then waited an hour.
