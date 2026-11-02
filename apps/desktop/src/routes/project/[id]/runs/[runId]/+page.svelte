<script lang="ts">
	import { tick } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import TerminalPane from '$lib/components/TerminalPane.svelte';
	import EntryRow from '$lib/components/EntryRow.svelte';
	import { squash } from '$lib/squash';
	import LevelChip from '$lib/components/LevelChip.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import {
		apiError,
		deleteRun,
		entries,
		getRun,
		projectRef,
		rawStream,
		reparse,
		revealPath
	} from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtCommand,
		fmtDuration,
		fmtFull,
		fmtStamp,
		runDuration,
		shortHash,
		shortSha,
		tildePath
	} from '$lib/format';
	import { LEVELS, type EntryView, type Level, type Run } from '$lib/api/types';

	/** One stored run, replayed. Metadata first, then the output. */

	const projectId = $derived(Number(page.params.id));
	const runId = $derived(Number(page.params.runId));
	const project = $derived(app.project(projectId));

	let run = $state<Run | null>(null);
	let list = $state<EntryView[]>([]);
	let loading = $state(true);
	let loadError = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let search = $state('');
	let levelFilter = $state<Set<Level>>(new Set());
	let expanded = $state<Set<number>>(new Set());
	let showMeta = $state(true);
	let squashed = $state(false);

	let termRef = $state<ReturnType<typeof TerminalPane> | undefined>();
	let replayTruncated = $state(false);
	let hasMore = $state(false);
	let loadingMore = $state(false);

	/** Entries fetched per page. A long run has hundreds of thousands. */
	const PAGE = 1000;

	const firstPage = (pid: number, rid: number) =>
		entries(projectRef(pid), { run_id: rid, limit: PAGE, ascending: true });

	$effect(() => {
		const pid = projectId;
		const rid = runId;
		if (!Number.isFinite(pid) || !Number.isFinite(rid)) return;
		let cancelled = false;
		loading = true;
		loadError = null;
		replayTruncated = false;
		expanded = new Set();

		(async () => {
			try {
				const ref = projectRef(pid);
				const [r, es] = await Promise.all([getRun(ref, rid), firstPage(pid, rid)]);
				if (cancelled) return;
				run = r;
				list = es;
				hasMore = es.length === PAGE;
				// The terminal is only in the DOM once `loading` is false.
				loading = false;
				await tick();
				if (cancelled) return;
				termRef?.clear();
				replayTruncated = await rawStream(ref, rid, (bytes) => {
					if (!cancelled) termRef?.write(bytes);
				}).catch(() => false);
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
		const last = list.at(-1);
		if (!run || !last || loadingMore) return;
		loadingMore = true;
		try {
			const next = await entries(projectRef(projectId), {
				run_id: run.id,
				after_entry: last.id,
				limit: PAGE,
				ascending: true
			});
			list = list.concat(next);
			hasMore = next.length === PAGE;
		} catch (err) {
			loadError = apiError(err);
		} finally {
			loadingMore = false;
		}
	}

	const filtered = $derived.by(() => {
		const q = search.trim().toLowerCase();
		return list.filter((e) => {
			if (levelFilter.size && !levelFilter.has(e.level)) return false;
			if (q && !e.text.toLowerCase().includes(q)) return false;
			return true;
		});
	});

	/** Each distinct entry once, with its repeat count; or every entry, one per row. */
	const rows = $derived(
		squashed ? squash(filtered) : filtered.map((entry) => ({ entry, count: 1, lastAt: entry.captured_at }))
	);

	const newSignatures = $derived(list.filter((e) => e.is_new_signature).length);

	/** From the run's counters, so the chips are right before every page is loaded. */
	function countFor(l: Level): number {
		if (!run) return 0;
		const c = run.counters;
		return { error: c.n_error, warn: c.n_warn, info: c.n_info, debug: c.n_debug, trace: c.n_trace, unknown: c.n_unknown }[l];
	}

	function toggleLevel(l: Level) {
		const next = new Set(levelFilter);
		if (next.has(l)) next.delete(l);
		else next.add(l);
		levelFilter = next;
	}

	function toggleEntry(e: EntryView) {
		const next = new Set(expanded);
		if (next.has(e.id)) next.delete(e.id);
		else next.add(e.id);
		expanded = next;
	}

	async function doReparse() {
		if (!run) return;
		busy = 'reparse';
		try {
			await reparse(projectRef(projectId), run.id);
			const [r, es] = await Promise.all([
				getRun(projectRef(projectId), run.id),
				firstPage(projectId, run.id)
			]);
			run = r;
			list = es;
			hasMore = es.length === PAGE;
		} catch (err) {
			loadError = apiError(err);
		} finally {
			busy = null;
		}
	}

	async function doDelete() {
		if (!run) return;
		busy = 'delete';
		try {
			await deleteRun(projectRef(projectId), run.id);
			await goto(`/project/${projectId}/runs`);
		} catch (err) {
			loadError = apiError(err);
			busy = null;
		}
	}
</script>

<PageHeader
	title="Run #{runId}"
	sub={project ? `${project.name} · ${run ? fmtCommand(run.command) : ''}` : ''}
>
	{#snippet actions()}
		<a class="text-[11px] text-meta hover:text-base-content" href="/project/{projectId}/runs"
			>← all runs</a
		>
		<a
			class="text-[11px] text-meta hover:text-base-content"
			href="/project/{projectId}/diff?b={runId}">diff against last good →</a
		>
		<button
			type="button"
			class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[10px] tracking-[0.16em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content"
			onclick={() => (showMeta = !showMeta)}>{showMeta ? 'hide' : 'show'} context</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="p-4">
		<ErrorNote what="Could not load run #{runId}." detail={loadError} />
	</div>
{:else if loading}
	<p class="p-4 text-meta">Loading stored output…</p>
{:else if run}
	{#if showMeta}
		<section
			class="rule-b grid flex-none grid-cols-[repeat(auto-fill,minmax(160px,1fr))] gap-0 bg-base-200"
			aria-label="Run context"
		>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">outcome</span>
				<span class="mt-1 block truncate text-[12px]"><ExitBadge {run} /></span>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">started</span>
				<span class="tnum mt-1 block truncate text-[12px]" title={fmtFull(run.started_at)}
					>{fmtStamp(run.started_at)}</span
				>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">duration</span>
				<span class="tnum mt-1 block truncate text-[12px]">{fmtDuration(runDuration(run))}</span>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">commit</span>
				<span class="tnum mt-1 block truncate text-[12px]"
					>{shortSha(run.git_sha)}
					<span class="text-meta">{run.git_branch ?? '—'}</span>
					{#if run.git_dirty_hash}<span
							class="ml-[6px] text-[10px] tracking-[0.16em] text-warning uppercase"
							title="Hash of the uncommitted changes present when this run started"
							>uncommitted {shortHash(run.git_dirty_hash)}</span
						>{/if}</span
				>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">entries</span>
				<span class="tnum mt-1 block truncate text-[12px]"
					>{run.counters.n_entries}
					<span class="text-meta"
						>/ {run.counters.n_error} err / {run.counters.n_warn} warn</span
					></span
				>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">new signatures</span>
				<span
					class="tnum mt-1 block truncate text-[12px] {newSignatures > 0
						? 'font-semibold text-flare'
						: ''}">{newSignatures}</span
				>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">delimiter mode</span>
				<span
					class="mt-1 block truncate text-[12px]"
					title="How the daemon split this stream into entries">{run.delimiter_mode}</span
				>
			</div>
			<div class="rule-r rule-b min-w-0 px-[14px] py-2">
				<span class="label-xs">terminal width</span>
				<span class="tnum mt-1 block truncate text-[12px]">{run.term_cols} cols</span>
			</div>
			<div class="rule-r rule-b col-span-2 min-w-0 px-[14px] py-2">
				<span class="label-xs">working directory</span>
				<button
					type="button"
					class="mt-1 block max-w-full cursor-pointer truncate border-b border-dotted border-hair-strong text-left text-[12px] text-base-content"
					onclick={() => void revealPath(run!.cwd)}
					title="Reveal {run.cwd} in Finder">{tildePath(run.cwd)}</button
				>
			</div>
			<div class="rule-r rule-b col-span-2 min-w-0 px-[14px] py-2">
				<span class="label-xs">lockfiles</span>
				<span class="tnum mt-1 block overflow-hidden text-[11px] text-ellipsis whitespace-normal">
					{#each Object.entries(run.lockfile_hashes) as [f, h] (f)}
						<span class="mr-3 inline-block">{f} <span class="text-meta">{shortHash(h)}</span></span>
					{:else}
						<span class="text-meta">none recorded</span>
					{/each}
				</span>
			</div>
			<div class="col-span-full min-w-0 px-[14px] py-2">
				<span class="label-xs"
					>environment variable names ({run.env_keys.length}) · values are never stored</span
				>
				<span class="mt-[5px] flex flex-wrap gap-x-[6px] gap-y-1 overflow-hidden text-[11px]">
					{#each run.env_keys as k (k)}<code
							class="rounded-[2px] border border-hair px-[5px] py-px text-[10.5px] text-meta"
							>{k}</code
						>{/each}
				</span>
			</div>
		</section>
	{/if}

	{#if run.chunks_evicted}
		<p
			class="rule-b flex-none bg-[color-mix(in_oklch,var(--color-warning)_10%,transparent)] px-4 py-[7px] text-[11.5px]"
		>
			Part of this run's raw output was evicted by the retention cap. The structured entries below
			are complete; the terminal replay is not. Raise the cap in Settings to keep more.
		</p>
	{/if}

	{#if replayTruncated}
		<p class="rule-b flex-none px-4 py-[5px] text-[11px] text-meta">
			The replay starts at the last 20,000 lines. Every entry is still listed below, and
			<code>logbear cat {run.id}</code> prints the whole run.
		</p>
	{/if}

	<section class="h-[40%] min-h-[160px] flex-none overflow-hidden bg-term-bg" aria-label="Terminal replay">
		<TerminalPane bind:this={termRef} cols={run.term_cols} />
	</section>

	<div class="rule-t rule-b flex flex-none flex-wrap items-center justify-between gap-3 px-3 py-[7px]">
		<div class="flex flex-wrap gap-[5px]">
			{#each LEVELS as l (l)}
				{@const n = countFor(l)}
				{#if n > 0}
					<LevelChip
						level={l}
						count={n}
						active={levelFilter.has(l)}
						onclick={() => toggleLevel(l)}
					/>
				{/if}
			{/each}
		</div>
		<div class="flex items-center gap-2">
			<button
				type="button"
				class="cursor-pointer rounded-[2px] border px-2 py-[4px] text-[10px] tracking-[0.16em] uppercase {squashed
					? 'border-hair-strong text-base-content'
					: 'border-hair text-meta hover:border-hair-strong hover:text-base-content'}"
				aria-pressed={squashed}
				onclick={() => (squashed = !squashed)}
				title="Show each distinct entry once, with how many times it repeated. Timestamps, ids and numbers are ignored when comparing."
				>squash{#if squashed}<span class="tnum ml-1.5 tracking-normal normal-case text-meta"
						>{rows.length} of {filtered.length}</span
					>{/if}</button
			>
			<input
				type="search"
				class="w-[200px] rounded-[2px] border border-hair bg-transparent px-2 py-[3px] text-[11.5px] text-base-content outline-none focus:border-hair-strong"
				placeholder="Filter entries…"
				bind:value={search}
				aria-label="Filter entries by text"
			/>
			<button
				type="button"
				class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[10px] tracking-[0.16em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content disabled:cursor-default disabled:opacity-50"
				disabled={busy !== null}
				onclick={doReparse}
				title="Re-run the fingerprinter over this run's stored bytes. Nothing is re-executed."
				>{busy === 'reparse' ? 'reparsing…' : 're-parse'}</button
			>
			<button
				type="button"
				class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[10px] tracking-[0.16em] text-meta uppercase enabled:hover:border-flare enabled:hover:text-flare disabled:cursor-default disabled:opacity-50"
				disabled={busy !== null}
				onclick={doDelete}
				title="Permanently delete this run and its stored output"
				>{busy === 'delete' ? 'deleting…' : 'delete run'}</button
			>
		</div>
	</div>

	<div class="pane flex-1 pt-1 pb-6">
		{#each rows as { entry: e, count, lastAt } (e.id)}
			<EntryRow
				entry={e}
				expanded={expanded.has(e.id)}
				highlight={search.trim()}
				onToggle={toggleEntry}
				repeats={count}
				{lastAt}
			/>
		{:else}
			<p class="px-4 py-[14px] text-[12px] text-meta">No entries match this filter.</p>
		{/each}
		{#if hasMore}
			<div class="px-4 py-3">
				<button
					type="button"
					class="cursor-pointer rounded-[2px] border border-hair px-2 py-[4px] text-[10px] tracking-[0.16em] text-meta uppercase enabled:hover:border-hair-strong enabled:hover:text-base-content disabled:cursor-default disabled:opacity-50"
					disabled={loadingMore}
					onclick={loadMore}
					>{loadingMore
						? 'loading…'
						: `load next ${PAGE} · ${list.length} of ${run.counters.n_entries}`}</button
				>
				{#if search.trim() || levelFilter.size || squashed}
					<span class="ml-2 text-[11px] text-meta">Filters and squash apply to loaded entries only.</span>
				{/if}
			</div>
		{/if}
	</div>
{/if}
