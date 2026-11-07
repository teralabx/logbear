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
	class="tnum inline-flex h-[18px] items-center rounded-[2px] border border-transparent px-1.5 text-[12px] leading-none whitespace-nowrap data-[state=fail]:bg-flare data-[state=fail]:font-semibold data-[state=fail]:text-flare-content data-[state=ok]:border-[color-mix(in_oklch,var(--color-success)_40%,transparent)] data-[state=ok]:text-success data-[state=running]:border-hair-strong data-[state=running]:text-base-content data-[state=signal]:bg-warning data-[state=signal]:font-semibold data-[state=signal]:text-warning-content"
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
