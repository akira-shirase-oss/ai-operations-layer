<script lang="ts">
	import { Badge } from '@aiops/ui/ui/badge';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import CircleHelp from '@lucide/svelte/icons/circle-help';
	import CircleDashed from '@lucide/svelte/icons/circle-dashed';
	import Split from '@lucide/svelte/icons/split';
	import CircleSlash from '@lucide/svelte/icons/circle-slash';
	import { statusCodes, statusText } from '#lib/format.js';
	import type { Status } from '#lib/types.js';

	// An attempt's status carries its host error code as `error_code`; an operation's as `code`.
	let {
		status
	}: {
		status: Pick<Status, 'state' | 'class' | 'reason'> & { code?: string; error_code?: string; declared?: boolean };
	} = $props();
	const shown = $derived({ ...status, code: status.code ?? status.error_code });
	const cancelled = $derived(statusText(shown) === 'cancelled');

	// Each state: its own hue (never the accent), an icon and a word.
	const look: Record<string, string> = {
		succeeded: 'bg-state-ok-soft text-state-ok-ink border-state-ok/40',
		failed: 'bg-state-fail-soft text-state-fail-ink border-state-fail/40',
		uncertain: 'bg-state-uncertain-soft text-state-uncertain-ink border-state-uncertain',
		conflicting: 'bg-state-conflict-soft text-state-conflict-ink border-state-conflict/50',
		no_finish_observed: 'bg-state-unknown-soft text-state-unknown-ink border-state-unknown border-dashed',
		// Stopped on request: neither a fault nor a success.
		cancelled: 'bg-state-unknown-soft text-state-unknown-ink border-state-unknown'
	};
</script>

<Badge
	variant="outline"
	class="badge {status.state} {cancelled ? look['cancelled'] : (look[status.state] ?? '')} max-sm:max-w-full max-sm:whitespace-normal max-sm:text-left"
	title="{status.declared === false ? 'not declared by the host' : 'declared by the host'}: {statusCodes(shown)}"
	data-code={statusCodes(shown)}
>
	{#if status.state === 'succeeded'}<CircleCheck aria-hidden="true" />
	{:else if cancelled}<CircleSlash aria-hidden="true" />
	{:else if status.state === 'failed'}<CircleX aria-hidden="true" />
	{:else if status.state === 'uncertain'}<CircleHelp aria-hidden="true" />
	{:else if status.state === 'conflicting'}<Split aria-hidden="true" />
	{:else}<CircleDashed aria-hidden="true" />{/if}{statusText(shown)}
</Badge>
