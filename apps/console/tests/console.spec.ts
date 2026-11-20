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
const E2 = process.env['AIOPS_E2E_E2'] ?? '';
const BIG = process.env['AIOPS_E2E_BIG'] ?? '';
const LONG = process.env['AIOPS_E2E_LONG'] ?? '';

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
	await expect(page.locator('h1 .badge')).toHaveText('outcome unknown: timed out after sending');
	await expect(page.getByText('An external effect may have happened.')).toBeVisible();
	await expect(page.getByText(/check the system that owns the effect/)).toBeVisible();
	const publish = page
		.getByRole('rowgroup', { name: 'Steps' })
		.getByRole('row')
		.filter({ hasText: 'publish' })
		.filter({ hasText: 'external effect' });
	await expect(publish.locator('.badge')).toHaveText('outcome unknown: timed out after sending');
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
	// The check is async — navigating while it is in flight aborts its own fetch.
	await expect(page.getByRole('button', { name: 'Sign out' })).toBeVisible();
	await page.goto(`/console/p/pilot/op/${DOWN}`);
	await expect(page.getByText('This token cannot read this project.')).toBeVisible();
	await expect(page.getByText('The denial was recorded in the audit log.', { exact: false })).toBeVisible();
	// The 403 itself is logged by the browser as a failed resource; nothing else may fail.
	expect(problems.filter((p) => !p.includes('403'))).toEqual([]);
});

test('a publish whose start was lost and whose outcome is unknown still warns, though the operation failed', async ({
	page
}) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, E3);
	await expect(page.locator('h1 .badge')).toHaveText('failed: host error');
	await expect(page.locator('.notice.uncertain').first()).toContainText('An external effect may have happened.');
	expect(problems).toEqual([]);
});

test('two services that declare different outcomes read as conflicting, in the list and the timeline', async ({
	page
}) => {
	const problems = watch(page);
	await signIn(page);
	await page.getByRole('link', { name: 'pilot' }).click();
	await page.getByPlaceholder(/Find by request ID/).fill(E2);
	await page.getByRole('button', { name: 'Find' }).click();
	const row = page.locator('tbody tr').first();
	await expect(row.locator('.badge')).toHaveText('conflicting outcomes');
	await row.getByRole('link').click();
	await expect(page.locator('h1 .badge')).toHaveText('conflicting outcomes');
	await expect(page.locator('.notice.fail').first()).toContainText('Conflicting outcomes.');
	expect(problems).toEqual([]);
});

test('a 5,000-event timeline renders a window of rows and scrolls to the last one', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, BIG);
	await expect(page.locator('h1 .badge')).toHaveText('succeeded');
	const list = page.getByRole('rowgroup', { name: 'Events' });
	const rendered = await list.getByRole('row').count();
	expect(rendered).toBeGreaterThan(5);
	expect(rendered).toBeLessThan(100); // of 5,000
	await list.evaluate((el) => (el.scrollTop = el.scrollHeight));
	await expect(list.getByText('operation finished')).toBeVisible();
	expect(problems).toEqual([]);
});

test('a 5,000-event operation lists its 2,499 step attempts as a window and scrolls to the last one', async ({
	page
}) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, BIG);
	const steps = page.getByRole('rowgroup', { name: 'Steps' });
	await expect(steps).toBeVisible();
	const rendered = await steps.getByRole('row').count();
	expect(rendered).toBeGreaterThan(5);
	expect(rendered).toBeLessThan(100); // of 2,499
	await expect(steps.locator('[data-step-id="00000000000009c3"]')).toHaveCount(0); // the last step is not in the DOM yet
	await steps.evaluate((el) => (el.scrollTop = el.scrollHeight));
	await expect(steps.locator('[data-step-id="00000000000009c3"]')).toBeVisible();
	expect(problems).toEqual([]);
});

test.describe('dark mode', () => {
	test.use({ colorScheme: 'dark' });
	test('renders under the CSP with its static theme script', async ({ page }) => {
		const problems = watch(page);
		await signIn(page);
		await expect(page.locator('html')).toHaveClass(/dark/);
		await openByRequest(page, LOST);
		await expect(page.locator('h1 .badge')).toHaveText('outcome unknown: timed out after sending');
		expect(problems).toEqual([]);
	});
});

for (const width of [1280, 1920]) {
	test(`every outcome badge in the events list is whole at ${width} px, even beside a 128-byte name`, async ({
		page
	}) => {
		const problems = watch(page);
		await page.setViewportSize({ width, height: 900 });
		await signIn(page);
		await openByRequest(page, LONG);
		await expect(page.locator('h1 .badge')).toHaveText('outcome unknown: connection lost after sending');
		const badges = page.getByRole('rowgroup', { name: 'Events' }).locator('[role="row"] .badge');
		await expect(badges).toHaveCount(2); // the step's and the operation's finish
		for (const badge of await badges.all()) {
			const cell = badge.locator('xpath=ancestor::*[@data-cell][1]');
			const b = (await badge.boundingBox())!;
			const c = (await cell.boundingBox())!;
			expect(b.x, 'badge starts inside its cell').toBeGreaterThanOrEqual(c.x - 0.5);
			expect(b.x + b.width, 'badge ends inside its cell').toBeLessThanOrEqual(c.x + c.width + 0.5);
			expect(b.x + b.width, 'badge is on screen').toBeLessThanOrEqual(width);
			expect(await badge.evaluate((el) => el.scrollWidth <= el.clientWidth + 1), 'badge text is not cut').toBe(true);
		}
		expect(problems).toEqual([]);
	});
}

test('host text is isolated, so a bidi control cannot reorder what follows it', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await openByRequest(page, XSS);
	const values = page.locator('[data-cell="details"] bdi');
	await expect(values).toHaveCount(2); // retry.reason and http.route
	const rlo = values.filter({ hasText: '\u202e' });
	await expect(rlo).toHaveCount(1);
	expect(await rlo.evaluate((el) => getComputedStyle(el).unicodeBidi)).toBe('isolate');
	expect(problems).toEqual([]);
});

// --- The first real host (A2): Durable Runner runs the article workflow through the adapter.
const DR_OK = process.env['AIOPS_E2E_DR_OK'] ?? '';
const DR_LOST = process.env['AIOPS_E2E_DR_LOST'] ?? '';
const DR_INVALID = process.env['AIOPS_E2E_DR_INVALID'] ?? '';
const DR_RESTART = process.env['AIOPS_E2E_DR_RESTART'] ?? '';

test.describe('Durable Runner, the first real host', () => {
	test('a run that finished is complete, and the runner says done beside it', async ({ page }) => {
		const problems = watch(page);
		await signIn(page);
		await openByRequest(page, DR_OK);
		await expect(page.locator('h1 .badge')).toHaveText('succeeded');
		await expect(page.getByText(/^No gaps: every event/)).toBeVisible();
		await expect(page.getByTestId('authority-state')).toContainText('done');
		await expect(page.getByTestId('authority')).toContainText('Read ');
		const steps = page.getByRole('rowgroup', { name: 'Steps' });
		for (const name of ['generate', 'validate', 'factcheck', 'publish']) {
			await expect(steps.getByRole('row').filter({ hasText: name }).first()).toBeVisible();
		}
		expect(problems).toEqual([]);
	});

	test('a lost publish reply: the attempt is uncertain, the runner reconciled it, and it says done', async ({
		page
	}) => {
		const problems = watch(page);
		await signIn(page);
		await openByRequest(page, DR_LOST);
		await expect(page.locator('h1 .badge')).toHaveText('succeeded');
		await expect(page.getByText('An external effect may have happened.')).toBeVisible();
		const steps = page.getByRole('rowgroup', { name: 'Steps' });
		const publish = steps.getByRole('row').filter({ hasText: 'external effect' });
		await expect(publish.locator('.badge')).toHaveText('outcome unknown: timed out after sending');
		await expect(publish).toContainText('runner_deadline_passed');
		await expect(steps.getByRole('row').filter({ hasText: 'publish.reconcile' }).locator('.badge')).toHaveText(
			'succeeded'
		);
		await expect(page.getByTestId('authority-state')).toContainText('done');
		expect(problems).toEqual([]);
	});

	test('a real failure: every validate attempt failed, and the runner says failed at that step', async ({ page }) => {
		const problems = watch(page);
		await signIn(page);
		await openByRequest(page, DR_INVALID);
		await expect(page.locator('h1 .badge')).toHaveText('failed: invalid output');
		await expect(page.getByTestId('authority-state')).toContainText('failed:step:1');
		const validate = page.getByRole('rowgroup', { name: 'Steps' }).getByRole('row').filter({ hasText: 'validate' });
		await expect(validate).toHaveCount(5); // the runner's five attempts, numbered by the runner
		await expect(validate.locator('.badge').first()).toHaveText('failed: invalid output');
		expect(problems).toEqual([]);
	});

	test('missing telemetry, not a failure: an adapter restart leaves an unknown tail while the runner says done', async ({
		page
	}) => {
		const problems = watch(page);
		await signIn(page);
		await openByRequest(page, DR_RESTART);
		await expect(page.locator('h1 .badge')).toHaveText('succeeded');
		await expect(page.getByText(/an unknown number/)).toBeVisible();
		await expect(page.getByText('Events after this point may be missing (no finish observed)')).toBeVisible();
		await expect(page.getByText(/^No gaps/)).toHaveCount(0);
		await expect(page.getByTestId('authority-state')).toContainText('done');
		const generate = page.getByRole('rowgroup', { name: 'Steps' }).getByRole('row').filter({ hasText: 'generate' });
		await expect(generate.first().locator('.badge')).toHaveText('no finish observed');
		expect(problems).toEqual([]);
	});
});

test('releases compare from the same records the filtered list shows', async ({ page }) => {
	const problems = watch(page);
	await signIn(page);
	await page.getByRole('link', { name: 'pilot' }).click();
	await page.getByRole('link', { name: 'Compare releases' }).click();
	await page.getByLabel('Releases').fill('cmp-a, cmp-b');
	await page.getByRole('button', { name: 'Compare' }).click();
	const b = page.locator('tr[data-release="cmp-b"]');
	await expect(b.locator('td').nth(1)).toHaveText('3');
	await expect(b.locator('[data-status="no_finish_observed"]')).toHaveText('1');
	await expect(page.locator('tr[data-release="cmp-a"] [data-status="succeeded"]')).toHaveText('2');
	await b.locator('[data-status="failed"] a').click();
	const rows = page.locator('tbody tr');
	await expect(rows).toHaveCount(1);
	await expect(rows.first()).toContainText('e2e-cmp-4');
	await expect(rows.first().locator('.badge')).toHaveText('failed: refused');
	await expect(page.getByLabel('Status')).toHaveValue('failed');
	expect(problems).toEqual([]);
});
