import { expect, test, type Page } from '@playwright/test';

// The audit surface in a real browser against the real collector (scripts/console-e2e.sh
// seeds the stack and mints both tokens). The spec seeds its own row: a read-scope visit
// to /audit is denied, and denied reads are the ones the log records.

const TOKEN = process.env['AIOPS_E2E_TOKEN'] ?? '';
const ADMIN = process.env['AIOPS_E2E_ADMIN_TOKEN'] ?? '';

function watch(page: Page): string[] {
	const problems: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(m.text());
	});
	page.on('pageerror', (e) => problems.push(e.message));
	return problems;
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
	await signIn(page, TOKEN);
	await page.getByRole('link', { name: 'Audit' }).click();
	await expect(page.getByRole('heading', { name: 'Audit' })).toBeVisible();
	// The refusal is explained, not a blank page — and the call wrote the denial row.
	await expect(page.getByText(/needs the/)).toContainText('admin');
	expect(problems).toEqual([]);

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
	expect(problems).toEqual([]);
});

test('filters narrow the log and the empty state names them', async ({ page }) => {
	const problems = watch(page);
	await signIn(page, ADMIN);
	await page.getByRole('link', { name: 'Audit' }).click();
	await expect(page.getByTestId('audit-row').first()).toBeVisible();

	await page.getByPlaceholder('audit.list').fill('audit.list');
	await page.getByRole('button', { name: 'Apply' }).click();
	for (const row of await page.getByTestId('audit-row').all()) {
		await expect(row).toContainText('audit.list');
	}

	await page.getByPlaceholder('ops').fill('no-such-actor');
	await page.getByRole('button', { name: 'Apply' }).click();
	const empty = page.getByTestId('audit-empty');
	await expect(empty).toBeVisible();
	await expect(empty).toContainText('these filters');

	await page.getByRole('button', { name: 'Clear' }).click();
	await expect(page.getByTestId('audit-row').first()).toBeVisible();
	expect(problems).toEqual([]);
});
