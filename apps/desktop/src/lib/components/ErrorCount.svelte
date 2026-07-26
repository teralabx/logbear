<script lang="ts">
	import { pluralise } from '$lib/format';

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
	<span class="zero" class:sm={size === 'sm'}>none</span>
{:else}
	<span class="wrap" class:sm={size === 'sm'}>
		<span class="total tnum" class:has-new={neu > 0}>{pluralise(total, label)}</span>
		{#if neu > 0}
			<span class="new tnum" title="{neu} of these have never been seen in this project before">
				{neu} new
			</span>
		{/if}
	</span>
{/if}

<style>
	.wrap {
		display: inline-flex;
		align-items: stretch;
		height: 20px;
		border-radius: 2px;
		overflow: hidden;
		font-size: 11px;
		line-height: 1;
		white-space: nowrap;
	}
	.wrap.sm {
		height: 17px;
		font-size: 10px;
	}
	.total {
		display: inline-flex;
		align-items: center;
		padding: 0 7px;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		color: var(--rl-lvl-error);
	}
	.total.has-new {
		border-right: 0;
		border-radius: 2px 0 0 2px;
	}
	.new {
		display: inline-flex;
		align-items: center;
		padding: 0 7px;
		background: var(--color-flare);
		color: #180703;
		font-weight: 600;
		border-radius: 0 2px 2px 0;
	}
	.zero {
		font-size: 11px;
		color: var(--rl-meta);
	}
	.zero.sm {
		font-size: 10px;
	}
</style>
