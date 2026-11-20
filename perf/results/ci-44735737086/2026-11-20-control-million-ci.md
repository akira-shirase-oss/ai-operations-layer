# Control-surface run: million-ci (2026-11-20)

Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity).

| | |
|---|---|
| Commit | `8928bdf` |
| Machine | Intel(R) Xeon(R) 6975P-C, 4 cores, 15 GB, Linux 7.0.0-1014-aws x86_64 |
| Collector | release build, one process, PostgreSQL at 127.0.0.1:5432 (external, AIOPS_PG_URL), 16 ingest connections |
| CI run | https://github.com/akira-shirase-oss/ai-operations-layer/actions/runs/44735737086 |
| Load average (1/5/15 min) before | 3.57, 1.82, 0.73 |
| … after | 1.24, 1.17, 1.12 |
| Load generator | k6 v2.3.0 on the same machine (open model: constant arrival rate) |
| Offered load | 300 control req/s for 3400s + 100 ingest batches/s concurrently |
| Credentials | 79 admin tokens, 17 read tokens, a 600-id revoke pool |

## The mix (`perf/k6/control.js`, exit 99)

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | 0.94 | 3.22 | 3.98 | 14.79 |
| controls (recovery reads) | 0.36 | 2.39 | 3.08 | 12.75 |
| credentials | 5.41 | 8.2 | 10.54 | 33.57 |
| denied (audited 403) | 0.49 | 2.55 | 3.26 | 11.81 |
| revoke (allowed→audited 404) | 0.76 | 2.99 | 4.11 | 13.39 |

| Metric | Value |
|---|---|
| Requests (total) | 1360082 |
| Control requests (rate achieved) | 299.64 |
| Rate-limited by the control bucket (429) | 122314 |
| Audit pages walked | 407527 |
| Checks passed (rate) | 1.0 |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | 0.0 |
| Dropped iterations | 0 |

## The invariant — ingest concurrent, never starved (scenario `ingest`)

| Metric | Value |
|---|---|
| Events sent | 3400010 |
| **Events lost** | 330 |
| Ingest p50 / p95 / p99 (ms) | 1.28 / 3.73 / 4.96 |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | 3399680 |
| Operations stored | 339968 |
| Audit rows (the log the walk read) | 122443 |
| … denials among them | 121843 |
| Tokens revoked by the run | 600 |
| Collector RSS after the run | 20076 KiB |
| Collector counters | `{"batches":339968,"events_accepted":3399680,"events_duplicate":0,"events_refused":0,"events_conflicting":0,"attributes_withheld":0,"redactions":0,"unauthenticated":0,"shed":0,"rate_limited":122314,"denied":71482,"store_unavailable":0,"auth_shed":0,"locks":{"names_map":{"acquisitions":339969,"wait_ns_total":29309045,"wait_ns_max":391793,"hold_ns_total":95466194,"hold_ns_max":43307},"names_shared":{"acquisitions":339968,"wait_ns_total":20966284,"wait_ns_max":19731,"hold_ns_total":312563257,"hold_ns_max":332107},"names_exclusive":{"acquisitions":1,"wait_ns_total":45,"wait_ns_max":45,"hold_ns_total":5566,"hold_ns_max":5566},"ingest_key_cache":{"acquisitions":340197,"wait_ns_total":41648098,"wait_ns_max":88787,"hold_ns_total":252917033,"hold_ns_max":265133},"ingest_rate_limit":{"acquisitions":339968,"wait_ns_total":22867904,"wait_ns_max":112550,"hold_ns_total":104024470,"hold_ns_max":209359},"token_cache":{"acquisitions":1037884,"wait_ns_total":131149365,"wait_ns_max":271258,"hold_ns_total":1200054384,"hold_ns_max":2747794},"query_rate_limit":{"acquisitions":1,"wait_ns_total":31,"wait_ns_max":31,"hold_ns_total":1741,"hold_ns_max":1741},"project_cache":{"acquisitions":214493,"wait_ns_total":22290116,"wait_ns_max":125430,"hold_ns_total":116921178,"hold_ns_max":203649}}}` |

## First failures (verbatim — status 0 means the request never left k6)

```
time="2026-11-20T12:34:14Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:34:14Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:34:14Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:39:14Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:39:15Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:39:15Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:44:15Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:49:15Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:49:15Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
time="2026-11-20T12:49:16Z" level=info msg="AIOPS_FIRST_FAILURE {\"op\":\"ingest\",\"status\":0,\"error\":\"EOF\",\"body\":\"\"}" source=console
```

## Threshold verdicts

```
✗ 'rate==1' rate=99.99%
✗ 'count==0' count=330
✓ 'p(95)<150' p(95)=3.21ms
✓ 'p(99)<300' p(99)=3.98ms
✓ 'p(95)<150' p(95)=2.39ms
✓ 'p(99)<300' p(99)=3.07ms
✓ 'p(95)<200' p(95)=8.2ms
✓ 'p(99)<400' p(99)=10.53ms
✓ 'p(95)<200' p(95)=2.54ms
✓ 'p(99)<400' p(99)=3.25ms
✓ 'p(95)<250' p(95)=2.98ms
✓ 'p(99)<500' p(99)=4.11ms
✓ 'p(95)<100' p(95)=3.72ms
✓ 'p(99)<250' p(99)=4.95ms
✗ 'rate==0' rate=0.00%
✓ revoke answered
✓ audit page answered
✓ audit page is a page
✓ controls answered
✗ ingest 200
↳  99% — ✓ 339968 / ✗ 33
✗ every event stored
↳  99% — ✓ 339968 / ✗ 33
✓ denied answered
✓ credentials answered
```
