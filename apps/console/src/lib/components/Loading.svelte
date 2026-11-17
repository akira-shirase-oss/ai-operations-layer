<script lang="ts">
	import { Skeleton } from '@aiops/ui/ui/skeleton';

	// Rows of the size of what is loading, so the page does not jump when it arrives.
	let { rows = 5, label = 'Loading' }: { rows?: number; label?: string } = $props();
</script>

<div role="status" aria-live="polite" class="grid gap-2">
	<span class="sr-only">{label}…</span>
	{#each Array.from({ length: rows }, (_, i) => i) as i (i)}<Skeleton class="h-12 w-full" />{/each}
</div>
