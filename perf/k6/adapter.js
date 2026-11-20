// perf/k6/adapter.js — Durable Runner adapter + authority-read load (A2).
//
// Two constant-arrival-rate scenarios share the run:
//   submit    — POST {runner}/jobs with the real four-step spec, then poll the
//               runner's own GET /jobs/{run} until the run is terminal (a run
//               travels runner -> adapter /h/{step} -> worker -> telemetry, so
//               the adapter carries real proxied-call + reporting load).
//   authority — read-token GET {base}/v1/projects/perf/operations?limit=N, pick
//               an op, GET .../authority; each read is proxied by the collector
//               to the runner's GET /jobs/{run} (D-027).
//
// Answers, not inferences: run state comes from the runner's own journal, an
// 'unavailable' authority answer is truthful load data (counted, not failed).
// Loss counters live in the adapter's GET /stats and the runner's result file.

import http from 'k6/http';
import { check, sleep } from 'k6';
import { Counter, Trend } from 'k6/metrics';
import encoding from 'k6/encoding';
import { note } from './ops.js';

const runner = __ENV.RUNNER_URL; // durable-server, e.g. http://127.0.0.1:57440
const drApi = __ENV.DR_API; // runner submit key
const drRead = __ENV.DR_READ; // runner read key
const base = __ENV.BASE_URL; // collector, e.g. http://127.0.0.1:57410
const token = __ENV.TOKEN; // collector read-scope token
const project = __ENV.PROJECT || 'perf';
const runId = __ENV.RUN_ID || 'local';
const rate = Number(__ENV.RATE || '20'); // job submissions /s
const authRate = Number(__ENV.AUTH_RATE || '40'); // authority reads /s
const duration = __ENV.DURATION || '300s';

const steps = ['generate', 'validate', 'factcheck', 'publish'];

const submitted = new Counter('jobs_submitted');
const runsDone = new Counter('runs_done');
const runsFailed = new Counter('runs_failed');
const runSeconds = new Trend('run_seconds', true);
const opsListed = new Counter('ops_listed');
const authAnswered = new Counter('authority_answered');
const authUnavailable = new Counter('authority_unavailable');
const noOps = new Counter('ops_list_empty');

export const options = {
  scenarios: {
    submit: {
      executor: 'constant-arrival-rate',
      rate,
      timeUnit: '1s',
      duration,
      preAllocatedVUs: Math.min(64, rate * 2 + 8),
      maxVUs: 200,
      exec: 'submitJob',
    },
    authority: {
      executor: 'constant-arrival-rate',
      rate: authRate,
      timeUnit: '1s',
      duration,
      preAllocatedVUs: 12,
      maxVUs: 60,
      exec: 'authorityRead',
      startTime: '8s', // let the first ops land before reads start
    },
  },
  thresholds: {
    // The submit POST is the runner's own admission surface.
    'http_req_duration{endpoint:submit}': ['p(95)<500', 'p(99)<2000'],
    // End-to-end: submission -> four adapter-proxied steps -> journal 'done'.
    run_seconds: ['p(99)<5000'],
    // The collector's authority read includes its proxied runner call (2s deadline).
    'http_req_duration{endpoint:ops-list}': ['p(95)<300', 'p(99)<1000'],
    'http_req_duration{endpoint:authority}': ['p(95)<500', 'p(99)<2200'],
    http_req_failed: ['rate==0'],
    checks: ['rate==1'],
    jobs_submitted: ['count>0'],
    runs_failed: ['count==0'],
    dropped_iterations: ['count==0'],
  },
};

const drWrite = { 'content-type': 'application/json', authorization: `Bearer ${drApi}` };
const drReadH = { authorization: `Bearer ${drRead}` };
const collRead = { authorization: `Bearer ${token}` };

export function submitJob() {
  const key = `article/${runId}-v${__VU}i${__ITER}`;
  const spec = {
    version: 1,
    steps: steps.map((s) => ({ name: s, handler: s })),
    input_b64: encoding.b64encode(JSON.stringify({ topic: 'perf', op_key: key })),
  };
  const r = http.post(`${runner}/jobs`, JSON.stringify({ op_key: key, spec }), {
    headers: drWrite,
    timeout: '10s',
    tags: { endpoint: 'submit' },
  });
  const ok = check(r, { 'job accepted': (x) => x.status === 200 || x.status === 201 });
  if (!ok) {
    note('submit job', r, [200, 201]);
    return;
  }
  submitted.add(1);
  const rid = r.json('run_id');

  const t0 = Date.now();
  const deadline = t0 + 30000;
  let state = '';
  while (Date.now() < deadline) {
    const j = http.get(`${runner}/jobs/${rid}`, {
      headers: drReadH,
      timeout: '5s',
      tags: { endpoint: 'run-poll' },
    });
    note('poll run', j, [200]);
    state = (j.json() || {}).state || '';
    if (state === 'done' || state === 'cancelled' || state.startsWith('failed')) break;
    sleep(0.15);
  }
  runSeconds.add(Date.now() - t0);
  if (state === 'done') runsDone.add(1);
  else runsFailed.add(1);
  check(state, { 'run reached done': (s) => s === 'done' });
}

export function authorityRead() {
  const list = http.get(`${base}/v1/projects/${project}/operations?limit=3`, {
    headers: collRead,
    timeout: '10s',
    tags: { endpoint: 'ops-list' },
  });
  const okList = check(list, { 'ops listed': (x) => x.status === 200 });
  if (!okList) {
    note('ops list', list, [200]);
    return;
  }
  const ops = (list.json() || {}).operations || [];
  if (!ops.length) {
    noOps.add(1);
    return;
  }
  opsListed.add(1);
  const op = ops[Math.floor(Math.random() * ops.length)].operation_id;
  const a = http.get(`${base}/v1/projects/${project}/operations/${op}/authority`, {
    headers: collRead,
    timeout: '10s',
    tags: { endpoint: 'authority' },
  });
  const okA = check(a, { 'authority answered': (x) => x.status === 200 });
  if (!okA) {
    note('authority read', a, [200]);
    return;
  }
  const st = ((a.json() || {}).state) || '';
  if (st === 'answered') authAnswered.add(1);
  else authUnavailable.add(1); // unavailable/not_linked/not_configured — truthful answers
}
