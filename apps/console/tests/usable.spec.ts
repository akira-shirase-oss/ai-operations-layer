import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Locator, type Page } from '@playwright/test';

// The console's usability bar (2026-11-17): the main investigation task by keyboard, zero
// serious or critical axe violations in light and dark, and every touched screen usable at
// 360 and 1440 px (screenshots in docs/screens/ when AIOPS_SCREENS_DIR is set).

const TOKEN = process.env['AIOPS_E2E_TOKEN'] ?? '';
const DR_INVALID = process.env['AIOPS_E2E_DR_INVALID'] ?? '';
const DR_RESTART = process.env['AIOPS_E2E_DR_RESTART'] ?? '';
const DR_LOST = process.env['AIOPS_E2E_DR_LOST'] ?? '';
const DR_OK = process.env['AIOPS_E2E_DR_OK'] ?? '';
const DR_CANCEL = process.env['AIOPS_E2E_DR_CANCEL'] ?? '';
const GAP_OP = process.env['AIOPS_E2E_GAP'] ?? '';
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
	await expect(page.locator('h1 .badge')).toHaveText('failed: invalid output');
	await expect(page.getByTestId('authority')).toContainText('failed at step 2 of 4');
	// See what is missing: on this operation nothing; on the restarted one, an unknown tail.
	await expect(page.getByText(/^No gaps/)).toBeVisible();
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_RESTART)}`);
	await expect(page.getByText('Events after this point may be missing (no finish observed)')).toBeVisible();
});

/** Why an element is not wholly readable where it is drawn, or null when it is: it must be on
 * screen, not cut sideways by any box that clips (a table that scrolls sideways included, at
 * its initial scroll), not cut top or bottom by its row, and its text must not be truncated. */
async function cutReason(el: Locator): Promise<string | null> {
	return el.evaluate((node) => {
		const r = node.getBoundingClientRect();
		if (r.width === 0 || r.height === 0) return 'not drawn';
		if (r.left < -0.5 || r.right > window.innerWidth + 0.5) return `off screen (${r.left}–${r.right})`;
		for (let a = node.parentElement; a; a = a.parentElement) {
			const s = getComputedStyle(a);
			if (s.overflowX === 'visible' && s.overflowY === 'visible') continue;
			const c = a.getBoundingClientRect();
			if (r.left < c.left - 0.5 || r.right > c.right + 0.5) return `cut sideways by <${a.tagName.toLowerCase()} class="${a.className}">`;
			if (a.getAttribute('role') === 'row' && (r.top < c.top - 0.5 || r.bottom > c.bottom + 0.5)) return 'cut by its row';
		}
		if (node.scrollWidth > node.clientWidth + 1) return 'its text is truncated';
		return null;
	});
}

async function allWhole(what: string, els: Locator) {
	const all = await els.all();
	expect(all.length, `${what}: nothing to check`).toBeGreaterThan(0);
	for (const [i, el] of all.entries()) expect(await cutReason(el), `${what} #${i + 1}: ${await el.innerText()}`).toBeNull();
}

for (const width of [360, 1440]) {
	test(`status and outcome are always whole at ${width} px`, async ({ page }) => {
		await page.setViewportSize({ width, height: 900 });
		await signIn(page);
		// The list: every operation's status, without scrolling sideways.
		await page.goto('/console/p/pilot');
		await expect(page.getByTestId('operation').first()).toBeVisible();
		await allWhole('operations list status', page.locator('[data-testid="operation"]:visible .badge'));
		// An operation: every attempt's outcome, every event's outcome and every gap's count.
		for (const rid of [DR_INVALID, DR_LOST, DR_RESTART, DR_OK]) {
			await page.goto(`/console/p/pilot/op/${await opIdOf(page, rid)}`);
			await expect(page.getByTestId('authority')).toBeVisible();
			await allWhole(`${rid}: header status`, page.locator('h1 .badge'));
			await allWhole(`${rid}: step outcomes`, page.getByRole('rowgroup', { name: 'Steps' }).locator('[role="row"] .badge'));
			await allWhole(`${rid}: event outcomes`, page.getByRole('rowgroup', { name: 'Events' }).locator('[role="row"] .badge'));
			if (rid === DR_RESTART) await allWhole(`${rid}: gap counts`, page.locator('[data-testid="gap-count"]:visible'));
		}
		await page.goto(`/console/p/pilot/op/${GAP_OP}`);
		await expect(page.locator('h1 .badge')).toBeVisible();
		await allWhole('gap-only operation: gap counts', page.locator('[data-testid="gap-count"]:visible'));
	});
}

test('outcomes read in plain words, with the codes one click away', async ({ page }) => {
	await signIn(page);
	const codesIn = async (where: string) => {
		const texts = await page.locator('.badge').allInnerTexts();
		expect(texts.filter((t) => /[a-z]_[a-z]/.test(t)), `${where}: a badge shows an internal code`).toEqual([]);
	};
	// A real failure: plain words in the header and the steps, the code behind a toggle.
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_INVALID)}`);
	await expect(page.locator('h1 .badge')).toHaveText('failed: invalid output');
	await expect(page.getByTestId('codes')).toBeHidden();
	await page.getByText('Codes', { exact: true }).click();
	await expect(page.getByTestId('codes')).toContainText('failure_class validation');
	const steps = page.getByRole('rowgroup', { name: 'Steps' });
	await expect(steps.getByRole('row').first().locator('.badge')).toHaveText('succeeded');
	expect(await steps.innerText(), 'no raw step IDs in the steps').not.toMatch(/[0-9a-f]{16}/);
	// Events by name in words, not wire types.
	const events = page.getByRole('rowgroup', { name: 'Events' });
	await expect(events.getByText(/^operation started/).first()).toBeVisible();
	expect(await events.innerText()).not.toContain('operation.started');
	await codesIn('a validation failure');
	// A lost reply.
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_LOST)}`);
	await expect(steps.getByRole('row').filter({ hasText: 'external effect' }).locator('.badge')).toHaveText(
		'outcome unknown: timed out after sending'
	);
	await codesIn('a lost reply');
	// A cancelled run is cancelled, in the list and on the page, never "failed: internal".
	await page.goto(`/console/p/pilot?request_id=${encodeURIComponent(DR_CANCEL)}`);
	await expect(page.getByTestId('operation').locator('.badge')).toHaveText('cancelled');
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_CANCEL)}`);
	await expect(page.locator('h1 .badge')).toHaveText('cancelled');
	await expect(page.getByTestId('authority')).toContainText('was cancelled');
	await codesIn('a cancelled run');
	// And the list: no badge on the first page shows a code.
	await page.goto('/console/p/pilot');
	await expect(page.getByTestId('operation').first()).toBeVisible();
	await codesIn('the operations list');
});

test('an operation says in one line what happened, in plain English', async ({ page }) => {
	await signIn(page);
	const said = page.getByTestId('what-happened');
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_INVALID)}`);
	await expect(said).toHaveText(/^validate failed 5 times \(invalid output\)/);
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_LOST)}`);
	await expect(said).toHaveText(/^publish ended with an unknown outcome \(timed out after sending\) on attempt 1/);
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_OK)}`);
	await expect(said).toHaveText(/^Every step succeeded: 4 steps, 4 attempts\.$/);
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_CANCEL)}`);
	await expect(said).toHaveText(/^Cancelled on request/);
	// Copy: counts agree with their nouns, and a one-event gap is one number.
	await page.goto(`/console/p/pilot/op/${await opIdOf(page, DR_RESTART)}`);
	await expect(page.locator('main')).not.toContainText(/\b1 steps\b/);
	await expect(page.locator('main')).not.toContainText(/#(\d+)–#\1\b/);
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
					[
						'operations',
						'/console/p/pilot?status=failed',
						() => expect(page.getByTestId('operation').first()).toBeVisible()
					],
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
					await noSeriousViolations(page, `${name} at ${width} px`);
					if (SCREENS) await page.screenshot({ path: `${SCREENS}/${name}-${width}-${scheme}.png`, fullPage: width === 1440 });
				}
			});
		}
	});
}
