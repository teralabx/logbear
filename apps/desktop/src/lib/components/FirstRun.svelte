<script lang="ts">
	import { onMount } from 'svelte';
	import ErrorNote from './ErrorNote.svelte';
	import Mark from './Mark.svelte';
	import Toggle from './Toggle.svelte';
	import { apiError, finishFirstRun, installStep } from '$lib/api';
	import {
		STEP_COPY,
		chosenSteps,
		installInOrder,
		installable,
		installed,
		readInstall,
		undetectedAgents,
		type InstallRead
	} from '$lib/install';
	import type { InstallStep } from '$lib/api/types';

	/**
	 * Asked once, before the app changes anything. Every step starts off, and nothing runs
	 * until the button is pressed: "Not now" leaves the machine exactly as it was.
	 */
	let { ondone }: { ondone: () => void } = $props();

	let read = $state<InstallRead>({ state: 'loading' });
	let chosen = $state<Set<InstallStep>>(new Set());
	let progress = $state<{ done: number; total: number } | null>(null);
	let failures = $state<string[]>([]);
	let attempted = $state(false);
	let finishError = $state<string | null>(null);
	let finishing = $state(false);

	const status = $derived(read.state === 'ready' ? read.status : null);
	const offered = $derived(status ? installable(status) : []);
	const present = $derived(status ? installed(status) : []);
	const missing = $derived(status ? undetectedAgents(status) : []);
	const count = $derived(status ? chosenSteps(status, chosen).length : 0);
	const busy = $derived(progress !== null || finishing);

	async function load() {
		read = { state: 'loading' };
		read = await readInstall();
	}

	onMount(() => {
		void load();
	});

	function choose(step: InstallStep, on: boolean) {
		const next = new Set(chosen);
		if (on) next.add(step);
		else next.delete(step);
		chosen = next;
	}

	async function finish() {
		finishing = true;
		finishError = null;
		try {
			await finishFirstRun();
			ondone();
		} catch (err) {
			finishError = apiError(err);
		} finally {
			finishing = false;
		}
	}

	async function setUp() {
		if (!status) return;
		const steps = chosenSteps(status, chosen);
		attempted = true;
		progress = { done: 0, total: steps.length };
		failures = await installInOrder(steps, installStep, (_, done) => {
			progress = { done, total: steps.length };
		});
		progress = null;
		if (failures.length > 0) {
			chosen = new Set();
			await load();
			return;
		}
		await finish();
	}
</script>

<div class="flex h-screen w-screen flex-col overflow-hidden bg-base-100">
	<div class="h-[46px] flex-none" data-tauri-drag-region></div>
	<div class="pane flex-1 px-6 pb-6">
		<div class="mx-auto max-w-[680px]">
			<div class="flex items-center gap-2.5">
				<Mark size={26} />
				<h2 class="text-[15px] font-semibold">Set up LogBear</h2>
			</div>
			<p class="mt-3 text-[12.5px] leading-[1.6]">
				{attempted ? 'Your chosen steps have run.' : 'Nothing on this machine has been changed yet.'}
				Choose what LogBear may set up. Each step can be undone from Settings → Install, and anything
				you leave off is there too.
			</p>

			{#if failures.length > 0}
				<div class="mt-3 rounded border border-warning/40 px-3 py-2" role="alert">
					<p class="text-[12px] font-medium text-warning">
						{failures.length === 1 ? 'One step' : `${failures.length} steps`} could not be set up
					</p>
					<ul class="mt-1 flex flex-col gap-0.5">
						{#each failures as failure (failure)}
							<li class="meta text-[12px] leading-[1.6]">{failure}</li>
						{/each}
					</ul>
				</div>
			{/if}
			{#if read.state === 'failed'}
				<ErrorNote
					what="Could not read what is installed."
					detail={read.error}
					retry={() => void load()}
				/>
			{:else if finishError}
				<ErrorNote
					what="Could not save your choice."
					detail={finishError}
					retry={() => void finish()}
				/>
			{/if}

			{#if read.state === 'loading'}
				<p class="meta mt-4 text-[12px]">Looking at what is on this machine…</p>
			{:else if status}
				{#if offered.length > 0}
					<div class="mt-3 rule-t">
						{#each offered as step (step)}
							<div class="rule-b py-1.5">
								<Toggle
									checked={chosen.has(step)}
									label={STEP_COPY[step].title}
									hint={STEP_COPY[step].what}
									disabled={busy}
									onchange={(on) => choose(step, on)}
								/>
								<p class="ml-10 max-w-[60ch] pb-1 text-[12px] leading-[1.5] opacity-75">
									{status[step].detail}
								</p>
							</div>
						{/each}
					</div>
				{:else}
					<p class="mt-3 text-[12.5px]">Everything LogBear can set up here already is.</p>
				{/if}
				{#if present.length > 0}
					<p class="meta mt-3 text-[12px] leading-[1.6]">
						Already set up: {present.map((s) => STEP_COPY[s].title).join(', ')}.
					</p>
				{/if}
				{#if missing.length > 0}
					<p class="meta mt-1 text-[12px] leading-[1.6]">
						Not found on this machine: {missing.join(', ')}. Install one later and add it from
						Settings.
					</p>
				{/if}
			{/if}

			<div class="mt-4 flex flex-wrap items-center gap-2">
				{#if offered.length > 0}
					<button
						type="button"
						class="btn btn-primary btn-sm h-auto px-[11px] py-[4px] text-[12px] font-normal disabled:border-primary disabled:bg-primary disabled:text-primary-content disabled:opacity-45"
						disabled={busy || count === 0}
						onclick={() => void setUp()}
					>
						{#if progress}
							Setting up… {progress.done}/{progress.total}
						{:else}
							Set up {count === 1 ? '1 selected step' : `${count} selected steps`}
						{/if}
					</button>
					<button
						type="button"
						class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[12px] font-normal hover:border-base-content hover:bg-transparent disabled:border-hair-strong disabled:bg-transparent disabled:text-base-content disabled:opacity-45"
						disabled={busy}
						onclick={() => (chosen = new Set(offered))}>Select all</button
					>
				{/if}
				<button
					type="button"
					class="ml-auto cursor-pointer text-[12px] text-meta hover:text-base-content disabled:cursor-default disabled:opacity-50"
					disabled={busy}
					onclick={() => void finish()}
					>{offered.length > 0 && failures.length === 0 ? 'Not now' : 'Continue'}</button
				>
			</div>
			<p class="meta mt-3 text-[12px] leading-[1.6]">
				Whatever you choose, the app starts the logbeard daemon when it needs one. That stores data
				only in ~/.logbear.
			</p>
		</div>
	</div>
</div>
