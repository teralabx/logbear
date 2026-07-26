<script lang="ts">
	import { fmtBytes } from '$lib/format';

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

<div class="meter" data-state={state}>
	<div class="head">
		<span class="label-xs">{label}</span>
		<span class="tnum figures">
			<span class="used">{fmtBytes(used)}</span>
			<span class="meta">/ {fmtBytes(cap)}</span>
			<span class="pct meta">{Math.round(pct * 100)}%</span>
		</span>
	</div>
	<div
		class="track"
		role="meter"
		aria-valuenow={used}
		aria-valuemin={0}
		aria-valuemax={cap}
		aria-label="{label}: {fmtBytes(used)} of {fmtBytes(cap)}"
	>
		<div class="fill" style="width: {(pct * 100).toFixed(2)}%"></div>
		<div class="tick" style="left: 80%"></div>
	</div>
	{#if note}<p class="note meta">{note}</p>{/if}
</div>

<style>
	.head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 12px;
		margin-bottom: 5px;
	}
	.figures {
		font-size: 12px;
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
	}
	.used {
		font-weight: 600;
	}
	.pct {
		min-width: 34px;
		text-align: right;
	}
	.track {
		position: relative;
		height: 6px;
		background: var(--rl-hair);
		border-radius: 1px;
		overflow: hidden;
	}
	.fill {
		height: 100%;
		background: var(--color-base-content);
		transition: width 300ms ease-out;
	}
	[data-state='warn'] .fill {
		background: var(--color-warning);
	}
	[data-state='critical'] .fill {
		background: var(--color-flare);
	}
	.tick {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 1px;
		background: var(--color-base-100);
		opacity: 0.7;
	}
	.note {
		margin-top: 6px;
		font-size: 11px;
	}
</style>
