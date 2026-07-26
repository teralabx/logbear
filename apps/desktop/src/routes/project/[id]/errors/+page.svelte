<script lang="ts">
	import { page } from '$app/state';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import LevelChip from '$lib/components/LevelChip.svelte';
	import Sparkline from '$lib/components/Sparkline.svelte';
	import { apiError, entries, fingerprintHistory, fingerprints, projectRef } from '$lib/api';
	import { app } from '$lib/stores/app.svelte';
	import { fmtAgo, fmtStamp, pluralise } from '$lib/format';
	import type { EntryView, Fingerprint, FingerprintHistoryPoint } from '$lib/api/types';

	/**
	 * Error signatures, not error lines.
	 *
	 * A signature that fired 40 times in one run is one problem; a signature
	 * that fired once in each of the last 12 runs is a different, worse problem.
	 * The sparkline is what separates them, so it is a first-class column rather
	 * than a decoration.
	 */

	const projectId = $derived(Number(page.params.id));
	const project = $derived(app.project(projectId));

	let fps = $state<Fingerprint[]>([]);
	let history = $state<Record<number, FingerprintHistoryPoint[]>>({});
	let instances = $state<Record<number, EntryView[]>>({});
	let loading = $state(true);
	let loadError = $state<string | null>(null);
	let sort = $state<'frequency' | 'recency' | 'spread'>('frequency');
	let errorsOnly = $state(true);
	let open = $state<number | null>(null);

	$effect(() => {
		const id = projectId;
		if (!Number.isFinite(id)) return;
		let cancelled = false;
		loading = true;
		loadError = null;
		open = null;

		(async () => {
			try {
				const ref = projectRef(id);
				const list = await fingerprints(ref, 200, false);
				if (cancelled) return;
				fps = list;

				const all = await entries(ref, { limit: 8000, ascending: false });
				if (cancelled) return;
				const byFp: Record<number, EntryView[]> = {};
				for (const e of all) {
					if (e.fingerprint_id === null) continue;
					(byFp[e.fingerprint_id] ??= []).push(e);
				}
				instances = byFp;

				const hist = await Promise.all(
					list.map(async (f) => [f.id, await fingerprintHistory(ref, f.id, 30)] as const)
				);
				if (cancelled) return;
				history = Object.fromEntries(hist);
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
		let out = fps.filter((f) => (errorsOnly ? f.level === 'error' : true));
		out = [...out].sort((a, b) => {
			if (sort === 'recency') return b.last_seen - a.last_seen;
			if (sort === 'spread') return b.run_count - a.run_count || b.total_count - a.total_count;
			return b.total_count - a.total_count || b.last_seen - a.last_seen;
		});
		return out;
	});

	/** Occurrences in the most recent run that carried this signature. */
	function latestBurst(id: number): number {
		const h = history[id];
		if (!h) return 0;
		for (let i = h.length - 1; i >= 0; i--) if (h[i].count > 0) return h[i].count;
		return 0;
	}

	/** Consecutive most-recent runs with zero occurrences. */
	function quietRuns(id: number): number {
		const h = history[id];
		if (!h) return 0;
		let n = 0;
		for (let i = h.length - 1; i >= 0; i--) {
			if (h[i].count > 0) break;
			n++;
		}
		return n;
	}
</script>

<PageHeader
	title="Errors"
	sub={project ? `${project.name} · ${shown.length} signatures` : ''}
>
	{#snippet actions()}
		<div class="seg" role="group" aria-label="Sort signatures">
			<button type="button" class:on={sort === 'frequency'} onclick={() => (sort = 'frequency')}
				>frequency</button
			>
			<button type="button" class:on={sort === 'recency'} onclick={() => (sort = 'recency')}
				>recency</button
			>
			<button
				type="button"
				class:on={sort === 'spread'}
				onclick={() => (sort = 'spread')}
				title="Signatures that appear across the most distinct runs">spread</button
			>
		</div>
		<button
			type="button"
			class="pill"
			class:on={errorsOnly}
			aria-pressed={errorsOnly}
			onclick={() => (errorsOnly = !errorsOnly)}>errors only</button
		>
	{/snippet}
</PageHeader>

{#if loadError}
	<div class="pad"><ErrorNote what="Could not read error signatures." detail={loadError} /></div>
{:else if loading}
	<p class="pad meta">Ranking signatures…</p>
{:else if shown.length === 0}
	<div class="pad">
		<EmptyState
			title={errorsOnly ? 'No error signatures recorded' : 'No signatures recorded'}
			body="runlog fingerprints failures as it captures them. Once something fails, its signature appears here with a count and a first-seen date."
			command="runlog mix test"
		/>
	</div>
{:else}
	<div class="pane list">
		<div class="colhead">
			<span class="c-level label-xs">level</span>
			<span class="c-tpl label-xs">signature</span>
			<span class="c-spark label-xs">last 30 runs</span>
			<span class="c-count label-xs">total</span>
			<span class="c-runs label-xs">runs</span>
			<span class="c-seen label-xs">last seen</span>
		</div>

		{#each shown as f (f.id)}
			{@const h = history[f.id] ?? []}
			{@const quiet = quietRuns(f.id)}
			<div class="fp" data-level={f.level} class:open={open === f.id}>
				<button
					type="button"
					class="row"
					aria-expanded={open === f.id}
					onclick={() => (open = open === f.id ? null : f.id)}
				>
					<span class="c-level"><LevelChip level={f.level} /></span>
					<span class="c-tpl">
						<code>{f.template}</code>
						<span class="firstseen meta">first seen {fmtStamp(f.first_seen)}</span>
					</span>
					<span class="c-spark"><Sparkline points={h} level={f.level} /></span>
					<span class="c-count tnum">
						<b>{f.total_count}</b>
						{#if latestBurst(f.id) > 1}<span class="meta">·{latestBurst(f.id)}× last</span>{/if}
					</span>
					<span class="c-runs tnum">{f.run_count}</span>
					<span class="c-seen tnum meta" title={fmtStamp(f.last_seen)}>
						{fmtAgo(f.last_seen)}
						{#if quiet >= 3}<span class="quiet" title="Not seen in the last {quiet} runs"
								>quiet {quiet}</span
							>{/if}
					</span>
				</button>

				{#if open === f.id}
					{@const rows = (instances[f.id] ?? []).slice(0, 40)}
					<div class="drill">
						<div class="drillhead">
							<span class="label-xs"
								>{pluralise(instances[f.id]?.length ?? 0, 'stored instance')}, newest first</span
							>
							<span class="hash meta tnum">fingerprint {f.hash.slice(0, 12)}</span>
						</div>
						{#if rows.length === 0}
							<p class="none meta">
								The instances for this signature have been evicted by the retention cap. The
								signature's counts survive eviction; the text does not.
							</p>
						{:else}
							<ul class="inst">
								{#each rows as e (e.id)}
									<li>
										<a class="ilink tnum" href="/project/{projectId}/runs/{e.run_id}">
											<span class="itime meta">{fmtStamp(e.captured_at)}</span>
											<span class="irun">run #{e.run_id}</span>
											{#if e.is_new_signature}<span class="inew">first</span>{/if}
											<span class="itext">{e.text.split('\n')[0]}</span>
										</a>
									</li>
								{/each}
							</ul>
							{#if (instances[f.id]?.length ?? 0) > rows.length}
								<p class="more meta">
									Showing the {rows.length} newest of {instances[f.id].length} stored instances.
								</p>
							{/if}
						{/if}
					</div>
				{/if}
			</div>
		{/each}
	</div>
{/if}

<style>
	.pad {
		padding: 16px;
	}
	.seg {
		display: flex;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		overflow: hidden;
	}
	.seg button {
		border: 0;
		background: transparent;
		color: var(--rl-meta);
		font: inherit;
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 4px 9px;
		cursor: pointer;
	}
	.seg button + button {
		border-left: 1px solid var(--rl-hair);
	}
	.seg button.on {
		background: var(--color-base-content);
		color: var(--color-base-100);
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

	.list {
		flex: 1;
		min-height: 0;
	}
	.colhead,
	.row {
		display: grid;
		grid-template-columns: 76px minmax(280px, 1fr) 140px 92px 54px 128px;
		gap: 12px;
		align-items: center;
	}
	.colhead {
		position: sticky;
		top: 0;
		z-index: 2;
		background: var(--color-base-100);
		padding: 7px 16px;
		border-bottom: 1px solid var(--rl-hair-strong);
	}
	.fp {
		border-bottom: 1px solid var(--rl-hair);
		border-left: 2px solid transparent;
	}
	.fp[data-level='error'] {
		border-left-color: color-mix(in oklch, var(--rl-lvl-error) 60%, transparent);
	}
	.fp[data-level='warn'] {
		border-left-color: color-mix(in oklch, var(--rl-lvl-warn) 50%, transparent);
	}
	.fp.open {
		background: var(--color-base-200);
	}
	.row {
		width: 100%;
		text-align: left;
		background: none;
		border: 0;
		font: inherit;
		color: inherit;
		padding: 8px 16px;
		cursor: pointer;
	}
	.row:hover {
		background: color-mix(in oklch, var(--color-base-content) 4%, transparent);
	}
	.c-tpl {
		min-width: 0;
	}
	.c-tpl code {
		display: block;
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.firstseen {
		display: block;
		font-size: 10.5px;
		margin-top: 2px;
	}
	.c-count b {
		font-size: 13px;
		font-weight: 600;
	}
	.c-count .meta {
		font-size: 10.5px;
		margin-left: 4px;
	}
	.c-runs {
		font-size: 12px;
		color: var(--rl-meta);
		text-align: right;
	}
	.c-seen {
		font-size: 11.5px;
	}
	.quiet {
		display: inline-block;
		margin-left: 6px;
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--color-success);
	}

	.drill {
		padding: 4px 16px 14px 16px;
		border-top: 1px dashed var(--rl-hair-strong);
	}
	.drillhead {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		padding: 8px 0 6px;
	}
	.hash {
		font-size: 10.5px;
	}
	.inst {
		list-style: none;
		margin: 0;
		padding: 0;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		max-height: 300px;
		overflow: auto;
		overscroll-behavior: contain;
	}
	.inst li + li {
		border-top: 1px solid var(--rl-hair);
	}
	.ilink {
		display: grid;
		grid-template-columns: 108px 72px auto 1fr;
		gap: 10px;
		align-items: baseline;
		padding: 4px 10px;
		text-decoration: none;
		color: var(--color-base-content);
		font-size: 11.5px;
	}
	.ilink:hover {
		background: color-mix(in oklch, var(--color-base-content) 6%, transparent);
	}
	.itime {
		font-size: 11px;
	}
	.irun {
		color: var(--rl-meta);
	}
	.inew {
		font-size: 9px;
		letter-spacing: 0.14em;
		text-transform: uppercase;
		background: var(--color-flare);
		color: #180703;
		border-radius: 2px;
		padding: 0 4px;
		font-weight: 600;
	}
	.itext {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.none,
	.more {
		font-size: 11.5px;
		margin: 6px 0 0;
		max-width: 70ch;
		line-height: 1.6;
	}
</style>
