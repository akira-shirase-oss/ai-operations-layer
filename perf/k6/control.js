// Control-surface load (A4): the whole control path under its own bucket and pool,
// while ingest runs concurrently at its reference rate — the invariant under test is
// that a flood of control READS never starves the write path (`events_lost == 0`,
// ingest p99 inside the ingest scenario's own gate).
//
// The mix is what an incident actually looks like, weighted per iteration:
//   audit        ~40% — GET /v1/admin/audit, the console's page walk: cursor-following,
//                        filters rotating, restarting at the top when a page ends
//   controls     ~30% — GET …/operations/{op}/controls, recovery reads on live ops
//   credentials  ~15% — GET /v1/admin/credentials
//   denied       ~10% — a read-scoped token on an admin route: an audited 403, the
//                        cheap-and-honest write share of every denial
//   revoke        ~5% — POST …/tokens/{id}/revoke on a pooled id: allowed once per id,
//                        an audited 404 after (a real revoke race reads the same)
//
// Every call sits under its caller's control bucket (5 rps/token): the run mints
// enough tokens that the offered rate is the surface's honest sustained capacity
// inside its own limits — never an unlimited one.
//
//   k6 run -e BASE_URL=… -e ADMINS=t1,t2,… -e READERS=r1,r2,… -e REVOKE_IDS=u1,u2,… \
//          -e INGEST_KEY=… -e RUN_ID=<8 hex> -e RATE=300 -e INGEST_RATE=100 \
//          -e DURATION=3400s perf/k6/control.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check, sleep } from 'k6';
import { Counter } from 'k6/metrics';
import { hex, note, operation } from './ops.js';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const ADMINS = (__ENV.ADMINS || '').split(',').filter(Boolean);
const READERS = (__ENV.READERS || '').split(',').filter(Boolean);
const REVOKE_IDS = (__ENV.REVOKE_IDS || '').split(',').filter(Boolean);
const INGEST_KEY = __ENV.INGEST_KEY;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const PROJECT = __ENV.PROJECT || 'perf';
const RATE = Number(__ENV.RATE || 20);
const INGEST_RATE = Number(__ENV.INGEST_RATE || 20);
const DURATION = __ENV.DURATION || '30s';
const OPS_PER_BATCH = Number(__ENV.OPS_PER_BATCH || 1);
const OPERATIONS = Number(__ENV.OPERATIONS || 10000);

const sent = new Counter('events_sent');
const lost = new Counter('events_lost');
const auditPages = new Counter('audit_pages_walked');
const controlReqs = new Counter('control_requests');
// 429 is the control bucket's designed back-pressure, not a defect: expected like
// the 403/404s, counted here so the result file shows the throttled share instead
// of hiding it inside the error rate.
const limited = new Counter('rate_limited');

export const options = {
	// A VU's connection must not idle past the collector's 10 s header
	// bound, whose idle close races a reuse (D-035): connections retire at
	// each iteration, and the intra-iteration gaps are the poll sleeps.
	noVUConnectionReuse: true,
	scenarios: {
		control: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || Math.max(ADMINS.length, 16)),
			maxVUs: Number(__ENV.MAX_VUS || 512),
		},
		ingest: {
			executor: 'constant-arrival-rate',
			rate: INGEST_RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.INGEST_PRE_VUS || 50),
			maxVUs: Number(__ENV.INGEST_MAX_VUS || 400),
			exec: 'ingest',
		},
	},
	thresholds: {
		'http_req_duration{name:audit}': ['p(95)<150', 'p(99)<300'],
		'http_req_duration{name:controls}': ['p(95)<150', 'p(99)<300'],
		'http_req_duration{name:credentials}': ['p(95)<200', 'p(99)<400'],
		'http_req_duration{name:denied}': ['p(95)<200', 'p(99)<400'],
		'http_req_duration{name:revoke}': ['p(95)<250', 'p(99)<500'],
		// The invariant: the write path holds its own latency under the read flood.
		'http_req_duration{scenario:ingest}': ['p(95)<100', 'p(99)<250'],
		events_lost: ['count==0'],
		http_req_failed: ['rate==0'],
		checks: ['rate==1'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

// The log must not be empty when the walk starts — and a denied read is itself a
// row, so seeding it exercises exactly what the run measures.
export function setup() {
	if (!ADMINS.length || !READERS.length || !INGEST_KEY) {
		throw new Error('ADMINS, READERS and INGEST_KEY are required');
	}
	// Round-robin the reader tokens and pace the seeding — a burst on one token
	// hits the 5 rps control bucket and a 429 writes no denial row.
	for (let i = 0; i < 80; i++) {
		const r = http.get(`${BASE_URL}/v1/admin/audit`, {
			headers: { authorization: `Bearer ${READERS[i % READERS.length]}` },
			responseCallback: http.expectedStatuses(403, 429),
		});
		note('setup-denial', r, [403, 429]);
		sleep(0.05);
	}
}

function admin() {
	return ADMINS[exec.vu.idInTest % ADMINS.length];
}
function reader() {
	return READERS[exec.vu.idInTest % READERS.length];
}

// The audit page walk's per-VU state: a cursor and which filter is active. A walk
// turns one page per iteration — what a person in the console does — and restarts
// at the top on a new filter once the log's tail is reached.
const AUDIT_FILTERS = ['', 'action=audit.list', 'action=credentials.list', 'actor=ops'];
let auditCursor = null;
let auditFilter = 0;

function auditPage() {
	let path = `${BASE_URL}/v1/admin/audit?limit=100`;
	const f = AUDIT_FILTERS[auditFilter];
	if (f) path += `&${f}`;
	if (auditCursor) {
		path += `&before_time=${encodeURIComponent(auditCursor[0])}&before_id=${auditCursor[1]}`;
	}
	const r = http.get(path, {
		headers: { authorization: `Bearer ${admin()}` },
		responseCallback: http.expectedStatuses(200, 429),
		tags: { name: 'audit' },
	});
	check(r, {
		'audit page answered': (x) => x.status === 200 || x.status === 429,
		'audit page is a page': (x) => x.status !== 200 || Array.isArray(x.json().audit),
	});
	note('audit', r, [200, 429]);
	const next = r.status === 200 ? r.json().next : null;
	if (r.status === 200) auditPages.add(1);
	if (next) {
		auditCursor = next;
	} else {
		auditCursor = null;
		auditFilter = (auditFilter + 1) % AUDIT_FILTERS.length;
	}
	return r;
}

function controlsRead() {
	const n = Math.floor(Math.random() * OPERATIONS);
	const op = RUN_ID + hex(n, 24);
	const r = http.get(`${BASE_URL}/v1/projects/${PROJECT}/operations/${op}/controls`, {
		headers: { authorization: `Bearer ${reader()}` },
		responseCallback: http.expectedStatuses(200, 429),
		tags: { name: 'controls' },
	});
	note('controls', r, [200, 429]);
	check(r, {
		'controls answered': (x) => x.status === 429 || (x.status === 200 && Array.isArray(x.json().controls)),
	});
	return r;
}

function credentialsRead() {
	const r = http.get(`${BASE_URL}/v1/admin/credentials`, {
		headers: { authorization: `Bearer ${admin()}` },
		responseCallback: http.expectedStatuses(200, 429),
		tags: { name: 'credentials' },
	});
	note('credentials', r, [200, 429]);
	check(r, {
		'credentials answered': (x) => x.status === 429 || (x.status === 200 && Array.isArray(x.json().tokens)),
	});
	return r;
}

function deniedRead() {
	// A read-scoped token on an admin route: refused, and the refusal is audited —
	// the log keeps growing while the run reads it, the live case.
	const path = Math.random() < 0.5 ? '/v1/admin/audit' : '/v1/admin/credentials';
	const r = http.get(`${BASE_URL}${path}`, {
		headers: { authorization: `Bearer ${reader()}` },
		responseCallback: http.expectedStatuses(403, 429),
		tags: { name: 'denied' },
	});
	note('denied', r, [403, 429]);
	check(r, { 'denied answered': (x) => x.status === 403 || x.status === 429 });
	return r;
}

let revokeAt = 0;
function revoke() {
	const id = REVOKE_IDS[revokeAt++ % REVOKE_IDS.length];
	const r = http.post(`${BASE_URL}/v1/admin/credentials/tokens/${id}/revoke`, '{}', {
		headers: {
			authorization: `Bearer ${admin()}`,
			'content-type': 'application/json',
		},
		// First pass over the pool grants (200, audited 'revoked'); every pass after
		// denies (404, audited 'unknown_or_revoked'). Both are honest outcomes.
		responseCallback: http.expectedStatuses(200, 404, 429),
		tags: { name: 'revoke' },
	});
	note('revoke', r, [200, 404, 429]);
	check(r, { 'revoke answered': (x) => x.status === 200 || x.status === 404 || x.status === 429 });
	return r;
}

export default function () {
	const p = Math.random() * 100;
	const r =
		p < 40 ? auditPage()
		: p < 70 ? controlsRead()
		: p < 85 ? credentialsRead()
		: p < 95 ? deniedRead()
		: REVOKE_IDS.length ? revoke()
		: deniedRead();
	controlReqs.add(1);
	if (r.status === 429) limited.add(1);
}

// The concurrent write path (same shape and envelope as perf/k6/ingest.js): if a
// control-read flood starves it, the events_lost counter and the scenario's own
// latency gate both see it.
export function ingest() {
	const i = exec.scenario.iterationInTest;
	const now = Date.now();
	let events = [];
	for (let j = 0; j < OPS_PER_BATCH; j++) {
		events = events.concat(operation(i * OPS_PER_BATCH + j, now, RUN_ID).events);
	}
	const body = JSON.stringify({
		schema_version: 1,
		source: {
			instance_id: RUN_ID + hex(exec.vu.idInTest, 24),
			service: 'k6-load',
			sdk: 'k6/control.js',
		},
		events,
	});
	const res = http.post(`${BASE_URL}/v1/ingest`, body, {
		headers: { authorization: `Bearer ${INGEST_KEY}`, 'content-type': 'application/json' },
		tags: { name: 'ingest' },
	});
	sent.add(events.length);
	note('ingest', res, [200]);
	let stored = 0;
	if (res.status === 200) {
		const answer = res.json();
		stored = answer.accepted + answer.duplicates;
	}
	lost.add(events.length - stored);
	check(res, {
		'ingest 200': (r) => r.status === 200,
		'every event stored': () => stored === events.length,
	});
}
