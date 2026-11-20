// The console holds no authority: it is a client of the same permissioned API as
// everything else (AGENTS.md, frontend rules). The token lives in this tab's session
// storage and travels as a bearer header — no cookies, so no CSRF surface (D-015).

const TOKEN_KEY = 'aiops.token';

export function token(): string | null {
	try {
		return sessionStorage.getItem(TOKEN_KEY);
	} catch {
		return null;
	}
}

export function setToken(value: string | null): void {
	try {
		if (value) sessionStorage.setItem(TOKEN_KEY, value.trim());
		else sessionStorage.removeItem(TOKEN_KEY);
	} catch {
		// storage unavailable: the user signs in again next time
	}
}

/** Every failure state is explicit; none is rendered as an empty success. */
export type Result<T> =
	| { state: 'ok'; data: T }
	| { state: 'needsAuth' }
	| { state: 'forbidden'; message: string }
	| { state: 'missing' }
	| { state: 'rateLimited'; retryAfter: number }
	| { state: 'unreachable'; message: string };

export async function api<T>(path: string, fetcher: typeof fetch = fetch, as?: string): Promise<Result<T>> {
	return apiCall<T>(path, { method: 'GET' }, fetcher, as);
}

/** A JSON POST — control actions and admin revocations; same failure states. */
export async function apiPost<T>(
	path: string,
	body: unknown,
	fetcher: typeof fetch = fetch
): Promise<Result<T>> {
	return apiCall<T>(
		path,
		{
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(body ?? {})
		},
		fetcher
	);
}

async function apiCall<T>(
	path: string,
	init: RequestInit,
	fetcher: typeof fetch,
	as?: string
): Promise<Result<T>> {
	// `as` lets a caller authenticate without storing the token first — the
	// sign-in check runs on the candidate so sessionStorage only ever holds a
	// token the collector has already answered for.
	const t = as ?? token();
	if (!t) return { state: 'needsAuth' };
	let response: Response;
	try {
		const headers = { ...(init.headers ?? {}), authorization: `Bearer ${t}` };
		response = await fetcher(path, { ...init, headers });
	} catch (e) {
		// Whatever the rejection — a dead collector, a navigation cancelling the
		// fetch — it is a non-answer, never a verdict. How it is named differs by
		// browser and is not worth branching on; callers must treat any failure
		// here as "the collector did not answer", not "the collector refused".
		return { state: 'unreachable', message: e instanceof Error ? e.message : 'network error' };
	}
	if (response.ok) return { state: 'ok', data: (await response.json()) as T };
	let message = `HTTP ${response.status}`;
	try {
		const body = (await response.json()) as { error?: { message?: string } };
		if (body?.error?.message) message = body.error.message;
	} catch {
		// keep the status line
	}
	switch (response.status) {
		case 401:
			return { state: 'needsAuth' };
		case 403:
			return { state: 'forbidden', message };
		case 404:
			return { state: 'missing' };
		case 429:
			return { state: 'rateLimited', retryAfter: Number(response.headers.get('retry-after') ?? '1') };
		default:
			return { state: 'unreachable', message };
	}
}
