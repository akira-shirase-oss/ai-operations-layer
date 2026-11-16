<script lang="ts">
	import { stateText } from '#lib/format.js';
	import type { Status } from '#lib/types.js';

	let { status }: { status: Pick<Status, 'state' | 'class' | 'reason'> & { declared?: boolean } } =
		$props();
</script>

<span class="badge {status.state}" title={status.declared === false ? 'not declared by the host' : 'declared by the host'}>
	{stateText(status.state)}{#if status.class}: {status.class}{/if}{#if status.reason}: {status.reason}{/if}
</span>
