<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import TerminalPane from '$lib/components/TerminalPane.svelte';
	import EntryRow from '$lib/components/EntryRow.svelte';
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

	let termRef = $state<ReturnType<typeof TerminalPane> | undefined>();

	$effect(() => {
		const pid = projectId;
		const rid = runId;
		if (!Number.isFinite(pid) || !Number.isFinite(rid)) return;
		let cancelled = false;
		loading = true;
		loadError = null;

		(async () => {
			try {
				const ref = projectRef(pid);
				const [r, es, raw] = await Promise.all([
					getRun(ref, rid),
					entries(ref, { run_id: rid, limit: 5000, ascending: true }),
					rawStream(ref, rid).catch(() => '')
				]);
				if (cancelled) return;
				run = r;
				list = es;
				termRef?.replace(raw);
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

	const filtered = $derived.by(() => {
		const q = search.trim().toLowerCase();
		return list.filter((e) => {
			if (levelFilter.size && !levelFilter.has(e.level)) return false;
			if (q && !e.text.toLowerCase().includes(q)) return false;
			return true;
		});
	});

	const newSignatures = $derived(list.filter((e) => e.is_new_signature).length);

	function countFor(l: Level): number {
		return list.filter((e) => e.level === l).length;
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
			list = await entries(projectRef(projectId), {
				run_id: run.id,
				limit: 5000,
				ascending: true
			});
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
		<a class="back" href="/project/{projectId}/runs">← all runs</a>
		<a class="back" href="/project/{projectId}/diff?b={runId}">diff against last good →</a>
		<button type="button" class="pill" onclick={() => (showMeta = !showMeta)}
			>{showMeta ? 'hide' : 'show'} context</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="pad">
		<ErrorNote what="Could not load run #{runId}." detail={loadError} />
	</div>
{:else if loading}
	<p class="pad meta">Loading stored output…</p>
{:else if run}
	{#if showMeta}
		<section class="meta-grid" aria-label="Run context">
			<div class="cell">
				<span class="label-xs">outcome</span>
				<span class="v"><ExitBadge {run} /></span>
			</div>
			<div class="cell">
				<span class="label-xs">started</span>
				<span class="v tnum" title={fmtFull(run.started_at)}>{fmtStamp(run.started_at)}</span>
			</div>
			<div class="cell">
				<span class="label-xs">duration</span>
				<span class="v tnum">{fmtDuration(runDuration(run))}</span>
			</div>
			<div class="cell">
				<span class="label-xs">commit</span>
				<span class="v tnum"
					>{shortSha(run.git_sha)}
					<span class="meta">{run.git_branch ?? '—'}</span>
					{#if run.git_dirty_hash}<span class="dirty">dirty {shortHash(run.git_dirty_hash)}</span
						>{/if}</span
				>
			</div>
			<div class="cell">
				<span class="label-xs">entries</span>
				<span class="v tnum"
					>{run.counters.n_entries}
					<span class="meta">/ {run.counters.n_error} err / {run.counters.n_warn} warn</span></span
				>
			</div>
			<div class="cell">
				<span class="label-xs">new signatures</span>
				<span class="v tnum" class:flare={newSignatures > 0}>{newSignatures}</span>
			</div>
			<div class="cell">
				<span class="label-xs">delimiter mode</span>
				<span class="v" title="How the daemon split this stream into entries"
					>{run.delimiter_mode}</span
				>
			</div>
			<div class="cell">
				<span class="label-xs">terminal width</span>
				<span class="v tnum">{run.term_cols} cols</span>
			</div>
			<div class="cell wide">
				<span class="label-xs">working directory</span>
				<button
					type="button"
					class="v pathbtn"
					onclick={() => void revealPath(run!.cwd)}
					title="Reveal {run.cwd} in Finder">{tildePath(run.cwd)}</button
				>
			</div>
			<div class="cell wide">
				<span class="label-xs">lockfiles</span>
				<span class="v tnum small">
					{#each Object.entries(run.lockfile_hashes) as [f, h] (f)}
						<span class="kv">{f} <span class="meta">{shortHash(h)}</span></span>
					{:else}
						<span class="meta">none recorded</span>
					{/each}
				</span>
			</div>
			<div class="cell full">
				<span class="label-xs">environment variable names ({run.env_keys.length}) · values are never stored</span>
				<span class="v small envkeys">
					{#each run.env_keys as k (k)}<code>{k}</code>{/each}
				</span>
			</div>
		</section>
	{/if}

	{#if run.chunks_evicted}
		<p class="evicted">
			Part of this run's raw output was evicted by the retention cap. The structured entries below
			are complete; the terminal replay is not. Raise the cap in Settings to keep more.
		</p>
	{/if}

	<section class="termwrap" aria-label="Terminal replay">
		<TerminalPane bind:this={termRef} cols={run.term_cols} />
	</section>

	<div class="controls">
		<div class="chips">
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
		<div class="right">
			<input
				type="search"
				placeholder="Filter entries…"
				bind:value={search}
				aria-label="Filter entries by text"
			/>
			<button
				type="button"
				class="pill"
				disabled={busy !== null}
				onclick={doReparse}
				title="Re-run the fingerprinter over this run's stored bytes. Nothing is re-executed."
				>{busy === 'reparse' ? 'reparsing…' : 're-parse'}</button
			>
			<button
				type="button"
				class="pill danger"
				disabled={busy !== null}
				onclick={doDelete}
				title="Permanently delete this run and its stored output"
				>{busy === 'delete' ? 'deleting…' : 'delete run'}</button
			>
		</div>
	</div>

	<div class="entries pane">
		{#each filtered as e (e.id)}
			<EntryRow
				entry={e}
				expanded={expanded.has(e.id)}
				highlight={search.trim()}
				onToggle={toggleEntry}
			/>
		{:else}
			<p class="none meta">No entries match this filter.</p>
		{/each}
	</div>
{/if}

<style>
	.pad {
		padding: 16px;
	}
	.back {
		font-size: 11px;
		color: var(--rl-meta);
		text-decoration: none;
	}
	.back:hover {
		color: var(--color-base-content);
	}
	.pill {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 4px 8px;
		cursor: pointer;
	}
	.pill:hover:not(:disabled) {
		color: var(--color-base-content);
		border-color: var(--rl-hair-strong);
	}
	.pill:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.pill.danger:hover:not(:disabled) {
		color: var(--color-flare);
		border-color: var(--color-flare);
	}

	.meta-grid {
		flex: none;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
		gap: 0;
		border-bottom: 1px solid var(--rl-hair);
		background: var(--color-base-200);
	}
	.cell {
		padding: 8px 14px;
		border-right: 1px solid var(--rl-hair);
		border-bottom: 1px solid var(--rl-hair);
		min-width: 0;
	}
	.cell.wide {
		grid-column: span 2;
	}
	.cell.full {
		grid-column: 1 / -1;
		border-right: 0;
		border-bottom: 0;
	}
	.v {
		display: block;
		margin-top: 4px;
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.v.small {
		font-size: 11px;
		white-space: normal;
	}
	.v.flare {
		color: var(--color-flare);
		font-weight: 600;
	}
	.dirty {
		color: var(--color-warning);
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.16em;
		margin-left: 6px;
	}
	.kv {
		display: inline-block;
		margin-right: 12px;
	}
	.pathbtn {
		background: none;
		border: 0;
		padding: 0;
		font: inherit;
		font-size: 12px;
		color: var(--color-base-content);
		cursor: pointer;
		text-align: left;
		border-bottom: 1px dotted var(--rl-hair-strong);
		max-width: 100%;
	}
	.envkeys {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 6px;
		margin-top: 5px;
	}
	.envkeys code {
		font-size: 10.5px;
		color: var(--rl-meta);
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		padding: 1px 5px;
	}

	.evicted {
		flex: none;
		margin: 0;
		padding: 7px 16px;
		font-size: 11.5px;
		background: color-mix(in oklch, var(--color-warning) 10%, transparent);
		border-bottom: 1px solid var(--rl-hair);
	}

	.termwrap {
		flex: none;
		height: 40%;
		min-height: 160px;
		overflow: hidden;
		background: var(--rl-term-bg);
	}

	.controls {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 7px 12px;
		border-top: 1px solid var(--rl-hair);
		border-bottom: 1px solid var(--rl-hair);
		flex-wrap: wrap;
	}
	.chips {
		display: flex;
		gap: 5px;
		flex-wrap: wrap;
	}
	.right {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	input[type='search'] {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 11.5px;
		padding: 3px 8px;
		width: 200px;
		outline: none;
	}
	input[type='search']:focus {
		border-color: var(--rl-hair-strong);
	}
	.entries {
		flex: 1;
		min-height: 0;
		padding: 4px 0 24px;
	}
	.none {
		padding: 14px 16px;
		font-size: 12px;
	}
</style>
