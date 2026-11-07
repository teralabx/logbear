<script lang="ts">
	import { fmtBytes } from '#lib/format';

	/**
	 * Bytes used against a cap.
	 *
	 * A percentage on its own is not actionable — you need both absolute numbers
	 * to decide whether to raise the cap or prune. Both are shown. The bar turns
	 * amber past 80% and flare past 95%, because past 95% the next run starts
	 * evicting the oldest one.
	 */
	let {
		used,
		cap,
		label = 'Stored',
		note = ''
	}: { used: number; cap: number; label?: string; note?: string } = $props();

	const pct = $derived(cap > 0 ? Math.min(1, used / cap) : 0);
	const state = $derived(pct >= 0.95 ? 'critical' : pct >= 0.8 ? 'warn' : 'ok');
</script>

<div class="group" data-state={state}>
	<div class="mb-[5px] flex items-baseline justify-between gap-3">
		<span class="label-xs">{label}</span>
		<span class="tnum inline-flex items-baseline gap-1.5 text-[12px]">
			<span class="font-semibold">{fmtBytes(used)}</span>
			<span class="meta">/ {fmtBytes(cap)}</span>
			<span class="meta min-w-[34px] text-right">{Math.round(pct * 100)}%</span>
		</span>
	</div>
	<div
		class="relative h-1.5 overflow-hidden rounded-[1px] bg-hair"
		role="meter"
		aria-valuenow={used}
		aria-valuemin={0}
		aria-valuemax={cap}
		aria-label="{label}: {fmtBytes(used)} of {fmtBytes(cap)}"
	>
		<div
			class="h-full bg-base-content transition-[width] duration-300 ease-out group-data-[state=critical]:bg-flare group-data-[state=warn]:bg-warning"
			style="width: {(pct * 100).toFixed(2)}%"
		></div>
		<div class="absolute top-0 bottom-0 left-[80%] w-px bg-base-100 opacity-70"></div>
	</div>
	{#if note}<p class="meta mt-1.5 text-[12px]">{note}</p>{/if}
</div>
