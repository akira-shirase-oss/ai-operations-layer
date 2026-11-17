<script lang="ts">
	import { resolve } from '$app/paths';
	import { Button } from '@aiops/ui/ui/button';
	import { Card, CardContent, CardHeader, CardTitle } from '@aiops/ui/ui/card';
	import Notice from '#lib/components/Notice.svelte';
	import { api, send, type Result, type Sent } from '#lib/api.js';
	import { time } from '#lib/format.js';
	import type { ProjectsView, Recovery, RecoveryList } from '#lib/types.js';

	// Controlled recovery (A3, D-030). Never a "run again": a person checks what the run may
	// have done (against the systems that own it), then confirms a new run under a derived
	// key. Whether it worked is the runner's answer, shown as the runner gives it.
	let {
		project,
		operation,
		runnerState,
		businessKey
	}: { project: string; operation: string; runnerState: string; businessKey: string } = $props();

	const base = $derived(`/v1/projects/${encodeURIComponent(project)}`);
	const recoverable = $derived(runnerState === 'cancelled' || /^failed:step:\d+$/.test(runnerState));
	let list = $state<Result<RecoveryList> | null>(null);
	let canOperate = $state(false);
	let answer = $state<Sent<Recovery> | null>(null);
	let confirmed = $state(false);
	let busy = $state(false);

	function load() {
		api<RecoveryList>(`${base}/operations/${encodeURIComponent(operation)}/recoveries`).then((r) => (list = r));
	}
	$effect(() => {
		load();
		api<ProjectsView>('/v1/projects').then((r) => (canOperate = r.state === 'ok' && r.data.scopes.includes('operate')));
	});

	const records = $derived(list?.state === 'ok' ? list.data.recoveries : []);
	const open = $derived(records.findLast((r) => r.status === 'planned'));
	// A recovery in flight, or one whose new run did not fail, is not followed by another plan.
	const pending = $derived(
		records.some(
			(r) =>
				r.status === 'submitting' ||
				(r.status === 'submitted' &&
					!(r.new_run?.runner_state === 'cancelled' || r.new_run?.runner_state?.startsWith('failed:')))
		)
	);

	async function check() {
		busy = true;
		confirmed = false;
		answer = await send<Recovery>(`${base}/operations/${encodeURIComponent(operation)}/recoveries`, {
			request_key: crypto.randomUUID(),
			expected_state: runnerState,
			business_key: businessKey
		});
		busy = false;
		load();
	}

	async function resubmit(r: Recovery) {
		busy = true;
		answer = await send<Recovery>(`${base}/recoveries/${encodeURIComponent(r.id)}/confirm`, {
			expected_state: runnerState
		});
		busy = false;
		confirmed = false;
		load();
	}

	function runnerWords(state: string | undefined): string {
		if (!state) return 'has not been asked';
		if (state === 'done') return 'finished every step';
		if (state === 'cancelled') return 'was cancelled';
		const failed = /^failed:step:(\d+)$/.exec(state);
		if (failed) return `failed at step ${Number(failed[1]) + 1}`;
		return 'is still in progress';
	}
</script>

{#if recoverable || records.length > 0}
	<Card class="mt-4" data-testid="recovery">
		<CardHeader>
			<CardTitle class="text-sm text-muted-foreground">Recovery (a new run, never a rerun of this one)</CardTitle>
		</CardHeader>
		<CardContent class="grid gap-3 text-sm">
			{#if list?.state === 'ok' && !list.data.configured}
				<p>This collector is not configured to recover runs (OPERATING, "Controlled recovery").</p>
			{:else if recoverable && !canOperate}
				<p>Recovering a run needs a token with the <code>operate</code> scope.</p>
			{:else if recoverable && !open && !pending}
				<p>
					Before anything is resubmitted, the layer asks the runner again and asks the systems that own this run's
					effects (the publisher, by business key <bdi class="font-mono">{businessKey}</bdi>) whether they already
					happened.
				</p>
				<Button class="w-fit" variant="outline" disabled={busy} onclick={check}>Check whether it can be resubmitted</Button>
			{/if}

			{#if answer && answer.state !== 'ok'}
				<Notice kind="fail" title="Nothing was submitted.">
					{answer.state === 'refused' || answer.state === 'forbidden' || answer.state === 'unreachable'
						? answer.message
						: 'The request could not be completed.'}
				</Notice>
			{/if}

			{#each records as r (r.id)}
				<div class="grid gap-2 rounded-lg border p-3" data-testid="recovery-record" data-status={r.status}>
					{#if r.status === 'blocked'}
						<Notice kind={r.effect === 'landed' ? 'fail' : 'uncertain'} title="Do not resubmit.">
							{r.effect === 'landed'
								? `The publisher has a record of ${r.business_key}: it already happened. Resubmitting would repeat it.`
								: `Whether ${r.business_key}'s effect happened is unknown (${r.effect_detail ?? 'no answer'}). Ask the system that owns it; nothing will be submitted.`}
						</Notice>
					{:else if r.status === 'planned'}
						<p>
							No system that owns this run's effects has a record of <bdi class="font-mono">{r.business_key}</bdi>.
							Resubmitting runs the same work as a <strong>new run</strong> under
							<bdi class="font-mono">{r.derived_key}</bdi>; this run stays as it is.
						</p>
						{#if canOperate}
							<label class="flex items-start gap-2">
								<input type="checkbox" bind:checked={confirmed} />
								<span>I confirm: submit a new run under <bdi class="font-mono">{r.derived_key}</bdi>.</span>
							</label>
							<Button class="w-fit" disabled={!confirmed || busy} onclick={() => resubmit(r)}
								>Resubmit as {r.derived_key}</Button
							>
						{/if}
					{:else if r.status === 'submitting'}
						<p>Being submitted by another confirm; reload in a moment.</p>
					{:else if r.new_run}
						<p data-testid="recovery-outcome">
							Submitted under <bdi class="font-mono">{r.derived_key}</bdi> as run {r.new_run.run_id}. Durable Runner says
							that run <strong>{runnerWords(r.new_run.runner_state)}</strong>{r.new_run.read_at
								? ` (read ${time(r.new_run.read_at)})`
								: ''}{r.new_run.unavailable ? `; it could not be asked: ${r.new_run.unavailable}` : ''}.
						</p>
						<p>
							<a
								class="underline-offset-4 hover:underline"
								href={resolve('/p/[project]/op/[id]', { project, id: r.new_run.operation_id })}>Open the new run's operation</a
							>
							· <Button variant="ghost" size="sm" onclick={load}>Ask the runner again</Button>
						</p>
					{/if}
					<p class="text-xs text-muted-foreground">
						Planned by {r.planned_by} at {time(r.created_at)} against the runner's state
						<code>{r.observed_state}</code>{r.approved_by ? `; confirmed by ${r.approved_by} at ${time(r.approved_at)}` : ''}.
						Every step is in the audit log.
					</p>
				</div>
			{/each}
		</CardContent>
	</Card>
{/if}
