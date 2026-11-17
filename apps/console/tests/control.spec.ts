import { expect, test, type Page } from '@playwright/test';

// Controlled recovery (A3, D-030) against the real collector, Durable Runner, adapter and
// worker: a failed publish whose article never landed is recovered as a new run once a
// person confirms; one whose article landed is stopped before anything is submitted; a
// finished run offers nothing; a read-only token cannot recover.

const READER = process.env['AIOPS_E2E_TOKEN'] ?? '';
const OPERATOR = process.env['AIOPS_E2E_OPERATOR_TOKEN'] ?? '';
const OUTAGE = process.env['AIOPS_E2E_DR_OUTAGE'] ?? '';
const LANDED = process.env['AIOPS_E2E_DR_LANDED'] ?? '';
const DR_OK = process.env['AIOPS_E2E_DR_OK'] ?? '';
const PUBLISHER = process.env['AIOPS_E2E_PUBLISHER'] ?? '';

async function signIn(page: Page, token: string) {
	await page.goto('/console/');
	await page.getByPlaceholder('aiops_tk_…').fill(token);
	await page.getByRole('button', { name: 'Sign in' }).click();
	await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
}

async function openRun(page: Page, opKey: string) {
	const r = await page.request.get(`/v1/projects/pilot/operations?request_id=${encodeURIComponent(opKey)}`, {
		headers: { authorization: `Bearer ${READER}` }
	});
	const id = ((await r.json()) as { operations: { operation_id: string }[] }).operations[0]!.operation_id;
	await page.goto(`/console/p/pilot/op/${id}`);
	await expect(page.getByTestId('authority')).toBeVisible();
}

async function published(key: string): Promise<number> {
	const r = await fetch(`${PUBLISHER}/publications/${encodeURIComponent(key)}`);
	if (r.status === 404) return 0;
	return ((await r.json()) as { times_published: number }).times_published;
}

test('a publish refused in an outage is recovered as a new run, once, after a person confirms', async ({ page }) => {
	await signIn(page, OPERATOR);
	await openRun(page, OUTAGE);
	await expect(page.getByTestId('authority-state')).toHaveText('failed:step:3');
	expect(await published(OUTAGE)).toBe(0);
	const card = page.getByTestId('recovery');
	await card.getByRole('button', { name: 'Check whether it can be resubmitted' }).click();
	const plan = card.getByTestId('recovery-record');
	await expect(plan).toHaveAttribute('data-status', 'planned');
	await expect(plan).toContainText(`${OUTAGE}#r1`);
	// Nothing is submitted until the person confirms.
	const resubmit = plan.getByRole('button', { name: `Resubmit as ${OUTAGE}#r1` });
	await expect(resubmit).toBeDisabled();
	await plan.getByRole('checkbox').check();
	await resubmit.click();
	await expect(plan).toHaveAttribute('data-status', 'submitted');
	// Success is the runner's word: ask until it says the new run finished.
	await expect(async () => {
		await card.getByRole('button', { name: 'Ask the runner again' }).click();
		await expect(card.getByTestId('recovery-outcome')).toContainText('finished every step', { timeout: 1_000 });
	}).toPass({ timeout: 20_000 });
	expect(await published(OUTAGE), 'the article is published exactly once').toBe(1);
	await expect(card.getByText(/confirmed by e2e-operator/)).toBeVisible();
	// The new run is its own operation, linked from here.
	await card.getByRole('link', { name: "Open the new run's operation" }).click();
	await expect(page.getByTestId('authority-state')).toHaveText('done');
});

test('a publish that landed while its reply was lost is stopped before any resubmit', async ({ page }) => {
	await signIn(page, OPERATOR);
	await openRun(page, LANDED);
	await expect(page.getByTestId('authority-state')).toHaveText('failed:step:3');
	await expect(page.getByText('An external effect may have happened.')).toBeVisible();
	const card = page.getByTestId('recovery');
	await card.getByRole('button', { name: 'Check whether it can be resubmitted' }).click();
	const plan = card.getByTestId('recovery-record');
	await expect(plan).toHaveAttribute('data-status', 'blocked');
	await expect(plan).toContainText('it already happened');
	await expect(card.getByRole('button', { name: /Resubmit/ })).toHaveCount(0);
	expect(await published(LANDED), 'still published once: nothing was resubmitted').toBe(1);
});

test('a finished run offers no recovery, and a read-only token cannot recover', async ({ page }) => {
	await signIn(page, OPERATOR);
	await openRun(page, DR_OK);
	await expect(page.getByTestId('authority-state')).toHaveText('done');
	await expect(page.getByTestId('recovery')).toHaveCount(0);
	await expect(page.getByRole('button', { name: /run again|rerun|retry|resubmit/i })).toHaveCount(0);
	await signIn(page, READER);
	await openRun(page, LANDED);
	await expect(page.getByTestId('recovery')).toContainText('needs a token with the operate scope');
	await expect(page.getByRole('button', { name: /Check whether/ })).toHaveCount(0);
});
