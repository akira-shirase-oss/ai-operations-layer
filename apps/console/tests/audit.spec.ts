import { expect, test, type Page } from '@playwright/test';

// The audit surface in a real browser against the real collector (scripts/console-e2e.sh
// seeds the stack and mints both tokens). The spec seeds its own row: a read-scope visit
// to /audit is denied, and denied reads are the ones the log records.

const TOKEN = process.env['AIOPS_E2E_TOKEN'] ?? '';
const ADMIN = process.env['AIOPS_E2E_ADMIN_TOKEN'] ?? '';

function watch(page: Page): string[] {
	const problems: string[] = [];
	page.on('console', (m) => {
		// The location is kept with the text so an expected 4xx can be matched to
		// the exact endpoint that produced it — a blanket "contains 403" would
		// also forgive a real failure somewhere else.
		if (m.type() === 'error') problems.push(`${m.text()} (${m.location().url})`);
	});
	page.on('pageerror', (e) => problems.push(e.message));
	return problems;
}

// The browser logs a read token's expected 403 as a console error; nothing else may fail.
function unexpected(problems: string[]): string[] {
	return problems.filter((p) => !(p.includes('403 (Forbidden)') && p.includes('/v1/admin/audit')));
}

async function signIn(page: Page, t: string) {
	await page.goto('/console/');
	await page.getByPlaceholder('aiops_tk_…').fill(t);
	await page.getByRole('button', { name: 'Sign in' }).click();
	await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
}

async function signOut(page: Page) {
	await page.getByRole('button', { name: 'Sign out' }).click();
	await expect(page.getByPlaceholder('aiops_tk_…')).toBeVisible();
}

test('a read token is refused and the refusal itself lands in the log', async ({ page }) => {
	const problems = watch(page);
	// The refusal is one request, not a storm: a 403 feeding back into the fetch
	// loop once crashed this page with effect_update_depth_exceeded.
	let fetches = 0;
	await page.route('**/v1/admin/audit**', (r) => {
		fetches++;
		return r.continue();
	});
	await signIn(page, TOKEN);
	await page.getByRole('link', { name: 'Audit' }).click();
	await expect(page.getByRole('heading', { name: 'Audit' })).toBeVisible();
	// The refusal is explained, not a blank page — and the call wrote the denial row.
	await expect(page.getByText(/needs the/)).toContainText('admin');
	await page.waitForTimeout(500);
	expect(fetches, 'the audit page refetches in a loop').toBe(1);
	expect(unexpected(problems)).toEqual([]);
	await page.unroute('**/v1/admin/audit**');

	await signOut(page);
	await signIn(page, ADMIN);
	await page.getByRole('link', { name: 'Audit' }).click();
	const denial = page
		.getByTestId('audit-row')
		.filter({ hasText: 'audit.list' })
		.filter({ hasText: 'scope_missing' });
	await expect(denial.first()).toBeVisible();
	await expect(denial.first()).toContainText('denied');
	await expect(denial.first()).toContainText('e2e');
	// Metadata only: no secret or digest ever renders in the log.
	await expect(page.locator('tbody')).not.toContainText(/hash|secret|digest|aiops_tk_/);
	expect(unexpected(problems)).toEqual([]);
});

test('filters narrow the log and the empty state names them', async ({ page }) => {
	const problems = watch(page);
	let fetches = 0;
	await page.route('**/v1/admin/audit**', (r) => {
		fetches++;
		return r.continue();
	});
	await signIn(page, ADMIN);
	await page.getByRole('link', { name: 'Audit' }).click();
	await expect(page.getByTestId('audit-row').first()).toBeVisible();
	expect(fetches).toBe(1); // the load on arrival

	// Typing applies nothing: filters fire on Apply, Clear or Enter — a request
	// per keystroke is a read-scope user's request storm in miniature.
	fetches = 0;
	await page.getByPlaceholder('audit.list').pressSequentially('no-such-ac');
	await page.waitForTimeout(400);
	expect(fetches, 'typing fetched before Apply').toBe(0);

	await page.getByPlaceholder('audit.list').fill('audit.list');
	await page.getByRole('button', { name: 'Apply' }).click();
	for (const row of await page.getByTestId('audit-row').all()) {
		await expect(row).toContainText('audit.list');
	}
	expect(fetches, 'Apply did not fetch exactly once').toBe(1);

	await page.getByPlaceholder('ops').fill('no-such-actor');
	await page.getByPlaceholder('ops').press('Enter');
	const empty = page.getByTestId('audit-empty');
	await expect(empty).toBeVisible();
	await expect(empty).toContainText('these filters');

	await page.getByRole('button', { name: 'Clear' }).click();
	await expect(page.getByTestId('audit-row').first()).toBeVisible();
	expect(unexpected(problems)).toEqual([]);
});
