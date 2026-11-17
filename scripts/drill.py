#!/usr/bin/env python3
"""The diagnosis drill kit (docs/DRILL.md, D-017, D-024, D-028). Standard library only.

  scripts/drill.py seed  --mode baseline|layer [--host reference|durable-runner]
                         [--seed N] [--background 200]
      reference (the kit's dry-run host): builds and starts the reference host
      (instrumented only in layer mode), sends background traffic plus six incidents
      drawn from the catalog.
      durable-runner (the workflow that decides the ship gate, D-024): starts the
      uninstrumented article worker and Durable Runner (scripts/durable-runner.sh); in
      layer mode the runner calls the adapter, which forwards to the worker. Submits
      background jobs plus six incidents and waits until every run is terminal.
      Either way writes drill-runs/<run>/tickets.md and seals the answer key (its
      SHA-256 is printed in the tickets). The processes keep running for diagnosis.
  scripts/drill.py stop  <run>
  scripts/drill.py score <run> <answers.json> --performer NAME [--seeder-aware]
      Compares answers with the sealed key and writes perf/results/drill/<run>.md.

answers.json: [{"ticket": 1, "class": "...", "recovery": "...", "actions": 7, "seconds": 95}, ...]
"""

import argparse
import base64
import hashlib
import json
import os
import random
import secrets
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUNS = ROOT / "drill-runs"
RESULTS = ROOT / "perf" / "results" / "drill"

# class -> (fault, customer-visible symptom, the one correct recovery choice)
CATALOG = {
    "model_retry_ok": ("model_transport_once", "It took a while, but I think it went through?", "none"),
    "validation_failed": ("invalid_draft", "My article never appeared.", "resubmit"),
    "publish_lost_response": ("publish_lost_response", "I got an error page. Did my article publish or not?", "reconcile_first"),
    "publish_rejected": ("publish_rejected", "My article never appeared.", "resubmit"),
    "tool_timeout": ("tool_timeout", "It spun for a while and then showed an error.", "resubmit"),
    "model_exhausted": ("model_timeout_all", "It spun for a long time and then showed an error.", "resubmit"),
}
RECOVERIES = ["none", "resubmit", "reconcile_first"]
INCIDENTS = 6

# The same classes and recovery choices on Durable Runner (D-028): class -> (the worker's
# fault, what a customer of a background job says, the one correct recovery choice).
# "resubmit" there means a new job under a new op_key (an op_key returns its first run).
DR_CATALOG = {
    "model_retry_ok": ("model_retry_ok", "It took longer than usual, but it seems to be there?", "none"),
    "validation_failed": ("validation_failed", "My article never appeared.", "resubmit"),
    "publish_lost_response": ("publish_lost_response", "It took very long. Did my article publish, and only once?", "reconcile_first"),
    "publish_rejected": ("publish_rejected", "My article never appeared.", "resubmit"),
    "tool_timeout": ("tool_timeout", "It was stuck for a while and then failed.", "resubmit"),
    "model_exhausted": ("model_exhausted", "It was stuck for a long time and then failed.", "resubmit"),
}
DR_STEPS = ["generate", "validate", "factcheck", "publish"]


def post(url, body, request_id):
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "x-request-id": request_id},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


def wait_up(url, seconds=30):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1):
                return
        except Exception:
            time.sleep(0.2)
    sys.exit(f"host did not start at {url}")


def seed(args):
    if args.host == "durable-runner":
        return seed_durable_runner(args)
    rng = random.Random(args.seed if args.seed is not None else secrets.randbits(32))
    run = f"{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}-{args.mode}"
    run_dir = RUNS / run
    (run_dir / "sealed").mkdir(parents=True)
    env = dict(os.environ)
    env.update(
        REFHOST_LISTEN=f"127.0.0.1:{args.port}",
        REFHOST_ALLOW_FAULTS="1",
        REFHOST_LOG_FILE=str(run_dir / "host.log"),
        REFHOST_MODEL_LATENCY_MS="20",
        REFHOST_TIMEOUT_MS="300",
    )
    if args.mode == "baseline":
        env.pop("AIOPS_ENDPOINT", None)
        env.pop("AIOPS_INGEST_KEY", None)
    elif not (env.get("AIOPS_ENDPOINT") and env.get("AIOPS_INGEST_KEY")):
        sys.exit("layer mode needs AIOPS_ENDPOINT and AIOPS_INGEST_KEY")
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "-p", "aiops-reference-host"],
        cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    host = subprocess.Popen(
        [str(ROOT / "target" / "release" / "reference-host")],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
    )
    (run_dir / "host.pid").write_text(str(host.pid))
    base = f"http://127.0.0.1:{args.port}"
    wait_up(f"{base}/healthz")

    topics = ["durable queues", "vector search", "retry budgets", "idempotency keys", "clock skew",
              "backpressure", "schema evolution", "rate limiting", "event sourcing", "cost control"]
    # D-024: every run has at least one lost publish response; the rest are drawn with
    # repetition, and the order is shuffled.
    classes = ["publish_lost_response"] + [rng.choice(sorted(CATALOG)) for _ in range(INCIDENTS - 1)]
    rng.shuffle(classes)
    positions = sorted(rng.sample(range(args.background), INCIDENTS))
    incidents = dict(zip(positions, classes))
    key, tickets = [], []
    for i in range(args.background):
        rid = f"req-{rng.getrandbits(64):016x}"
        fault = None
        cls = incidents.get(i)
        if cls:
            fault = CATALOG[cls][0]
        elif rng.random() < 0.05:
            fault = "model_transport_once"  # unticketed noise: retries that worked
        status = post(f"{base}/articles", {"topic": rng.choice(topics), "fault": fault}, rid)
        if cls:
            n = len(tickets) + 1
            tickets.append((n, rid, CATALOG[cls][1]))
            key.append({"ticket": n, "request_id": rid, "class": cls, "recovery": CATALOG[cls][2], "http_status": status})
    sealed = run_dir / "sealed" / "answer-key.json"
    sealed.write_text(json.dumps({"run": run, "mode": args.mode, "key": key}, indent=2))
    digest = hashlib.sha256(sealed.read_bytes()).hexdigest()
    lines = [
        f"# Drill tickets — run {run} ({args.mode})",
        "",
        f"Answer key sealed: `sealed/answer-key.json`, SHA-256 `{digest}`. Do not open it.",
        "",
        "Available: the host log (`host.log`), the publisher (`GET "
        f"{base}/publications/<business_key>`, business key = `article:<request id>`)"
        + (", and the console." if args.mode == "layer" else "."),
        "",
        f"For each ticket write: class (one of {', '.join(sorted(CATALOG))}), recovery "
        f"(one of {', '.join(RECOVERIES)}), search actions, seconds.",
        "",
        "| # | Request ID | Customer says |",
        "|---|---|---|",
    ]
    lines += [f"| {n} | `{rid}` | {symptom} |" for n, rid, symptom in tickets]
    (run_dir / "tickets.md").write_text("\n".join(lines) + "\n")
    print(f"run {run}: {run_dir}/tickets.md (key sha256 {digest}); host pid {host.pid} on {base}")


def request(url, body=None, key=None, timeout=10):
    headers = {"content-type": "application/json"}
    if key:
        headers["authorization"] = f"Bearer {key}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers, method="POST" if data else "GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read() or b"null")


def seed_durable_runner(args):
    rng = random.Random(args.seed if args.seed is not None else secrets.randbits(32))
    run = f"{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}-dr-{args.mode}"
    run_dir = RUNS / run
    (run_dir / "sealed").mkdir(parents=True)
    env = dict(os.environ)
    if args.mode == "layer" and not (env.get("AIOPS_ENDPOINT") and env.get("AIOPS_INGEST_KEY")):
        sys.exit("layer mode needs AIOPS_ENDPOINT and AIOPS_INGEST_KEY")
    server = subprocess.run([str(ROOT / "scripts" / "durable-runner.sh")], cwd=ROOT, check=True,
                            capture_output=True, text=True).stdout.strip()
    cli = str(Path(server).with_name("durable-cli"))
    build = ["cargo", "build", "--release", "--locked", "-p", "aiops-dr-article-worker", "-p", "aiops-durable-runner"]
    subprocess.run(build, cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    dr_port, adapter_port, worker_port = args.dr_port, args.dr_port + 1, args.dr_port + 2
    api_key, read_key = secrets.token_hex(32), secrets.token_hex(32)
    (run_dir / "sealed" / "api-key").write_text(api_key)  # the operator's key, not the performer's
    env.update(DURABLE_API_KEY=api_key, DURABLE_READ_KEY=read_key,
               WORKER_LISTEN=f"127.0.0.1:{worker_port}", WORKER_HANG_MS="3000")
    pids = []

    def start(cmd, log=None):
        out = open(log, "wb") if log else subprocess.DEVNULL
        p = subprocess.Popen(cmd, env=env, stdout=out, stderr=out, start_new_session=True)
        pids.append(p.pid)
        (run_dir / "pids").write_text("\n".join(map(str, pids)))
        return p

    # The worker writes no log of its own (as Durable Runner's pilot handler ships).
    start([str(ROOT / "target" / "release" / "dr-article-worker")])
    wait_up(f"http://127.0.0.1:{worker_port}/healthz")
    if args.mode == "layer":
        cfg = {
            "listen": f"127.0.0.1:{adapter_port}",
            "runner": {"label": "dr-drill", "url": f"http://127.0.0.1:{dr_port}", "step_timeout_ms": 1000},
            "workflow": "publish_article",
            "routes": [
                {"route": "generate", "kind": "model", "upstream": f"http://127.0.0.1:{worker_port}/generate",
                 "model": "fake-model-1", "price": {"input_micros_per_token": 1, "output_micros_per_token": 4,
                                                    "currency": "USD", "basis": "fake-list-price-2026-11"}},
                {"route": "validate", "kind": "validation", "upstream": f"http://127.0.0.1:{worker_port}/validate"},
                {"route": "factcheck", "kind": "tool", "upstream": f"http://127.0.0.1:{worker_port}/factcheck"},
                {"route": "publish", "kind": "tool", "external_effect": True,
                 "upstream": f"http://127.0.0.1:{worker_port}/publish"},
            ],
        }
        (run_dir / "sealed" / "adapter.json").write_text(json.dumps(cfg))
        env["AIOPS_DR_ADAPTER_CONFIG"] = str(run_dir / "sealed" / "adapter.json")
        start([str(ROOT / "target" / "release" / "aiops-dr-adapter")])
        wait_up(f"http://127.0.0.1:{adapter_port}/healthz")
        target = f"http://127.0.0.1:{adapter_port}/h"
    else:
        target = f"http://127.0.0.1:{worker_port}"
    handlers = [a for h in DR_STEPS for a in ("--handler", f"{h}={target}/{h}")]
    # Durable Runner's own stderr is its shipped log: the baseline's log evidence (D-024).
    start([server, "--journal", str(run_dir / "journal"), "--listen", f"127.0.0.1:{dr_port}",
           "--step-timeout-ms", "1000", *handlers], log=run_dir / "durable.log")
    base = f"http://127.0.0.1:{dr_port}"
    wait_up(f"{base}/health")

    topics = ["durable queues", "vector search", "retry budgets", "idempotency keys", "clock skew",
              "backpressure", "schema evolution", "rate limiting", "event sourcing", "cost control"]
    classes = ["publish_lost_response"] + [rng.choice(sorted(DR_CATALOG)) for _ in range(INCIDENTS - 1)]
    rng.shuffle(classes)
    incidents = dict(zip(sorted(rng.sample(range(args.background), INCIDENTS)), classes))
    key, tickets, runs = [], [], []
    for i in range(args.background):
        rid = f"req-{rng.getrandbits(64):016x}"
        cls = incidents.get(i)
        fault = DR_CATALOG[cls][0] if cls else ("model_retry_ok" if rng.random() < 0.05 else None)
        job = {"topic": rng.choice(topics), "fault": fault}
        op_key = f"article/{rid}"
        spec = {"version": 1, "steps": [{"name": h, "handler": h} for h in DR_STEPS],
                "input_b64": base64.b64encode(json.dumps(job).encode()).decode()}
        run_id = request(f"{base}/jobs", {"op_key": op_key, "spec": spec}, api_key)["run_id"]
        runs.append(run_id)
        if cls:
            n = len(tickets) + 1
            tickets.append((n, op_key, DR_CATALOG[cls][1]))
            key.append({"ticket": n, "request_id": op_key, "run_id": run_id, "class": cls,
                        "recovery": DR_CATALOG[cls][2]})
    deadline = time.time() + 180
    pending = set(runs)
    while pending and time.time() < deadline:
        for r in list(pending):
            state = request(f"{base}/jobs/{r}", key=read_key)["state"]
            if state == "done" or state == "cancelled" or state.startswith("failed:"):
                pending.discard(r)
        time.sleep(0.5)
    if pending:
        sys.exit(f"{len(pending)} runs were not terminal after 180 s")
    sealed = run_dir / "sealed" / "answer-key.json"
    sealed.write_text(json.dumps({"run": run, "mode": args.mode, "host": "durable-runner", "key": key}, indent=2))
    digest = hashlib.sha256(sealed.read_bytes()).hexdigest()
    (run_dir / "read-key").write_text(read_key)
    lines = [
        f"# Drill tickets — run {run} (Durable Runner, {args.mode})",
        "",
        f"Answer key sealed: `sealed/answer-key.json`, SHA-256 `{digest}`. Do not open `sealed/`.",
        "",
        "Available, as the existing setup ships (D-024):",
        f"- Durable Runner's log: `durable.log`; its runs: `DURABLE_READ_KEY=$(cat read-key) {cli} "
        f"--server {base} list --newest` and `… inspect --run <id>`.",
        f"- The publisher: `GET http://127.0.0.1:{worker_port}/publications/<op_key>`.",
        "- The article worker writes no log." + (" The console." if args.mode == "layer" else ""),
        "",
        f"For each ticket write: class (one of {', '.join(sorted(DR_CATALOG))}), recovery (one of "
        f"{', '.join(RECOVERIES)}; resubmit = a new job under a new op_key), search actions, seconds.",
        "",
        "| # | Request (op_key) | Customer says |",
        "|---|---|---|",
    ]
    lines += [f"| {n} | `{k}` | {symptom} |" for n, k, symptom in tickets]
    (run_dir / "tickets.md").write_text("\n".join(lines) + "\n")
    print(f"run {run}: {run_dir}/tickets.md (key sha256 {digest}); runner on {base}")


def stop(args):
    run_dir = RUNS / args.run
    files = [run_dir / "pids", run_dir / "host.pid"]
    pids = [int(p) for f in files if f.exists() for p in f.read_text().split()]
    for pid in pids:
        try:
            os.killpg(pid, 15)
        except OSError as e:
            print(f"{pid}: not running ({e})")
    print(f"stopped {len(pids)} process group(s)")


def score(args):
    run_dir = RUNS / args.run
    sealed = run_dir / "sealed" / "answer-key.json"
    digest = hashlib.sha256(sealed.read_bytes()).hexdigest()
    printed = (run_dir / "tickets.md").read_text()
    if digest not in printed:
        sys.exit("the answer key changed after the tickets were written")
    key = {k["ticket"]: k for k in json.loads(sealed.read_text())["key"]}
    answers = {a["ticket"]: a for a in json.loads(Path(args.answers).read_text())}
    rows, actions, seconds, correct = [], [], [], 0
    for n, k in sorted(key.items()):
        a = answers.get(n, {})
        class_ok = a.get("class") == k["class"]
        recovery_ok = a.get("recovery") == k["recovery"]
        ok = class_ok and recovery_ok
        correct += ok
        actions.append(a.get("actions", 0))
        seconds.append(a.get("seconds", 0))
        rows.append(
            f"| {n} | {k['class']} | {a.get('class', '—')} | {k['recovery']} | {a.get('recovery', '—')} | "
            f"{'yes' if ok else 'NO'} | {a.get('actions', '—')} | {a.get('seconds', '—')} |"
        )
    sealed_doc = json.loads(sealed.read_text())
    mode = sealed_doc["mode"] + (f", {sealed_doc['host']}" if sealed_doc.get("host") else "")
    machine = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    label = "seeder-aware (does not count toward the ship gate, D-017)" if args.seeder_aware else "independent"
    RESULTS.mkdir(parents=True, exist_ok=True)
    out = RESULTS / f"{args.run}.md"
    out.write_text("\n".join([
        f"# Diagnosis drill — {args.run} ({mode})",
        "",
        f"Protocol: `docs/DRILL.md`. Performer: {args.performer} — {label}. Commit `{commit}`, {machine}.",
        f"Answer key SHA-256 `{digest}` (matches the tickets).",
        "",
        "| # | Key class | Answer class | Key recovery | Answer recovery | Correct | Actions | Seconds |",
        "|---|---|---|---|---|---|---|---|",
        *rows,
        "",
        f"**Correct: {correct}/{len(key)}. Median search actions per ticket: {statistics.median(actions)}. "
        f"Median seconds per ticket: {statistics.median(seconds)}.**",
        "",
        args.notes or "",
    ]) + "\n")
    print(f"wrote {out}: {correct}/{len(key)} correct, median actions {statistics.median(actions)}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("seed")
    s.add_argument("--mode", choices=["baseline", "layer"], required=True)
    s.add_argument("--seed", type=int)
    s.add_argument("--background", type=int, default=200)
    s.add_argument("--port", type=int, default=57401)
    s.add_argument("--host", choices=["reference", "durable-runner"], default="reference")
    s.add_argument("--dr-port", type=int, default=53342, help="runner; adapter and worker on the next two")
    s.set_defaults(func=seed)
    t = sub.add_parser("stop")
    t.add_argument("run")
    t.set_defaults(func=stop)
    c = sub.add_parser("score")
    c.add_argument("run")
    c.add_argument("answers")
    c.add_argument("--performer", required=True)
    c.add_argument("--seeder-aware", action="store_true")
    c.add_argument("--notes")
    c.set_defaults(func=score)
    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
