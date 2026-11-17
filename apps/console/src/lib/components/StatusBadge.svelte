<script lang="ts">
	import { Badge } from '@aiops/ui/ui/badge';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import CircleHelp from '@lucide/svelte/icons/circle-help';
	import CircleDashed from '@lucide/svelte/icons/circle-dashed';
	import Split from '@lucide/svelte/icons/split';
	import { stateText } from '#lib/format.js';
	import type { Status } from '#lib/types.js';

	let { status }: { status: Pick<Status, 'state' | 'class' | 'reason'> & { declared?: boolean } } =
		$props();

	// Each state: its own hue (never the accent), an icon and a word.
	const look: Record<string, string> = {
		succeeded: 'bg-state-ok-soft text-state-ok-ink border-state-ok/40',
		failed: 'bg-state-fail-soft text-state-fail-ink border-state-fail/40',
		uncertain: 'bg-state-uncertain-soft text-state-uncertain-ink border-state-uncertain',
		conflicting: 'bg-state-conflict-soft text-state-conflict-ink border-state-conflict/50',
		no_finish_observed: 'bg-state-unknown-soft text-state-unknown-ink border-state-unknown border-dashed'
	};
</script>

<Badge
	variant="outline"
	class="badge {status.state} {look[status.state] ?? ''} max-sm:max-w-full max-sm:whitespace-normal max-sm:text-left"
	title={status.declared === false ? 'not declared by the host' : 'declared by the host'}
>
	{#if status.state === 'succeeded'}<CircleCheck aria-hidden="true" />
	{:else if status.state === 'failed'}<CircleX aria-hidden="true" />
	{:else if status.state === 'uncertain'}<CircleHelp aria-hidden="true" />
	{:else if status.state === 'conflicting'}<Split aria-hidden="true" />
	{:else}<CircleDashed aria-hidden="true" />{/if}{stateText(status.state)}{#if status.class}: {status.class}{/if}{#if status.reason}: {status.reason}{/if}
</Badge>
