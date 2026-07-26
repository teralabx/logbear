<script lang="ts">
	import type { Run } from '$lib/api/types';

	/**
	 * Exit status encoded twice — in colour and in form — so it survives a
	 * monochrome screenshot and red-green colour blindness. Passing is a hollow
	 * outline; failing is filled; running is a breathing outline.
	 */
	let { run, showLabel = true }: { run: Run; showLabel?: boolean } = $props();

	const state = $derived(
		run.ended_at === null
			? 'running'
			: run.signal
				? 'signal'
				: run.exit_code === 0
					? 'ok'
					: 'fail'
	);

	const text = $derived(
		state === 'running'
			? 'running'
			: state === 'signal'
				? (run.signal ?? 'signal')
				: state === 'ok'
					? showLabel
						? 'exit 0'
						: '0'
					: showLabel
						? `exit ${run.exit_code}`
						: String(run.exit_code)
	);
</script>

<span
	class="badge tnum"
	data-state={state}
	class:is-running={state === 'running'}
	title={state === 'running'
		? 'Still running'
		: state === 'ok'
			? 'Exited 0'
			: state === 'signal'
				? `Killed by ${run.signal}`
				: `Exited ${run.exit_code}`}
>
	{text}
</span>

<style>
	.badge {
		display: inline-flex;
		align-items: center;
		height: 18px;
		padding: 0 6px;
		border-radius: 2px;
		font-size: 11px;
		line-height: 1;
		border: 1px solid transparent;
		white-space: nowrap;
	}
	[data-state='ok'] {
		color: var(--color-success);
		border-color: color-mix(in oklch, var(--color-success) 40%, transparent);
	}
	[data-state='fail'] {
		background: var(--color-flare);
		color: #180703;
		font-weight: 600;
	}
	[data-state='signal'] {
		background: var(--color-warning);
		color: var(--color-warning-content);
		font-weight: 600;
	}
	[data-state='running'] {
		color: var(--color-base-content);
		border-color: var(--rl-hair-strong);
	}
</style>
