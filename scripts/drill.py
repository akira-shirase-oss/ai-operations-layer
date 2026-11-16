#!/usr/bin/env python3
"""The diagnosis drill kit (docs/DRILL.md, D-017). Standard library only.

  scripts/drill.py seed  --mode baseline|layer [--seed N] [--background 200]
      Builds and starts the reference host (instrumented only in layer mode), sends
      background traffic plus six incidents drawn from the catalog, writes
      drill-runs/<run>/tickets.md and seals the answer key (its SHA-256 is printed in
      the tickets). The host keeps running so the publisher can be asked
      (GET /publications/<business_key>) during diagnosis.
  scripts/drill.py stop  <run>
  scripts/drill.py score <run> <answers.json> --performer NAME [--seeder-aware]
      Compares answers with the sealed key and writes perf/results/drill/<run>.md.

answers.json: [{"ticket": 1, "class": "...", "recovery": "...", "actions": 7, "seconds": 95}, ...]
"""

import argparse
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


def stop(args):
    pid_file = RUNS / args.run / "host.pid"
    try:
        os.kill(int(pid_file.read_text()), 2)
        print("stopped")
    except (OSError, ValueError) as e:
        print(f"not running: {e}")


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
    mode = json.loads(sealed.read_text())["mode"]
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
