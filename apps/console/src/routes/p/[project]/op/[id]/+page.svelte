<script lang="ts">
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { api, type Result } from '#lib/api.js';
	import { micros, money, short, time, value } from '#lib/format.js';
	import type { Timeline } from '#lib/types.js';
	import StatusBadge from '#lib/components/StatusBadge.svelte';

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
</script>

<p class="muted"><a href={resolve('/')}>Projects</a> / <a href={resolve('/p/[project]', { project })}>{project}</a> / operation</p>

{#if result === null}
	<p class="muted">Loading…</p>
{:else if result.state === 'ok'}
	{@const t = result.data}
	<h1>{t.name ?? 'Operation (name not observed)'} <StatusBadge status={t.status} /></h1>
	<p class="mono muted">
		{t.operation_id}{t.request_id ? ` · request ${t.request_id}` : ''}{t.release ? ` · release ${t.release}` : ''}{t.deployment
			? ` · ${t.deployment}`
			: ''}
	</p>

	{#if t.status.state === 'uncertain' || t.flags.external_effect_unknown}
		<div class="notice uncertain">
			<strong>An external effect may have happened.</strong>
			<ul>
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
			This timeline is evidence, not authority: check the system that owns the effect (by business key)
			before any retry. Do not rerun blind.
		</div>
	{/if}
	{#if t.status.state === 'no_finish_observed'}
		<div class="notice unknown">
			<strong>No finish observed.</strong> The host never reported how this operation ended. A missing
			event is not proof that the work did not run.
		</div>
	{/if}
	{#if t.expired_by_retention}
		<div class="notice unknown">
			<strong>Some of this operation's detail has expired.</strong> Raw events are kept 14 days; those
			received before {time(t.retention_cutoff)} were deleted ({t.coverage.expired} of this operation's
			events). The outcome and counts that remain come from its summary.
		</div>
	{/if}
	{#if t.flags.start_not_observed && !t.expired_by_retention}
		<div class="notice unknown">The start of this operation was not received; it is known only from later events.</div>
	{/if}
	{#if t.flags.conflicting_finish}
		<div class="notice fail">
			<strong>Conflicting outcomes.</strong> More than one host declared how this operation ended, and the
			declarations disagree (see the finish events below). No single outcome is shown; ask the system that owns
			the work.
		</div>
	{/if}
	{#if t.flags.clock_skew_suspected}
		<div class="notice uncertain">
			Clock skew suspected: some event times disagree with emission order or arrive from the future. The
			order below is by event time; flagged rows may be out of place.
		</div>
	{/if}
	{#if t.events_truncated}
		<div class="notice uncertain">Only the first part of this very large operation is shown.</div>
	{/if}

	<div class="grid">
		<div class="panel">
			<dl class="kv">
				<dt>Events received</dt>
				<dd>{t.coverage.events_received}{t.coverage.sdk_gap_reports ? ` + ${t.coverage.sdk_gap_reports} gap reports` : ''}</dd>
				<dt>Missing</dt>
				<dd>
					{#if t.coverage.missing_known === 0 && !t.coverage.missing_unknown}none{/if}
					{#if t.coverage.missing_known > 0}{t.coverage.missing_known} proven missing{/if}
					{#if t.coverage.missing_unknown}{t.coverage.missing_known > 0 ? ' + ' : ''}an unknown number{/if}
				</dd>
				{#if t.coverage.expired > 0}
					<dt>Expired (retention)</dt>
					<dd>{t.coverage.expired}</dd>
				{/if}
				<dt>Dropped by the SDK (reported)</dt>
				<dd>{t.coverage.reported_dropped}</dd>
			</dl>
		</div>
		<div class="panel">
			<dl class="kv">
				<dt>Attempts</dt>
				<dd>
					{t.flags.failed_attempts} failed · {t.flags.uncertain_attempts} uncertain · {t.flags.retried_steps} steps retried
				</dd>
				<dt>Late events</dt>
				<dd>{t.flags.late_events}</dd>
			</dl>
		</div>
		<div class="panel">
			<dl class="kv">
				<dt>Cost (estimate, not a bill)</dt>
				<dd class="mono">
					{t.cost.attempts_with_estimate ? money(t.cost.estimate_micros, t.cost.currency) : 'no estimate reported'}
					{#if t.cost.mixed_currencies}<span class="flag">mixed currencies</span>{/if}
				</dd>
				<dt>Tokens</dt>
				<dd class="mono">{t.cost.input_tokens} in · {t.cost.output_tokens} out</dd>
			</dl>
		</div>
	</div>

	<h2>Steps</h2>
	{#if t.steps.length === 0}
		<p class="panel">No step events received.</p>
	{:else}
		<table>
			<thead>
				<tr>
					<th>Step</th>
					<th>Attempt</th>
					<th>Outcome</th>
					<th>Duration</th>
					<th>Model / version</th>
					<th>Error</th>
					<th>Est. cost</th>
				</tr>
			</thead>
			<tbody>
				{#each t.steps as s (s.step_id)}
					{#each s.attempts as a (a.attempt)}
						<tr>
							<td>
								{#if a.attempt === s.attempts[0]?.attempt}
									<strong>{s.name ?? 'name not observed'}</strong>
									<div class="muted">{s.kind ?? 'kind unknown'}{s.external_effect ? ' · external effect' : ''}</div>
								{/if}
							</td>
							<td>{a.attempt}</td>
							<td>
								<StatusBadge status={a} />
								{#if !a.start_observed}<span class="flag">start not received</span>{/if}
							</td>
							<td class="mono">{micros(a.duration_us)}</td>
							<td class="mono">{a.model ?? ''}{a.prompt_version ? ` · ${a.prompt_version}` : ''}{a.config_version ? ` · ${a.config_version}` : ''}</td>
							<td class="mono">{a.error_code ?? ''}{#if a.error_message}<div class="muted">{a.error_message}</div>{/if}</td>
							<td class="mono">{a.cost_estimate_micros !== undefined ? money(a.cost_estimate_micros, a.cost_currency ?? null) : ''}</td>
						</tr>
					{/each}
				{/each}
			</tbody>
		</table>
	{/if}

	<h2>Gaps</h2>
	{#if t.gaps.length === 0}
		<p class="panel">No gaps: every event this operation's SDK numbered was received{t.status.declared ? '' : ' so far'}.</p>
	{:else}
		<table>
			<thead><tr><th>What</th><th>How many</th><th>Where</th><th>Source</th></tr></thead>
			<tbody>
				{#each t.gaps as g, i (i)}
					<tr class="gap">
						<td>{GAP_TEXT[g.kind] ?? g.kind}{g.reason ? ` (${g.reason})` : ''}</td>
						<td>{g.count ?? 'unknown'}</td>
						<td class="mono">
							{#if g.op_seq_from !== undefined}#{g.op_seq_from}{g.op_seq_to !== undefined ? `–#${g.op_seq_to}` : ' onwards'}{/if}
							{#if g.from}{time(g.from)} – {time(g.to)}{/if}
						</td>
						<td class="mono">{short(g.instance_id)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	<h2>Events (event time · receive time)</h2>
	<table>
		<thead>
			<tr>
				<th>Event time</th>
				<th>Received</th>
				<th>Delay</th>
				<th>Event</th>
				<th>#</th>
				<th>Source</th>
				<th>Notes</th>
			</tr>
		</thead>
		<tbody>
			{#each t.entries as e (e.event_id)}
				<tr class:gap={e.type === 'gap.reported'}>
					<td class="mono">{time(e.event_time)}</td>
					<td class="mono">{time(e.received_at)}</td>
					<td class="mono">{micros(e.receive_delay_us)}</td>
					<td>
						{e.type}{e.name ? ` · ${e.name}` : ''}{e.attempt ? ` #${e.attempt}` : ''}
						{#if e.outcome}<StatusBadge status={e.outcome} />{/if}
						{#if e.dropped}<span class="flag">{e.dropped} dropped: {e.reason}</span>{/if}
						{#each e.attributes as a (a.key)}
							<div class="mono muted">{a.key}={value(a.value)}{a.opt_in ? ' (opt-in)' : ''}</div>
						{/each}
					</td>
					<td class="mono">{e.op_seq || '—'}</td>
					<td class="mono" title="instance {e.instance_id}">{e.ingest_key} · {short(e.instance_id)}</td>
					<td>
						{#each e.flags as f (f)}<span class="flag">{f.replaceAll('_', ' ')}</span>{/each}
						{#if e.withheld_attributes}<span class="flag">{e.withheld_attributes} attributes withheld by policy</span>{/if}
						{#if e.redactions}<span class="flag">{e.redactions} redacted</span>{/if}
						{#if e.overflowed_names}<span class="flag">{e.overflowed_names} names over budget, stored as _overflow</span>{/if}
						{#if e.truncated}<span class="flag">{e.truncated} truncated by SDK</span>{/if}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>

	<h2>Sources</h2>
	<table>
		<thead><tr><th>Instance</th><th>Service · SDK</th><th>Key</th><th>Seen</th><th>Loss over the instance's life</th></tr></thead>
		<tbody>
			{#each t.sources as s (s.instance_id)}
				<tr>
					<td class="mono">{short(s.instance_id, 12)}</td>
					<td>{s.service} · <span class="mono">{s.sdk}</span></td>
					<td class="mono">{s.ingest_key}</td>
					<td class="mono">{time(s.first_seen)} – {time(s.last_seen)}</td>
					<td>
						{s.missing_by_seq} not received (by sequence); SDK reported {s.reported_dropped_queue_full} queue-full,
						{s.reported_dropped_send_failed} send-failed, {s.reported_dropped_rejected} refused,
						{s.reported_gap_overflow} unattributed
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{:else if result.state === 'missing'}
	<p class="notice unknown">No events have been received for this operation. That is not proof that it did not run.</p>
{:else if result.state === 'forbidden'}
	<p class="notice fail">Denied: {result.message}. The denial was recorded in the audit log.</p>
{:else if result.state === 'rateLimited'}
	<p class="notice uncertain">Rate limited; retry in {result.retryAfter} s.</p>
{:else if result.state === 'needsAuth'}
	<p class="notice fail">The token is missing, expired or revoked.</p>
{:else}
	<p class="notice fail">The collector could not be reached: {result.message}</p>
{/if}
