<script lang="ts" generics="T">
	import type { Snippet } from 'svelte';

	// A windowed list for long timelines (up to 5,000 events, or as many step attempts): only
	// the rows in view, plus a margin, are in the DOM; spacers keep the scrollbar honest. Rows
	// have a fixed height. `label` names the list for assistive technology (and for tests).
	let {
		rows,
		label,
		rowHeight = 56,
		height = 560,
		overscan = 8,
		key,
		row
	}: {
		rows: T[];
		label: string;
		rowHeight?: number;
		height?: number;
		overscan?: number;
		key: (item: T) => string;
		row: Snippet<[T, number]>;
	} = $props();

	let scrollTop = $state(0);
	const viewport = $derived(Math.min(height, rows.length * rowHeight));
	const first = $derived(Math.max(0, Math.floor(scrollTop / rowHeight) - overscan));
	const last = $derived(Math.min(rows.length, Math.ceil((scrollTop + viewport) / rowHeight) + overscan));
	const visible = $derived(rows.slice(first, last));
</script>

<div
	class="overflow-y-auto rounded-xl border"
	style="height: {viewport + 2}px"
	role="rowgroup"
	aria-label={label}
	onscroll={(e) => (scrollTop = e.currentTarget.scrollTop)}
>
	<div style="height: {first * rowHeight}px" aria-hidden="true"></div>
	{#each visible as item, i (key(item))}
		<div role="row" class="overflow-hidden border-b last:border-b-0" style="height: {rowHeight}px">
			{@render row(item, first + i)}
		</div>
	{/each}
	<div style="height: {(rows.length - last) * rowHeight}px" aria-hidden="true"></div>
</div>
