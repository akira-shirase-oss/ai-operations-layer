import { expect, test, type Page } from '@playwright/test';

// The investigation view in a real browser against the real collector and reference host
// (scripts/console-e2e.sh seeds the data). Every page load is checked for CSP violations
// and script errors, and attacker-influenced text must render as text.

const TOKEN = process.env['AIOPS_E2E_TOKEN'] ?? '';
const LOST = process.env['AIOPS_E2E_LOST'] ?? '';
const OK = process.env['AIOPS_E2E_OK'] ?? '';
const XSS = process.env['AIOPS_E2E_XSS'] ?? '';
const DOWN = process.env['AIOPS_E2E_GAP'] ?? '';
const E3 = process.env['AIOPS_E2E_E3'] ?? '';

function watch(page: Page): string[] {
	const problems: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(m.text());
	});
	page.on('pageerror', (e) => problems.push(e.message));
	page.on('dialog', (d) => {
		problems.push(`dialog opened: ${d.message()}`);
		void d.dismiss();
	});
	return problems;
}

async function signIn(page: Page) {
	await page.goto('/console/');
	await page.getByPlaceholder('aiops_tk_…').fill(TOKEN);
	await page.getByRole('button', { name: 'Sign in' }).click();
	await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
}

async function openByRequest(page: Page, requestId: string) {
	await page.getByRole('link', { name: 'pilot' }).click();
	await page.getByPlaceholder(/Find by request ID/).fill(requestId);
	await page.getByRole('button', { name: 'Find' }).click();
	const row = page.locator('tbody tr').first();
	await expect(row).toContainText(requestId);
	await row.getByRole('link').click();
}

test('a lost publish response reads as an unknown external effect, with the authority named', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, LOST);
	await expect(page.getByRole('heading', { level: 1 })).toContainText('publish_article');
	await expect(page.locator('h1 .badge')).toHaveText('outcome unknown: timeout_after_send');
	await expect(page.getByText('An external effect may have happened.')).toBeVisible();
	await expect(page.getByText(/check the system that owns the effect/)).toBeVisible();
	const publish = page.locator('tbody tr', { hasText: 'publish' }).filter({ hasText: 'external effect' });
	await expect(publish.locator('.badge')).toHaveText('outcome unknown: timeout_after_send');
	await expect(page.getByText('Cost (estimate, not a bill)')).toBeVisible();
	await expect(page.getByText(/^Events \(event time · receive time\)$/)).toBeVisible();
	expect(problems).toEqual([]);
});

test('a complete operation shows no gaps and no warnings', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, OK);
	await expect(page.locator('h1 .badge')).toHaveText('succeeded');
	await expect(page.getByText(/^No gaps: every event/)).toBeVisible();
	await expect(page.locator('.notice')).toHaveCount(0);
	expect(problems).toEqual([]);
});

test('an operation known only from gap reports is shown as unknown, never as absent', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await page.goto(`/console/p/pilot/op/${DOWN}`);
	await expect(page.locator('h1 .badge')).toHaveText('no finish observed');
	await expect(page.locator('.notice.unknown').first()).toContainText(
		'A missing event is not proof that the work did not run.'
	);
	await expect(page.getByText('The SDK reported dropping events (send_failed)')).toBeVisible();
	expect(problems).toEqual([]);
});

test('event text from a host renders as text, never as markup', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, XSS);
	await expect(page.getByText('retry.reason=<img src=x onerror=alert(1)>')).toBeVisible();
	await expect(page.locator('img')).toHaveCount(0);
	expect(problems).toEqual([]);
});

test('a token without the project sees a denial, not data', async ({ page }) => {
	const problems = watch(page);
	await page.goto('/console/');
	await page.getByPlaceholder('aiops_tk_…').fill(process.env['AIOPS_E2E_OTHER_TOKEN'] ?? '');
	await page.getByRole('button', { name: 'Sign in' }).click();
	await page.goto(`/console/p/pilot/op/${DOWN}`);
	await expect(page.getByText(/Denied: forbidden: project_not_in_scope/)).toBeVisible();
	// The 403 itself is logged by the browser as a failed resource; nothing else may fail.
	expect(problems.filter((p) => !p.includes('403'))).toEqual([]);
});

test('a publish whose start was lost and whose outcome is unknown still warns, though the operation failed', async ({
	page
}) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, E3);
	await expect(page.locator('h1 .badge')).toHaveText('failed: internal');
	await expect(page.locator('.notice.uncertain').first()).toContainText('An external effect may have happened.');
	expect(problems).toEqual([]);
});
