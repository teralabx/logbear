<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import ErrorCount from '$lib/components/ErrorCount.svelte';
	import { apiError, entries, listRuns, projectRef } from '$lib/api';
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
				const rs = await listRuns(ref, 100, 0);
				if (cancelled) return;
				runs = rs;

				// One pass over the project's error entries gives every row its
				// total and its new-signature count without a call per run.
				const errs = await entries(ref, { errors_only: true, limit: 6000, ascending: true });
				if (cancelled) return;
				const map: Record<number, { total: number; neu: number }> = {};
				for (const e of errs) {
					const bucket = (map[e.run_id] ??= { total: 0, neu: 0 });
					bucket.total++;
					if (e.is_new_signature) bucket.neu++;
				}
				errorsByRun = map;
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
	sub={project ? `${project.name} · ${runs.length} stored` : ''}
>
	{#snippet actions()}
		<input
			class="filter"
			type="search"
			placeholder="Filter by command, branch, SHA…"
			bind:value={filter}
			aria-label="Filter runs"
		/>
		<button
			type="button"
			class="pill"
			class:on={onlyFailed}
			aria-pressed={onlyFailed}
			onclick={() => (onlyFailed = !onlyFailed)}>failed only</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="pad"><ErrorNote what="Could not read the run history." detail={loadError} /></div>
{:else if loading}
	<p class="pad meta">Reading run history…</p>
{:else if runs.length === 0}
	<div class="pad">
		<EmptyState
			title="No runs stored for this project"
			body="Every command you run through runlog lands here with its output, its exit code, and the git state that produced it."
			command="runlog npm test"
		/>
	</div>
{:else}
	{#if withNew > 0}
		<p class="banner">
			<span class="dot"></span>
			{withNew}
			{withNew === 1 ? 'run' : 'runs'} below produced an error signature this project had never seen before.
		</p>
	{/if}

	<div class="pane tablewrap">
		<table>
			<thead>
				<tr>
					<th class="c-sel" scope="col"><span class="sr">Compare</span></th>
					<th class="c-when" scope="col">Started</th>
					<th class="c-cmd" scope="col">Command</th>
					<th class="c-git" scope="col">Commit</th>
					<th class="c-dur" scope="col">Duration</th>
					<th class="c-exit" scope="col">Exit</th>
					<th class="c-err" scope="col">Errors</th>
					<th class="c-warn" scope="col">Warn</th>
				</tr>
			</thead>
			<tbody>
				{#each shown as r (r.id)}
					{@const err = errorsByRun[r.id] ?? { total: r.counters.n_error, neu: 0 }}
					<tr
						class:selected={selected.includes(r.id)}
						class:failed={r.exit_code !== null && r.exit_code !== 0}
						class:live={r.ended_at === null}
						class:has-new={err.neu > 0}
						onclick={() => open(r)}
						onkeydown={(e) => {
							if (e.key === 'Enter') open(r);
						}}
						tabindex="0"
					>
						<td class="c-sel">
							<button
								type="button"
								class="sel"
								class:on={selected.includes(r.id)}
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
						<td class="c-when tnum meta" title={fmtStamp(r.started_at)}>{fmtStamp(r.started_at)}</td
						>
						<td class="c-cmd"><code>{fmtCommand(r.command)}</code></td>
						<td class="c-git tnum">
							<span class="sha">{shortSha(r.git_sha)}</span>
							<span class="branch meta">{r.git_branch ?? '—'}</span>
							{#if r.git_dirty_hash}<span class="dirty" title="Working tree was dirty at run time"
									>dirty</span
								>{/if}
						</td>
						<td class="c-dur tnum meta">
							{r.ended_at === null
								? fmtDuration(runElapsed(r, now))
								: fmtDuration(runDuration(r))}
						</td>
						<td class="c-exit"><ExitBadge run={r} showLabel={false} /></td>
						<td class="c-err"><ErrorCount total={err.total} neu={err.neu} size="sm" /></td>
						<td class="c-warn tnum meta">{r.counters.n_warn || '—'}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if shown.length === 0}
			<p class="none meta">No runs match this filter.</p>
		{/if}
	</div>

	{#if selected.length > 0}
		<div class="compare">
			<span class="ctext">
				{#if selected.length === 1}
					Pick a second run to compare against.
				{:else}
					Comparing two runs.
				{/if}
			</span>
			<span class="picked tnum">
				{#each selected as id, i (id)}
					{@const r = runs.find((x) => x.id === id)}
					<span class="chip"
						><b>{i === 0 ? 'A' : 'B'}</b>#{id} · {r ? fmtCommand(r.command) : ''}</span
					>
				{/each}
			</span>
			<button type="button" class="ghost" onclick={() => (selected = [])}>clear</button>
			<button type="button" class="go" disabled={selected.length !== 2} onclick={openDiff}
				>Diff these two runs →</button
			>
		</div>
	{/if}
{/if}

<style>
	.pad {
		padding: 16px;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
	}
	.filter {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 11.5px;
		padding: 3px 8px;
		width: 230px;
		outline: none;
	}
	.filter:focus {
		border-color: var(--rl-hair-strong);
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
	.pill.on {
		background: var(--color-base-content);
		color: var(--color-base-100);
		border-color: var(--color-base-content);
	}

	.banner {
		flex: none;
		margin: 0;
		padding: 7px 16px;
		font-size: 12px;
		border-bottom: 1px solid var(--rl-hair);
		background: color-mix(in oklch, var(--color-flare) 8%, transparent);
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.banner .dot {
		width: 6px;
		height: 6px;
		background: var(--color-flare);
		border-radius: 1px;
		flex: none;
	}

	.tablewrap {
		flex: 1;
		min-height: 0;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		font-size: 12px;
		min-width: 940px;
	}
	thead th {
		position: sticky;
		top: 0;
		z-index: 2;
		background: var(--color-base-100);
		text-align: left;
		font-size: 10px;
		font-weight: 500;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--rl-meta);
		padding: 7px 10px;
		border-bottom: 1px solid var(--rl-hair-strong);
		white-space: nowrap;
	}
	tbody tr {
		border-bottom: 1px solid var(--rl-hair);
		cursor: pointer;
		border-left: 2px solid transparent;
	}
	tbody tr:hover {
		background: color-mix(in oklch, var(--color-base-content) 5%, transparent);
	}
	tbody tr.failed {
		border-left-color: color-mix(in oklch, var(--color-flare) 55%, transparent);
	}
	tbody tr.has-new {
		border-left-color: var(--color-flare);
	}
	tbody tr.live {
		border-left-color: var(--color-success);
	}
	tbody tr.selected {
		background: color-mix(in oklch, var(--color-base-content) 9%, transparent);
	}
	td {
		padding: 5px 10px;
		vertical-align: middle;
		white-space: nowrap;
	}
	.c-sel {
		width: 30px;
		padding-left: 8px;
		padding-right: 0;
	}
	.sel {
		width: 17px;
		height: 17px;
		border: 1px solid var(--rl-hair-strong);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-100);
		font: inherit;
		font-size: 10px;
		font-weight: 600;
		line-height: 1;
		cursor: pointer;
		display: grid;
		place-items: center;
	}
	.sel.on {
		background: var(--color-base-content);
		border-color: var(--color-base-content);
	}
	.c-when {
		width: 108px;
	}
	.c-cmd {
		max-width: 320px;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.c-cmd code {
		font-size: 12px;
	}
	.c-git {
		width: 230px;
	}
	.sha {
		color: var(--color-base-content);
	}
	.branch {
		margin-left: 7px;
		font-size: 11px;
	}
	.dirty {
		margin-left: 6px;
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--color-warning);
	}
	.c-dur {
		width: 82px;
		text-align: right;
	}
	.c-exit {
		width: 62px;
	}
	.c-err {
		width: 148px;
	}
	.c-warn {
		width: 56px;
		text-align: right;
	}
	.none {
		padding: 16px;
		font-size: 12px;
	}

	.compare {
		flex: none;
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 8px 16px;
		border-top: 1px solid var(--rl-hair-strong);
		background: var(--color-base-200);
		font-size: 12px;
	}
	.ctext {
		color: var(--rl-meta);
	}
	.picked {
		display: flex;
		gap: 8px;
		min-width: 0;
		overflow: hidden;
	}
	.picked .chip {
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		padding: 2px 7px;
		font-size: 11px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 260px;
	}
	.picked b {
		margin-right: 6px;
		font-weight: 600;
	}
	.ghost {
		margin-left: auto;
		border: 0;
		background: none;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		cursor: pointer;
	}
	.ghost:hover {
		color: var(--color-base-content);
	}
	.go {
		border: 0;
		border-radius: 2px;
		background: var(--color-base-content);
		color: var(--color-base-100);
		font: inherit;
		font-size: 11.5px;
		font-weight: 500;
		padding: 5px 12px;
		cursor: pointer;
	}
	.go:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
</style>
