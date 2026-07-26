<script lang="ts">
	import PageHeader from '$lib/components/PageHeader.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorNote from '$lib/components/ErrorNote.svelte';
	import ExitBadge from '$lib/components/ExitBadge.svelte';
	import ErrorCount from '$lib/components/ErrorCount.svelte';
	import { app } from '$lib/stores/app.svelte';
	import {
		fmtAgo,
		fmtBytes,
		fmtCommand,
		fmtDuration,
		fmtStamp,
		runDuration,
		runElapsed,
		shortSha,
		tildePath
	} from '$lib/format';
	import type { ProjectStatus } from '$lib/api/types';

	// Elapsed time on a running command has to tick on its own — no event
	// arrives just because a second passed.
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});

	const ordered = $derived(
		app.projects
			.map((p) => ({ p, s: app.statuses[p.id] as ProjectStatus | undefined }))
			.sort((a, b) => {
				const ra = (a.s?.active_runs.length ?? 0) > 0 ? 1 : 0;
				const rb = (b.s?.active_runs.length ?? 0) > 0 ? 1 : 0;
				if (ra !== rb) return rb - ra;
				return b.p.last_seen_at - a.p.last_seen_at;
			})
	);

	const totalActive = $derived(app.activeRuns.length);
</script>

<PageHeader
	title="Projects"
	sub={app.projects.length
		? `${app.projects.length} tracked · ${totalActive} running · ${fmtBytes(app.daemon?.total_db_bytes ?? 0)} stored`
		: ''}
/>

<div class="pane body">
	{#if app.daemonError}
		<ErrorNote
			what="The runlog daemon is not answering, so this list may be stale."
			detail={app.daemonError}
			retry={() => void app.refresh()}
		/>
	{/if}

	{#if !app.loaded}
		<p class="meta loading">Reading the store…</p>
	{:else if app.projects.length === 0}
		<EmptyState
			title="Nothing recorded yet"
			body="runlog records a project the first time you run something through it — start your dev command with runlog and this list fills itself in."
			command="runlog npm run dev"
		>
			<p class="teach meta">
				Once it is capturing, install the PATH shims from Settings → Install and you can drop the
				prefix entirely.
			</p>
		</EmptyState>
	{:else}
		<div class="grid">
			{#each ordered as { p, s } (p.id)}
				{@const running = s?.active_runs ?? []}
				{@const last = s?.last_run ?? null}
				<article class="card" class:running={running.length > 0}>
					<a class="hit" href="/project/{p.id}/live" aria-label="Open {p.name}"></a>

					<header class="top">
						<div class="ident">
							<h3>{p.name}</h3>
							<span class="root meta" title={p.root}>{tildePath(p.root)}</span>
						</div>
						{#if running.length}
							<span class="state live">
								<span class="pip is-running"></span>
								running
							</span>
						{:else}
							<span class="state idle">idle</span>
						{/if}
					</header>

					{#if running.length}
						<div class="now">
							{#each running as r (r.id)}
								<div class="nowrow">
									<code class="cmd">{fmtCommand(r.command)}</code>
									<span class="tnum elapsed">{fmtDuration(runElapsed(r, now))}</span>
								</div>
								<div class="nowmeta meta tnum">
									{(app.counters[r.id] ?? r.counters).n_entries} entries ·
									{(app.counters[r.id] ?? r.counters).n_error} errors ·
									{r.git_branch ?? 'no branch'}@{shortSha(r.git_sha)}
								</div>
							{/each}
						</div>
					{:else if last}
						<div class="lastrun">
							<div class="nowrow">
								<code class="cmd dim">{fmtCommand(last.command)}</code>
								<ExitBadge run={last} showLabel={false} />
							</div>
							<div class="nowmeta meta tnum">
								{fmtAgo(last.started_at, now)} · {fmtDuration(runDuration(last))} ·
								{last.git_branch ?? 'no branch'}@{shortSha(last.git_sha)}{last.git_dirty_hash
									? ' +dirty'
									: ''}
							</div>
						</div>
					{:else}
						<div class="lastrun">
							<span class="meta">No completed runs stored.</span>
						</div>
					{/if}

					<footer class="foot">
						<span class="stat">
							<span class="label-xs">last hour</span>
							<ErrorCount
								total={s?.errors_last_hour ?? 0}
								neu={s?.unacked_errors ?? 0}
								label="error"
								size="sm"
							/>
						</span>
						<span class="stat">
							<span class="label-xs">stored</span>
							<span class="tnum val">{fmtBytes(s?.db_bytes ?? 0)}</span>
						</span>
						<span class="stat">
							<span class="label-xs">last seen</span>
							<span class="tnum val" title={fmtStamp(p.last_seen_at)}
								>{fmtAgo(p.last_seen_at, now)}</span
							>
						</span>
						<nav class="links">
							<a href="/project/{p.id}/runs">runs</a>
							<a href="/project/{p.id}/errors">errors</a>
							<a href="/project/{p.id}/diff">diff</a>
						</nav>
					</footer>
				</article>
			{/each}
		</div>
	{/if}
</div>

<style>
	.body {
		flex: 1;
		padding: 16px;
	}
	.loading {
		font-size: 12px;
	}
	.teach {
		font-size: 11.5px;
		line-height: 1.6;
		margin: 0;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
		gap: 12px;
	}
	.card {
		position: relative;
		border: 1px solid var(--rl-hair);
		border-left: 2px solid var(--rl-hair-strong);
		border-radius: 3px;
		background: var(--color-base-200);
		padding: 12px 14px 10px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		min-width: 0;
	}
	.card:hover {
		border-color: var(--rl-hair-strong);
	}
	/* A project running something reads as running from across the room: a lit
	   left edge, a tinted surface, and a breathing pip. */
	.card.running {
		border-left-color: var(--color-success);
		background: color-mix(in oklch, var(--color-success) 5%, var(--color-base-200));
	}
	.hit {
		position: absolute;
		inset: 0;
		z-index: 0;
	}
	.top,
	.now,
	.lastrun,
	.foot {
		position: relative;
		z-index: 1;
		pointer-events: none;
	}
	.foot :global(.links),
	.links a {
		pointer-events: auto;
	}
	.top {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 12px;
	}
	.ident {
		min-width: 0;
	}
	h3 {
		font-size: 14px;
		font-weight: 600;
		margin: 0;
	}
	.root {
		display: block;
		font-size: 11px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.state {
		flex: none;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.16em;
	}
	.state.live {
		color: var(--color-success);
	}
	.state.idle {
		color: var(--rl-meta);
	}
	.pip {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--color-success);
	}
	.nowrow {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
	}
	.cmd {
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.cmd.dim {
		color: var(--rl-meta);
	}
	.elapsed {
		font-size: 12px;
		color: var(--color-success);
		flex: none;
	}
	.nowmeta {
		margin-top: 3px;
		font-size: 11px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.foot {
		display: flex;
		align-items: flex-end;
		gap: 18px;
		padding-top: 9px;
		border-top: 1px solid var(--rl-hair);
	}
	.stat {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.val {
		font-size: 11.5px;
	}
	.links {
		margin-left: auto;
		display: flex;
		gap: 10px;
		pointer-events: auto;
	}
	.links a {
		font-size: 11px;
		color: var(--rl-meta);
		text-decoration: none;
		border-bottom: 1px solid transparent;
	}
	.links a:hover {
		color: var(--color-base-content);
		border-bottom-color: var(--rl-hair-strong);
	}
</style>
