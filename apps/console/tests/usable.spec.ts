import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

// The console's usability bar (2026-11-17): the main investigation task by keyboard, zero
// serious or critical axe violations in light and dark, and every touched screen usable at
// 360 and 1440 px (screenshots in docs/screens/ when AIOPS_SCREENS_DIR is set).

const TOKEN = process.env['AIOPS_E2E_TOKEN'] ?? '';
const DR_INVALID = process.env['AIOPS_E2E_DR_INVALID'] ?? '';
const DR_RESTART = process.env['AIOPS_E2E_DR_RESTART'] ?? '';
const DR_LOST = process.env['AIOPS_E2E_DR_LOST'] ?? '';
const SCREENS = process.env['AIOPS_SCREENS_DIR'];

async function signIn(page: Page) {
	await page.goto('/console/');
	await page.getByPlaceholder('aiops_tk_…').fill(TOKEN);
	await page.getByRole('button', { name: 'Sign in' }).click();
	await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
}

async function opIdOf(page: Page, requestId: string): Promise<string> {
	const r = await page.request.get(`/v1/projects/pilot/operations?request_id=${encodeURIComponent(requestId)}`, {
		headers: { authorization: `Bearer ${TOKEN}` }
	});
	return ((await r.json()) as { operations: { operation_id: string }[] }).operations[0]!.operation_id;
}

async function noSeriousViolations(page: Page, what: string) {
	const { violations } = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa']).analyze();
	const bad = violations
		.filter((v) => v.impact === 'serious' || v.impact === 'critical')
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(' ')).slice(0, 3).join(' | ')}`);
	expect(bad, `${what}: serious or critical axe violations`).toEqual([]);
}

test('the main task by keyboard: find a failed operation, see what happened, see what is missing', async ({
	page
}) => {
	await signIn(page);
	await page.getByRole('link', { name: 'pilot' }).click();
	// Find: filter to failed operations, from the keyboard.
	await page.getByLabel('Status').focus();
	await page.keyboard.press('Tab'); // focus moves on, so the select is reachable in order
	await page.getByLabel('Status').selectOption('failed');
	await page.getByLabel('Operation name').fill('publish_article');
	await page.getByLabel('Operation name').press('Enter');
	const row = page.locator('tbody tr', { hasText: DR_INVALID });
	await expect(row).toHaveCount(1);
	const link = row.getByRole('link');
	await link.focus();
	const ring = await link.evaluate((el) => {
		const s = getComputedStyle(el);
		return s.outlineStyle !== 'none' || s.boxShadow !== 'none';
	});
	expect(ring, 'the focused link shows where focus is').toBe(true);
	await page.keyboard.press('Enter');
	// See what happened: a validation failure, five attempts, the runner's answer.
	await expect(page.locator('h1 .badge')).toHaveText('failed: validation');
	await expect(page.getByTestId('authority')).toContainText('failed at step 2 of 4');
	// See what is missing: on this operation nothing; on the restarted one, an unknown tail.
	await expect(page.getByText(/^No gaps/)).toBeVisible();
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_RESTART)}`);
	await expect(page.getByText('Events after this point may be missing (no finish observed)')).toBeVisible();
});

for (const scheme of ['light', 'dark'] as const) {
	test.describe(`${scheme} scheme`, () => {
		test.use({ colorScheme: scheme });
		test('touched screens have no serious or critical accessibility violations', async ({ page }) => {
			await signIn(page);
			await noSeriousViolations(page, 'projects');
			await page.goto('/console/p/pilot?status=failed');
			await expect(page.locator('tbody tr').first()).toBeVisible();
			await noSeriousViolations(page, 'operations, filtered');
			for (const rid of [DR_LOST, DR_RESTART]) {
				await page.goto(`/console/p/pilot/op/${await opIdOf(page, rid)}`);
				await expect(page.getByTestId('authority')).toBeVisible();
				await noSeriousViolations(page, `operation ${rid}`);
			}
			await page.goto('/console/p/pilot/releases?releases=cmp-a,cmp-b');
			await expect(page.locator('tr[data-release="cmp-b"]')).toBeVisible();
			await noSeriousViolations(page, 'releases');
		});

		for (const width of [360, 1440]) {
			test(`touched screens fit ${width} px`, async ({ page }) => {
				await page.setViewportSize({ width, height: 900 });
				await signIn(page);
				const screens: [string, string, () => Promise<void>][] = [
					['operations', '/console/p/pilot?status=failed', () => expect(page.locator('tbody tr').first()).toBeVisible()],
					[
						'operation',
						`/console/p/pilot/op/${await opIdOf(page, DR_LOST)}`,
						() => expect(page.getByTestId('authority')).toBeVisible()
					],
					[
						'releases',
						'/console/p/pilot/releases?releases=cmp-a,cmp-b',
						() => expect(page.locator('tr[data-release="cmp-b"]')).toBeVisible()
					]
				];
				for (const [name, url, ready] of screens) {
					await page.goto(url);
					await ready();
					const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
					expect(overflow, `${name} at ${width} px scrolls sideways`).toBeLessThanOrEqual(0);
					if (SCREENS) await page.screenshot({ path: `${SCREENS}/${name}-${width}-${scheme}.png`, fullPage: width === 1440 });
				}
			});
		}
	});
}
