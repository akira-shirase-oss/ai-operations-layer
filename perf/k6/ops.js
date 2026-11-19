// The reference operation's event shape, shared by the load scenarios so a contract
// change lands in one place (docs/INGEST_CONTRACT.md, schema_version 1). Module
// state is per VU — k6 gives each VU its own JS runtime — so `seq` is one source
// instance's sequence, as the SDK keeps it.
export const EVENTS_PER_OP = 10;

let seq = 0;

export function hex(n, width) {
	return n.toString(16).padStart(width, '0').slice(-width);
}

function iso(ms) {
	// Fixed six fractional digits, like the SDK.
	return new Date(ms).toISOString().replace('Z', '000Z');
}

// One operation = ten events: operation.started, four steps started+finished,
// operation.finished. `op` is deterministic in (runId, n): control reads can name
// the same operation ingest creates.
export function operation(n, now, runId) {
	const op = runId + hex(n, 24);
	const step = (k) => hex(n * 8 + k + 1, 16);
	const base = (k, type) => {
		seq += 1;
		return {
			event_id: runId + hex(n * EVENTS_PER_OP + k, 24),
			type,
			operation_id: op,
			seq,
			op_seq: k + 1,
			event_time: iso(now - 50 + k),
		};
	};
	const steps = [
		['model', 'generate'],
		['validation', 'validate'],
		['tool', 'fact_check'],
		['tool', 'publish'],
	];
	const events = [
		Object.assign(base(0, 'operation.started'), {
			name: 'publish_article',
			request_id: `load-${runId}-${n}`,
			release: 'load-r1',
		}),
	];
	steps.forEach(([kind, name], i) => {
		events.push(
			Object.assign(base(1 + i * 2, 'step.started'), {
				step_id: step(i),
				kind,
				name,
				attempt: 1,
				external_effect: name === 'publish',
			}),
		);
		const finished = Object.assign(base(2 + i * 2, 'step.finished'), {
			step_id: step(i),
			attempt: 1,
			outcome: 'succeeded',
		});
		if (kind === 'model') {
			finished.usage = { input_tokens: 132, output_tokens: 480 };
			finished.cost_estimate = { micros: 2052, currency: 'USD', basis: 'fake-list-price-2026-11' };
		}
		events.push(finished);
	});
	events.push(
		Object.assign(base(9, 'operation.finished'), { outcome: 'succeeded', events_emitted: EVENTS_PER_OP }),
	);
	return { operation_id: op, events };
}
