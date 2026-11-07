<script lang="ts">
	import { pluralise } from '#lib/format';

	/**
	 * "7 errors (1 new)".
	 *
	 * The `new` segment is the single most valuable number in the run history —
	 * it is the difference between a familiar failure and a regression. It gets
	 * flare, filled, at full weight. When nothing is new the segment does not
	 * render at all, so flare stays rare enough to mean something.
	 */
	let {
		total,
		neu = 0,
		label = 'error',
		size = 'md'
	}: { total: number; neu?: number; label?: string; size?: 'sm' | 'md' } = $props();
</script>

{#if total === 0}
	<span class="text-meta {size === 'sm' ? 'text-[12px]' : 'text-[12px]'}">none</span>
{:else}
	<span
		class="inline-flex items-stretch overflow-hidden rounded-[2px] leading-none whitespace-nowrap {size ===
		'sm'
			? 'h-[17px] text-[12px]'
			: 'h-5 text-[12px]'}"
	>
		<span
			class="tnum inline-flex items-center border-hair px-[7px] text-lvl-error {neu > 0
				? 'rounded-l-[2px] border-y border-l'
				: 'rounded-[2px] border'}">{pluralise(total, label)}</span
		>
		{#if neu > 0}
			<span
				class="tnum inline-flex items-center rounded-r-[2px] bg-flare px-[7px] font-semibold text-flare-content"
				title="{neu} of these have never been seen in this project before"
			>
				{neu} new
			</span>
		{/if}
	</span>
{/if}
