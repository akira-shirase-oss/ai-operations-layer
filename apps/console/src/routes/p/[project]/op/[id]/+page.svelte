<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { Card, CardContent, CardHeader, CardTitle } from '@aiops/ui/ui/card';
	import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@aiops/ui/ui/table';
	import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@aiops/ui/ui/tooltip';
	import { api, type Result } from '#lib/api.js';
	import { micros, money, short, time, value } from '#lib/format.js';
	import type { EntryView, Timeline } from '#lib/types.js';
	import StatusBadge from '#lib/components/StatusBadge.svelte';
	import Notice from '#lib/components/Notice.svelte';
	import Flag from '#lib/components/Flag.svelte';
	import VirtualRows from '#lib/components/VirtualRows.svelte';

	const project = $derived(page.params['project'] ?? '');
	const id = $derived(page.params['id'] ?? '');
	let result: Result<Timeline> | null = $state(null);

	$effect(() => {
		api<Timeline>(`/v1/projects/${encodeURIComponent(project)}/operations/${encodeURIComponent(id)}`).then(
			(r) => (result = r)
		);
	});

	const GAP_TEXT: Record<string, string> = {
		sdk_reported: 'The SDK reported dropping events',
		op_seq_hole: 'Events missing (proven by sequence numbers)',
		unknown_tail: 'Events after this point may be missing (no finish observed)',
		expired: 'Deleted by retention (received before the cutoff)'
	};
	// The outcome has its own fixed column, wide enough for the longest badge ("outcome
	// unknown: connection_lost_after_send"), ahead of the free text; the event name and the
	// details truncate instead, with the full text on hover (review N4).
	const EVENT_COLUMNS =
		'grid grid-cols-[12rem_4.5rem_23rem_minmax(7rem,1fr)_2.5rem_8rem_minmax(6rem,14rem)] gap-3 px-3';
	const eventText = (e: EntryView) =>
		`${e.type}${e.name ? ` · ${e.name}` : ''}${e.attempt ? ` #${e.attempt}` : ''}`;
	const detailText = (e: EntryView) =>
		[
			e.dropped ? `${e.dropped} dropped: ${e.reason}` : '',
			...e.attributes.map((a) => `${a.key}=${value(a.value)}${a.opt_in ? ' (opt-in)' : ''}`)
		]
			.filter(Boolean)
			.join('  ');
</script>

<p class="text-sm text-muted-foreground">
	<a href={resolve('/')}>Projects</a> / <a href={resolve('/p/[project]', { project })}>{project}</a> / operation
</p>

{#if result === null}
	<p class="text-muted-foreground">Loading…</p>
{:else if result.state === 'ok'}
	{@const t = result.data}
	<h1 class="mt-1 flex flex-wrap items-center gap-3 text-2xl font-semibold tracking-tight">
		{t.name ?? 'Operation (name not observed)'} <StatusBadge status={t.status} />
	</h1>
	<p class="mb-4 font-mono text-xs text-muted-foreground">
		{t.operation_id}{t.request_id ? ` · request ${t.request_id}` : ''}{t.release ? ` · release ${t.release}` : ''}{t.deployment
			? ` · ${t.deployment}`
			: ''}
	</p>

	{#if t.status.state === 'uncertain' || t.flags.external_effect_unknown}
		<Notice kind="uncertain" title="An external effect may have happened.">
			<ul class="list-disc pl-4">
				{#each t.effect_unknown_steps as c (c.step_id + c.attempt)}
					<li>
						{c.name ?? 'A step'} (attempt {c.attempt})
						{c.cause === 'uncertain' ? 'ended with an unknown outcome' : 'never reported how it ended'}{c.declared ===
						'unknown'
							? '; its start was not received, so whether it can affect the outside world is unknown'
							: ''}.
					</li>
				{/each}
				{#if t.effect_unknown_steps.length === 0}
					<li>The host declared the operation's outcome unknown.</li>
				{/if}
			</ul>
			This timeline is evidence, not authority: check the system that owns the effect (by business key) before any
			retry. Do not rerun blind.
		</Notice>
	{/if}
	{#if t.status.state === 'no_finish_observed'}
		<Notice kind="unknown" title="No finish observed.">
			The host never reported how this operation ended. A missing event is not proof that the work did not run.
		</Notice>
	{/if}
	{#if t.expired_by_retention}
		<Notice kind="unknown" title="Some of this operation's detail has expired.">
			Raw events are kept 14 days; those received before {time(t.retention_cutoff)} were deleted ({t.coverage.expired} of
			this operation's events). The outcome and counts that remain come from its summary.
		</Notice>
	{/if}
	{#if t.flags.start_not_observed && !t.expired_by_retention}
		<Notice kind="unknown">The start of this operation was not received; it is known only from later events.</Notice>
	{/if}
	{#if t.flags.conflicting_finish}
		<Notice kind="fail" title="Conflicting outcomes.">
			More than one host declared how this operation ended, and the declarations disagree (see the finish events
			below). No single outcome is shown; ask the system that owns the work.
		</Notice>
	{/if}
	{#if t.flags.clock_skew_suspected}
		<Notice kind="uncertain">
			Clock skew suspected: some event times disagree with emission order or arrive from the future. Each source is
			shown in its emission order; flagged rows may be out of place across sources.
		</Notice>
	{/if}
	{#if t.events_truncated}
		<Notice kind="uncertain">Only the first part of this very large operation is shown.</Notice>
	{/if}

	<div class="mt-4 grid gap-4 md:grid-cols-3">
		<Card>
			<CardHeader><CardTitle class="text-sm text-muted-foreground">Coverage</CardTitle></CardHeader>
			<CardContent>
				<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
					<dt class="text-muted-foreground">Events received</dt>
					<dd>
						{t.coverage.events_received}{t.coverage.sdk_gap_reports ? ` + ${t.coverage.sdk_gap_reports} gap reports` : ''}
					</dd>
					<dt class="text-muted-foreground">Missing</dt>
					<dd>
						{#if t.coverage.missing_known === 0 && !t.coverage.missing_unknown}none{/if}
						{#if t.coverage.missing_known > 0}{t.coverage.missing_known} proven missing{/if}
						{#if t.coverage.missing_unknown}{t.coverage.missing_known > 0 ? ' + ' : ''}an unknown number{/if}
					</dd>
					{#if t.coverage.expired > 0}
						<dt class="text-muted-foreground">Expired (retention)</dt>
						<dd>{t.coverage.expired}</dd>
					{/if}
					<dt class="text-muted-foreground">Dropped by the SDK (reported)</dt>
					<dd>{t.coverage.reported_dropped}</dd>
				</dl>
			</CardContent>
		</Card>
		<Card>
			<CardHeader><CardTitle class="text-sm text-muted-foreground">Attempts</CardTitle></CardHeader>
			<CardContent class="text-sm">
				{t.flags.failed_attempts} failed · {t.flags.uncertain_attempts} uncertain · {t.flags.retried_steps} steps retried
				<div class="text-muted-foreground">Late events: {t.flags.late_events}</div>
			</CardContent>
		</Card>
		<Card>
			<CardHeader><CardTitle class="text-sm text-muted-foreground">Cost (estimate, not a bill)</CardTitle></CardHeader>
			<CardContent class="font-mono text-sm">
				{t.cost.attempts_with_estimate ? money(t.cost.estimate_micros, t.cost.currency) : 'no estimate reported'}
				{#if t.cost.mixed_currencies}<Flag>mixed currencies</Flag>{/if}
				<div class="text-muted-foreground">{t.cost.input_tokens} in · {t.cost.output_tokens} out (tokens)</div>
			</CardContent>
		</Card>
	</div>

	<h2 class="mt-8 mb-2 text-lg font-semibold">Steps</h2>
	{#if t.steps.length === 0}
		<p class="text-muted-foreground">No step events received.</p>
	{:else}
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Step</TableHead>
					<TableHead>Attempt</TableHead>
					<TableHead>Outcome</TableHead>
					<TableHead>Duration</TableHead>
					<TableHead>Model / version</TableHead>
					<TableHead>Error</TableHead>
					<TableHead>Est. cost</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each t.steps as s (s.step_id)}
					{#each s.attempts as a (a.attempt)}
						<TableRow>
							<TableCell>
								{#if a.attempt === s.attempts[0]?.attempt}
									<strong>{s.name ?? 'name not observed'}</strong>
									<div class="text-xs text-muted-foreground">
										{s.kind ?? 'kind unknown'}{s.external_effect ? ' · external effect' : ''}
									</div>
								{/if}
							</TableCell>
							<TableCell>{a.attempt}</TableCell>
							<TableCell>
								<StatusBadge status={a} />
								{#if !a.start_observed}<Flag>start not received</Flag>{/if}
							</TableCell>
							<TableCell class="font-mono text-xs">{micros(a.duration_us)}</TableCell>
							<TableCell class="font-mono text-xs"
								>{a.model ?? ''}{a.prompt_version ? ` · ${a.prompt_version}` : ''}{a.config_version
									? ` · ${a.config_version}`
									: ''}</TableCell
							>
							<TableCell class="font-mono text-xs">
								{a.error_code ?? ''}{#if a.error_message}<div class="text-muted-foreground">{a.error_message}</div>{/if}
							</TableCell>
							<TableCell class="font-mono text-xs"
								>{a.cost_estimate_micros !== undefined ? money(a.cost_estimate_micros, a.cost_currency ?? null) : ''}</TableCell
							>
						</TableRow>
					{/each}
				{/each}
			</TableBody>
		</Table>
	{/if}

	<h2 class="mt-8 mb-2 text-lg font-semibold">Gaps</h2>
	{#if t.gaps.length === 0}
		<p class="text-muted-foreground">
			No gaps: every event this operation's SDK numbered was received{t.status.declared ? '' : ' so far'}.
		</p>
	{:else}
		<Table>
			<TableHeader>
				<TableRow><TableHead>What</TableHead><TableHead>How many</TableHead><TableHead>Where</TableHead><TableHead
						>Source</TableHead
					></TableRow
				>
			</TableHeader>
			<TableBody>
				{#each t.gaps as g, i (i)}
					<TableRow class="gap border-dashed bg-state-unknown-soft/60">
						<TableCell>{GAP_TEXT[g.kind] ?? g.kind}{g.reason ? ` (${g.reason})` : ''}</TableCell>
						<TableCell>{g.count ?? 'unknown'}</TableCell>
						<TableCell class="font-mono text-xs">
							{#if g.op_seq_from !== undefined}#{g.op_seq_from}{g.op_seq_to !== undefined ? `–#${g.op_seq_to}` : ' onwards'}{/if}
							{#if g.from}{time(g.from)} – {time(g.to)}{/if}
						</TableCell>
						<TableCell class="font-mono text-xs">{g.instance_id ? short(g.instance_id) : '—'}</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	{/if}

	<h2 class="mt-8 mb-2 text-lg font-semibold">Events (event time · receive time)</h2>
	<TooltipProvider>
		<div class="overflow-x-auto">
			<div class="min-w-[69rem]">
				<div class="{EVENT_COLUMNS} py-2 text-xs font-medium text-muted-foreground" role="row">
					<span>Event time · received</span><span>Delay</span><span>Outcome</span><span>Event</span><span>#</span
					><span>Source</span><span>Details</span>
				</div>
				<VirtualRows rows={t.entries} key={(e: EntryView) => e.event_id}>
					{#snippet row(e: EntryView)}
						<div
							class="{EVENT_COLUMNS} h-full items-center text-sm {e.type === 'gap.reported'
								? 'gap bg-state-unknown-soft/60'
								: ''}"
						>
							<span class="font-mono text-xs leading-tight">
								{time(e.event_time)}<br /><span class="text-muted-foreground">{time(e.received_at)}</span>
							</span>
							<span class="font-mono text-xs">{micros(e.receive_delay_us)}</span>
							<span data-cell="outcome" class="min-w-0">
								{#if e.outcome}<StatusBadge status={e.outcome} />{/if}
							</span>
							<span data-cell="event" class="min-w-0 truncate" title={eventText(e)}>{eventText(e)}</span>
							<span class="font-mono text-xs">{e.op_seq || '—'}</span>
							<span class="min-w-0 truncate font-mono text-xs">
								<Tooltip>
									<TooltipTrigger>{e.ingest_key} · {short(e.instance_id)}</TooltipTrigger>
									<TooltipContent>instance {e.instance_id}</TooltipContent>
								</Tooltip>
							</span>
							<span data-cell="details" class="min-w-0 truncate" title={detailText(e)}>
								{#each e.flags as f (f)}<Flag>{f.replaceAll('_', ' ')}</Flag>{/each}
								{#if e.withheld_attributes}<Flag>{e.withheld_attributes} attributes withheld by policy</Flag>{/if}
								{#if e.redactions}<Flag>{e.redactions} redacted</Flag>{/if}
								{#if e.overflowed_names}<Flag>{e.overflowed_names} names over budget, stored as _overflow</Flag>{/if}
								{#if e.truncated}<Flag>{e.truncated} truncated by SDK</Flag>{/if}
								{#if e.dropped}<Flag>{e.dropped} dropped: {e.reason}</Flag>{/if}
								{#each e.attributes as a (a.key)}
									<span class="ml-1 font-mono text-xs text-muted-foreground"
										>{a.key}={value(a.value)}{a.opt_in ? ' (opt-in)' : ''}</span
									>
								{/each}
							</span>
						</div>
					{/snippet}
				</VirtualRows>
			</div>
		</div>
	</TooltipProvider>

	<h2 class="mt-8 mb-2 text-lg font-semibold">Sources</h2>
	<Table>
		<TableHeader>
			<TableRow
				><TableHead>Instance</TableHead><TableHead>Service · SDK</TableHead><TableHead>Key</TableHead><TableHead
					>Seen</TableHead
				><TableHead>Loss over the instance's life</TableHead></TableRow
			>
		</TableHeader>
		<TableBody>
			{#each t.sources as s (s.instance_id)}
				<TableRow>
					<TableCell class="font-mono text-xs">{short(s.instance_id, 12)}</TableCell>
					<TableCell>{s.service} · <span class="font-mono text-xs">{s.sdk}</span></TableCell>
					<TableCell class="font-mono text-xs">{s.ingest_key}</TableCell>
					<TableCell class="font-mono text-xs">{time(s.first_seen)} – {time(s.last_seen)}</TableCell>
					<TableCell class="text-sm">
						{s.missing_by_seq} not received (by sequence), of which {s.events_refused} refused by the collector; SDK reported
						{s.reported_dropped_queue_full} queue-full, {s.reported_dropped_send_failed} send-failed, {s.reported_dropped_rejected}
						refused, {s.reported_gap_overflow} unattributed
					</TableCell>
				</TableRow>
			{/each}
		</TableBody>
	</Table>
{:else if result.state === 'missing'}
	<Notice kind="unknown">No events have been received for this operation. That is not proof that it did not run.</Notice>
{:else if result.state === 'forbidden'}
	<Notice kind="fail">Denied: {result.message}. The denial was recorded in the audit log.</Notice>
{:else if result.state === 'rateLimited'}
	<Notice kind="uncertain">Rate limited; retry in {result.retryAfter} s.</Notice>
{:else if result.state === 'needsAuth'}
	<Notice kind="fail">The token is missing, expired or revoked.</Notice>
{:else}
	<Notice kind="fail">The collector could not be reached: {result.message}</Notice>
{/if}
