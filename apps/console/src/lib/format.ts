// Formatting only. No value is ever invented: missing data stays visibly missing.

export function time(iso: string | null | undefined): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	const pad = (n: number, w = 2) => String(n).padStart(w, '0');
	return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(
		d.getUTCHours()
	)}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}.${pad(d.getUTCMilliseconds(), 3)}Z`;
}

/** Microseconds as a short duration. */
export function micros(us: number | null | undefined): string {
	if (us === null || us === undefined) return '—';
	const sign = us < 0 ? '−' : '';
	const a = Math.abs(us);
	if (a < 1_000) return `${sign}${a} µs`;
	if (a < 1_000_000) return `${sign}${(a / 1_000).toFixed(1)} ms`;
	return `${sign}${(a / 1_000_000).toFixed(2)} s`;
}

/** An estimate in a currency, from millionths of a unit. */
export function money(microsAmount: number, currency: string | null): string {
	if (currency === 'MIXED') return `${microsAmount} µ (mixed currencies)`;
	const amount = microsAmount / 1_000_000;
	return `${amount.toFixed(amount < 0.01 ? 6 : 4)} ${currency ?? ''}`.trim();
}

export function short(id: string, n = 8): string {
	return id.length > n ? `${id.slice(0, n)}…` : id;
}

export function value(v: unknown): string {
	return typeof v === 'string' ? v : JSON.stringify(v);
}

const STATE_TEXT: Record<string, string> = {
	succeeded: 'succeeded',
	failed: 'failed',
	uncertain: 'outcome unknown',
	conflicting: 'conflicting outcomes',
	no_finish_observed: 'no finish observed'
};

export function stateText(state: string): string {
	return STATE_TEXT[state] ?? state;
}
