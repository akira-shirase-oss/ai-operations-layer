# Drill: unauthenticated bodies (review N2) — 2026-11-17

- Commit: `9dfd625`
- Machine: Apple M2 Pro, 12 CPUs, 32 GiB
- Load average before / after: 34.67 38.19 32.74 / 34.86 38.09 32.77
- Shape: 256 connections, each with a well-shaped unknown ingest key and a 999,999-byte body sent except its last byte, held 8 s, then closed
- Release collector on 127.0.0.1:57412, defaults (2,048 connections, 1 MiB body cap, 32 MiB ingest body budget)

```
rss_before_mib=6.4
rss_during_mib=12.3
rss_after_mib=12.3
answer 239x: no answer in 5 s (BrokenPipeError)
answer 10x: HTTP/1.1 503 Service Unavailable
answer 6x: no answer in 5 s (ConnectionResetError)
answer 1x: HTTP/1.1 401 Unauthorized
healthz after: 200
```

**PASS** — pass means resident memory while the connections are open stays within 64 MiB
of the baseline (no bodies buffered) and `/healthz` answers afterwards. Answers: an unknown key
is refused from its headers (401, or 503 when the credential-lookup budget is spent); a client
still sending its body when the collector answers and closes usually sees a reset instead of the
answer (the collector does not linger to drain an unauthenticated body).

## Before the fix

The same drill on `9f4f6d4` (the commit before), load 53 / 49:

```
rss_before_mib=6.4
rss_during_mib=266.0
rss_after_mib=266.1
answer 256x: no answer in 5 s (TimeoutError)
healthz after: 200
```

**FAIL** — every unauthenticated body was read before its key was checked (about 1 MiB each), the
memory stayed after the connections closed, and no client was answered while it held its last byte.
