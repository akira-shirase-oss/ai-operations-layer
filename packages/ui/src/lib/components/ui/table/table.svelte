<script lang="ts">
	import { cn, type WithElementRef } from "#lib/utils.js";
	import type { HTMLTableAttributes } from "svelte/elements";

	let {
		ref = $bindable(null),
		class: className,
		children,
		...restProps
	}: WithElementRef<HTMLTableAttributes> = $props();
</script>

<!-- Focusable, so a table wider than the screen can be scrolled from the keyboard. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div
	data-slot="table-container"
	tabindex="0"
	class="relative w-full overflow-x-auto rounded-md outline-none focus-visible:ring-4 focus-visible:ring-ring/25"
>
	<table bind:this={ref} data-slot="table" class={cn("w-full caption-bottom text-sm", className)} {...restProps}>
		{@render children?.()}
	</table>
</div>
