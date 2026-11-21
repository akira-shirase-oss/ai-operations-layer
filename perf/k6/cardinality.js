// High-cardinality attack at scale (the evidence plan's cardinality run).
// Every batch carries two operations of the reference shape:
//
//   one under a name never seen before (unique per iteration — the attack:
//   unbounded distinct names must neither grow the registry nor cost the
//   honest traffic), and
//   one under the fixed name `publish_article` (the honest traffic that must
//   keep landing whole).
//
// The collector's budget answer is the measurement: names past the project's
// cap are stored as `_overflow` and counted in the response's
// `overflowed_names` — bounded, labeled, never a silent drop. The gates:
//
//   events_unaccounted == 0   (every event stored or refused by name)
//   checks rate == 1          (every batch answered 200)
//   overflowed_names_seen > 0 (the budget provably engaged — a run that never
//                              overflowed measured nothing)
//   the honest ops lose nothing (events_honest_lost == 0)
//
// Afterwards the runner asserts `known_names` stayed inside the project cap
// and the honest ops are complete in the store.
//
//   k6 run -e BASE_URL=… -e INGEST_KEY=… -e RUN_ID=<8 hex> \
//          -e RATE=50 -e DURATION=120s perf/k6/cardinality.js
import http from 'k6/http';
import exec from 'k6/execution';
import { check } from 'k6';
import { Counter } from 'k6/metrics';
import { EVENTS_PER_OP, hex, note, operation } from './ops.js';

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:57410';
const KEY = __ENV.INGEST_KEY;
const RUN_ID = (__ENV.RUN_ID || '0000beef').padStart(8, '0').slice(0, 8);
const RATE = Number(__ENV.RATE || 50);
const DURATION = __ENV.DURATION || '120s';
const HONEST = 'publish_article';

const sent = new Counter('events_sent');
const stored = new Counter('events_stored');
const refused = new Counter('events_refused');
const unstored = new Counter('events_unstored');
const unaccounted = new Counter('events_unaccounted');
const overflowed = new Counter('overflowed_names_seen');
const honestLost = new Counter('events_honest_lost');
const retried = new Counter('batches_retried');

export const options = {
	// A VU's connection must not idle past the collector's 10 s header
	// bound, whose idle close races a reuse (D-035): connections retire at
	// each iteration.
	noVUConnectionReuse: true,
	scenarios: {
		cardinality: {
			executor: 'constant-arrival-rate',
			rate: RATE,
			timeUnit: '1s',
			duration: DURATION,
			preAllocatedVUs: Number(__ENV.PRE_VUS || 30),
			maxVUs: Number(__ENV.MAX_VUS || 200),
		},
	},
	thresholds: {
		events_unaccounted: ['count==0'],
		events_unstored: ['count==0'],
		events_honest_lost: ['count==0'],
		overflowed_names_seen: ['count>0'],
		checks: ['rate==1'],
	},
	summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
	const i = exec.scenario.iterationInTest;
	const now = Date.now();
	// The attack op's name is unique per iteration; the honest op's is the
	// registry's first citizen. Both ops get distinct operation ids by n.
	const attack = operation(i * 2, now, RUN_ID, `flood-${RUN_ID}-${hex(i, 20)}`);
	const honest = operation(i * 2 + 1, now, RUN_ID, HONEST);
	const events = attack.events.concat(honest.events);
	const body = JSON.stringify({
		schema_version: 1,
		source: {
			instance_id: RUN_ID + hex(exec.vu.idInTest, 24),
			service: 'k6-load',
			sdk: 'k6/cardinality.js',
		},
		events,
	});
	const params = {
		headers: { authorization: `Bearer ${KEY}`, 'content-type': 'application/json' },
		tags: { name: 'ingest' },
	};
	let res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	if (res.status === 0) {
		note('cardinality', res, [200]);
		retried.add(1);
		res = http.post(`${BASE_URL}/v1/ingest`, body, params);
	}
	const n = events.length;
	sent.add(n);
	if (res.status === 200) {
		const answer = res.json();
		const storedN = answer.accepted + answer.duplicates;
		const refusals = answer.refused || [];
		stored.add(storedN);
		refused.add(refusals.length);
		overflowed.add(answer.overflowed_names || 0);
		if (storedN + refusals.length !== n) unaccounted.add(n - storedN - refusals.length);
		// The honest op's events sit at indices EVENTS_PER_OP..n-1: a
		// refusal there means the attack cost the honest traffic an event.
		honestLost.add(refusals.filter((r) => r.index >= EVENTS_PER_OP).length);
	} else {
		unstored.add(n);
		honestLost.add(EVENTS_PER_OP);
	}
	note('cardinality', res, [200]);
	check(res, { '200': (r) => r.status === 200 });
}
