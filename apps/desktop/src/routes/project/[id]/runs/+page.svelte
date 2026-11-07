<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import ErrorCount from '$lib/components/ErrorCount.svelte';
	import { apiError, listRuns, projectRef, runErrorCounts } from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtCommand,
		fmtDuration,
		fmtStamp,
		runDuration,
		runElapsed,
		shortSha,
		tildePath
	} from '$lib/format';
	import type { Run } from '$lib/api/types';
	import { INLINE } from '$lib/icons';
	import { ArrowRight } from '@lucide/svelte';

	/**
	 * Run history.
	 *
	 * The column that earns this screen is `errors`: a total plus, when it is
	 * non-zero, how many of those signatures this project has never seen before.
	 * "7 errors" is noise you have already triaged. "7 errors (1 new)" is a
	 * regression with a timestamp on it.
	 */

	const projectId = $derived(Number(page.params.id));
	const project = $derived(app.project(projectId));

	let runs = $state<Run[]>([]);
	let errorsByRun = $state<Record<number, { total: number; neu: number }>>({});
	let loading = $state(true);
	let loadError = $state<string | null>(null);
	let selected = $state<number[]>([]);
	let onlyFailed = $state(false);
	let filter = $state('');
	let hasMore = $state(false);
	let loadingMore = $state(false);

	const PAGE = 100;

	/** Error totals for a page of runs, from one grouped query. */
	async function countErrors(id: number, rs: Run[]) {
		const counts = await runErrorCounts(
			projectRef(id),
			rs.map((r) => r.id)
		);
		const map = { ...errorsByRun };
		for (const c of counts) map[c.run_id] = { total: c.total, neu: c.new };
		errorsByRun = map;
	}

	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	$effect(() => {
		const id = projectId;
		if (!Number.isFinite(id)) return;
		let cancelled = false;
		loading = true;
		loadError = null;
		selected = [];

		(async () => {
			try {
				const ref = projectRef(id);
				const rs = await listRuns(ref, PAGE, 0);
				if (cancelled) return;
				runs = rs;
				hasMore = rs.length === PAGE;
				errorsByRun = {};
				await countErrors(id, rs);
				if (cancelled) return;
				loading = false;
			} catch (err) {
				if (!cancelled) {
					loadError = apiError(err);
					loading = false;
				}
			}
		})();

		return () => {
			cancelled = true;
		};
	});

	async function loadMore() {
		if (loadingMore) return;
		loadingMore = true;
		try {
			const id = projectId;
			const next = await listRuns(projectRef(id), PAGE, runs.length);
			runs = runs.concat(next);
			hasMore = next.length === PAGE;
			await countErrors(id, next);
		} catch (err) {
			loadError = apiError(err);
		} finally {
			loadingMore = false;
		}
	}

	const shown = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		return runs.filter((r) => {
			if (onlyFailed && (r.exit_code === 0 || r.exit_code === null)) return false;
			if (!q) return true;
			return (
				fmtCommand(r.command).toLowerCase().includes(q) ||
				(r.git_branch ?? '').toLowerCase().includes(q) ||
				(r.git_sha ?? '').toLowerCase().includes(q)
			);
		});
	});

	const withNew = $derived(shown.filter((r) => (errorsByRun[r.id]?.neu ?? 0) > 0).length);

	function toggleSelect(id: number) {
		if (selected.includes(id)) {
			selected = selected.filter((x) => x !== id);
		} else if (selected.length < 2) {
			selected = [...selected, id];
		} else {
			// Third pick replaces the older of the two, so shift-comparing stays fluid.
			selected = [selected[1], id];
		}
	}

	function openDiff() {
		if (selected.length !== 2) return;
		const [a, b] = [...selected].sort((x, y) => {
			const ra = runs.find((r) => r.id === x);
			const rb = runs.find((r) => r.id === y);
			return (ra?.started_at ?? 0) - (rb?.started_at ?? 0);
		});
		void goto(`/project/${projectId}/diff?a=${a}&b=${b}`);
	}

	function open(r: Run) {
		void goto(`/project/${projectId}/runs/${r.id}`);
	}
</script>

<PageHeader
	title="Runs"
	sub={project ? `${project.name} · ${runs.length}${hasMore ? '+' : ''} runs` : ''}
>
	{#snippet actions()}
		<input
			class="w-[230px] rounded-[2px] border border-hair bg-transparent px-[8px] py-[3px] text-[12px] text-base-content outline-none focus:border-hair-strong"
			type="search"
			placeholder="Filter by command, branch, SHA…"
			bind:value={filter}
			aria-label="Filter runs"
		/>
		<button
			type="button"
			class={[
				'cursor-pointer rounded-[2px] border px-[8px] py-[4px] text-[12px] tracking-[0.1em] uppercase',
				onlyFailed
					? 'border-base-content bg-base-content text-base-100'
					: 'border-hair bg-transparent text-meta'
			]}
			aria-pressed={onlyFailed}
			onclick={() => (onlyFailed = !onlyFailed)}>failed only</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="p-4"><ErrorNote what="Could not read the run history." detail={loadError} /></div>
{:else if loading}
	<p class="meta p-4">Reading run history…</p>
{:else if runs.length === 0}
	<div class="p-4">
		<EmptyState
			title="No runs stored for this project"
			body="Every command you run through logbear lands here with its output, its exit code, and the git state that produced it."
			command="logbear npm test"
		/>
	</div>
{:else}
	{#if withNew > 0}
		<p
			class="rule-b m-0 flex flex-none items-center gap-[8px] bg-[color-mix(in_oklch,var(--color-flare)_8%,transparent)] px-4 py-[7px] text-[12px]"
		>
			<span class="h-[6px] w-[6px] flex-none rounded-[1px] bg-flare"></span>
			{withNew}
			{withNew === 1 ? 'run' : 'runs'} below produced an error signature this project had never seen before.
		</p>
	{/if}

	<div class="pane @container min-h-0 flex-1">
		<table class="w-full table-fixed border-collapse text-[12px]">
			<thead>
				<tr>
					<th
						class="sticky top-0 z-[2] w-[34px] border-b border-hair-strong bg-base-100 py-[7px] pr-0 pl-[8px] text-left text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col"><span class="sr-only">Compare</span></th
					>
					<th
						class="sticky top-0 z-[2] w-[138px] border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-left text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Started</th
					>
					<th
						class="sticky top-0 z-[2] overflow-hidden border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-left text-[12px] font-medium tracking-[0.1em] text-ellipsis text-meta uppercase whitespace-nowrap"
						scope="col">Command</th
					>
					<th
						class="sticky top-0 z-[2] w-[230px] @max-[1000px]:w-[92px] border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-left text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Commit</th
					>
					<th
						class="sticky top-0 z-[2] w-[92px] border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-right text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Duration</th
					>
					<th
						class="sticky top-0 z-[2] w-[80px] border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-left text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Exit</th
					>
					<th
						class="sticky top-0 z-[2] w-[150px] border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-left text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Errors</th
					>
					<th
						class="sticky top-0 z-[2] w-[62px] @max-[1000px]:hidden border-b border-hair-strong bg-base-100 px-[10px] py-[7px] text-right text-[12px] font-medium tracking-[0.1em] text-meta uppercase whitespace-nowrap"
						scope="col">Warn</th
					>
				</tr>
			</thead>
			<tbody>
				{#each shown as r (r.id)}
					{@const err = errorsByRun[r.id] ?? { total: r.counters.n_error, neu: 0 }}
					<tr
						class={[
							'cursor-pointer border-b border-b-hair border-l-2',
							r.ended_at === null
								? 'border-l-success'
								: err.neu > 0
									? 'border-l-flare'
									: r.exit_code !== null && r.exit_code !== 0
										? 'border-l-[color-mix(in_oklch,var(--color-flare)_55%,transparent)]'
										: 'border-l-transparent',
							selected.includes(r.id)
								? 'bg-[color-mix(in_oklch,var(--color-base-content)_9%,transparent)]'
								: 'hover:bg-[color-mix(in_oklch,var(--color-base-content)_5%,transparent)]'
						]}
						onclick={() => open(r)}
						onkeydown={(e) => {
							if (e.key === 'Enter') open(r);
						}}
						tabindex="0"
					>
						<td class="w-[30px] py-[5px] pr-0 pl-[8px] align-middle whitespace-nowrap">
							<button
								type="button"
								class={[
									'grid h-[17px] w-[17px] cursor-pointer place-items-center rounded-[2px] border text-[12px] leading-none font-semibold text-base-100',
									selected.includes(r.id)
										? 'border-base-content bg-base-content'
										: 'border-hair-strong bg-transparent'
								]}
								aria-pressed={selected.includes(r.id)}
								aria-label="Select run {r.id} for comparison"
								title="Select two runs to diff them"
								onclick={(e) => {
									e.stopPropagation();
									toggleSelect(r.id);
								}}
							>
								{#if selected.includes(r.id)}{selected.indexOf(r.id) === 0 ? 'A' : 'B'}{/if}
							</button>
						</td>
						<td
							class="meta tnum overflow-hidden px-[10px] py-[5px] align-middle text-ellipsis whitespace-nowrap"
							title={fmtStamp(r.started_at)}>{fmtStamp(r.started_at)}</td
						>
						<td
							class="overflow-hidden px-[10px] py-[5px] align-middle text-ellipsis whitespace-nowrap"
							><code class="text-[12px]">{fmtCommand(r.command)}</code></td
						>
						<td class="tnum overflow-hidden px-[10px] py-[5px] align-middle text-ellipsis whitespace-nowrap">
							<span class="text-base-content">{shortSha(r.git_sha)}</span>
							<span class="meta ml-[7px] text-[12px] @max-[1000px]:hidden">{r.git_branch ?? '—'}</span>
							{#if r.git_dirty_hash}<span
									class="ml-[6px] text-[12px] tracking-[0.1em] text-warning uppercase @max-[1000px]:hidden"
									title="This run started with uncommitted changes, so the commit above does not fully describe it"
									>uncommitted</span
								>{/if}
						</td>
						<td
							class="meta tnum w-[82px] px-[10px] py-[5px] text-right align-middle whitespace-nowrap"
						>
							{r.ended_at === null
								? fmtDuration(runElapsed(r, now))
								: fmtDuration(runDuration(r))}
						</td>
						<td class="w-[62px] px-[10px] py-[5px] align-middle whitespace-nowrap"
							><ExitBadge run={r} showLabel={false} /></td
						>
						<td class="w-[148px] px-[10px] py-[5px] align-middle whitespace-nowrap"
							><ErrorCount total={err.total} neu={err.neu} size="sm" /></td
						>
						<td
							class="meta tnum px-[10px] py-[5px] text-right align-middle whitespace-nowrap @max-[1000px]:hidden"
							>{r.counters.n_warn || '—'}</td
						>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if shown.length === 0}
			<p class="meta p-4 text-[12px]">No runs match this filter.</p>
		{/if}
		{#if hasMore}
			<div class="px-4 py-3">
				<button
					type="button"
					class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[12px] tracking-[0.1em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content disabled:cursor-default disabled:opacity-50"
					disabled={loadingMore}
					onclick={loadMore}>{loadingMore ? 'loading…' : `load ${PAGE} older runs`}</button
				>
			</div>
		{/if}
	</div>

	{#if selected.length > 0}
		<div
			class="flex flex-none items-center gap-[12px] border-t border-hair-strong bg-base-200 px-4 py-[8px] text-[12px]"
		>
			<span class="text-meta">
				{#if selected.length === 1}
					Pick a second run to compare against.
				{:else}
					Comparing two runs.
				{/if}
			</span>
			<span class="tnum flex min-w-0 gap-[8px] overflow-hidden">
				{#each selected as id, i (id)}
					{@const r = runs.find((x) => x.id === id)}
					<span
						class="max-w-[260px] overflow-hidden rounded-[2px] border border-hair px-[7px] py-[2px] text-[12px] text-ellipsis whitespace-nowrap"
						><b class="mr-[6px] font-semibold">{i === 0 ? 'A' : 'B'}</b>#{id} · {r
							? fmtCommand(r.command)
							: ''}</span
					>
				{/each}
			</span>
			<button
				type="button"
				class="ml-auto cursor-pointer text-[12px] tracking-[0.1em] text-meta uppercase hover:text-base-content"
				onclick={() => (selected = [])}>clear</button
			>
			<button
				type="button"
				class="inline-flex cursor-pointer items-center gap-1.5 rounded-[2px] bg-base-content px-[12px] py-[5px] text-[12px] font-medium text-base-100 disabled:cursor-not-allowed disabled:opacity-40"
				disabled={selected.length !== 2}
				onclick={openDiff}>Diff these two runs <ArrowRight {...INLINE} aria-hidden="true" /></button
			>
		</div>
	{/if}
{/if}
