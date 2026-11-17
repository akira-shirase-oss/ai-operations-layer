<script lang="ts">
	import type { Snippet } from 'svelte';
	import { Alert, AlertDescription, AlertTitle } from '@aiops/ui/ui/alert';
	import CircleHelp from '@lucide/svelte/icons/circle-help';
	import CircleDashed from '@lucide/svelte/icons/circle-dashed';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';

	/** `uncertain` (an effect may have happened), `unknown` (missing data), `fail`. */
	let { kind, title, children }: { kind: 'uncertain' | 'unknown' | 'fail'; title?: string; children: Snippet } =
		$props();

	const look = {
		uncertain: 'border-state-uncertain bg-state-uncertain-soft text-state-uncertain-ink',
		unknown: 'border-state-unknown border-dashed bg-state-unknown-soft text-state-unknown-ink',
		fail: 'border-state-fail/50 bg-state-fail-soft text-state-fail-ink'
	};
</script>

<Alert class="notice {kind} my-2 {look[kind]}">
	{#if kind === 'uncertain'}<CircleHelp aria-hidden="true" />
	{:else if kind === 'unknown'}<CircleDashed aria-hidden="true" />
	{:else}<TriangleAlert aria-hidden="true" />{/if}
	{#if title}<AlertTitle>{title}</AlertTitle>{/if}
	<AlertDescription class="text-inherit">{@render children()}</AlertDescription>
</Alert>
