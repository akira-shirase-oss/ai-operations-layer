# Control-surface run: million-ci (2026-11-19)

Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity).

| | |
|---|---|
| Commit | `9b058c9` |
| Machine | Intel(R) Xeon(R) 6975P-C, 4 cores, 15 GB, Linux 7.0.0-1014-aws x86_64 |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:5432 (external, AIOPS_PG_URL), 16 ingest connections |
| CI run | https://github.com/akira-shirase-oss/ai-operations-layer/actions/runs/44544883621 |
| Load average (1/5/15 min) before | 2.21, 1.41, 0.59 |
| … after | 0.22, 0.20, 0.14 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 300 control req/s for 3400s + 100 ingest batches/s concurrently |
| Credentials | 79 admin tokens, 17 read tokens, a 600-id revoke pool |

## The mix (`perf/k6/control.js`, exit 99)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 0 | 0 | 0 | 0 |
| controls (recovery reads) | 0 | 0 | 0 | 0 |
| credentials | 0 | 0 | 0 | 0 |
| denied (audited 403) | 0 | 0 | 0 | 0 |
| revoke (allowed→audited 404) | 0 | 0 | 0 | 0 |

| Metric | Value |
|---|---|
| Requests (total) | 1360081 |
| Control requests (rate achieved) | 299.64 |
| Rate-limited by the control bucket (429) | 0 |
| Audit pages walked | 0 |
| Checks passed (rate) | 0.19 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 1 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 3400010 |
| **Events lost** | 3400010 |
| Ingest p50 / p95 / p99 (ms) | 0 / 0 / 0 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 0 |
| Operations stored | 0 |
| Audit rows (the log the walk read) | 0 |
| … denials among them | 0 |
| Tokens revoked by the run | 0 |
| Collector RSS after the run | unavailable (process tools denied here) |
| Collector counters | `unavailable` |

## Threshold verdicts

```
✗ 'rate==1' rate=19.34%
✗ 'count==0' count=3400010
✓ 'p(95)<150' p(95)=0s
✓ 'p(99)<300' p(99)=0s
✓ 'p(95)<150' p(95)=0s
✓ 'p(99)<300' p(99)=0s
✓ 'p(95)<200' p(95)=0s
✓ 'p(99)<400' p(99)=0s
✓ 'p(95)<200' p(95)=0s
✓ 'p(99)<400' p(99)=0s
✓ 'p(95)<250' p(95)=0s
✓ 'p(99)<500' p(99)=0s
✓ 'p(95)<100' p(95)=0s
✓ 'p(99)<250' p(99)=0s
✗ 'rate==0' rate=100.00%
✗ ingest 200
↳  0% — ✓ 0 / ✗ 340001
✗ every event stored
↳  0% — ✓ 0 / ✗ 340001
✗ credentials answered
↳  0% — ✓ 0 / ✗ 152998
✗ denied answered
↳  0% — ✓ 0 / ✗ 101896
✗ controls answered
↳  0% — ✓ 0 / ✗ 306541
✗ audit page answered
↳  0% — ✓ 0 / ✗ 407678
✓ audit page is a page
✗ revoke answered
↳  0% — ✓ 0 / ✗ 50887
```
