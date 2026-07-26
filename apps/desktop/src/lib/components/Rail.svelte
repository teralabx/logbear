<script lang="ts">
	import { page } from '$app/state';
	import Mark from './Mark.svelte';
	import { app } from '$lib/stores/app.svelte';
	import { theme } from '$lib/stores/theme.svelte';
	import { fmtBytes, tildePath } from '$lib/format';

	/**
	 * The persistent left rail. Always shows: where you are, which projects are
	 * running something right now, and whether the daemon is actually connected
	 * — because every number on every other screen is meaningless if it is not.
	 */

	const currentId = $derived(
		page.params.id ? Number(page.params.id) : (app.projects[0]?.id ?? null)
	);

	const sections = $derived(
		currentId === null
			? []
			: [
					{ href: `/project/${currentId}/live`, label: 'Live', key: 'live' },
					{ href: `/project/${currentId}/runs`, label: 'Runs', key: 'runs' },
					{ href: `/project/${currentId}/errors`, label: 'Errors', key: 'errors' },
					{ href: `/project/${currentId}/diff`, label: 'Diff', key: 'diff' }
				]
	);

	const active = $derived(page.url.pathname);

	function isRunning(projectId: number): boolean {
		return (app.statuses[projectId]?.active_runs.length ?? 0) > 0;
	}
</script>

<nav class="rail" aria-label="Primary">
	<a class="brand" href="/" title="runlog — all projects">
		<Mark size={22} />
		<span class="wordmark">runlog</span>
	</a>

	<div class="scroll pane">
		<div class="group">
			<div class="group-head">
				<span class="label-xs">Projects</span>
				<a class="all" href="/" class:on={active === '/'}>all</a>
			</div>
			{#if app.projects.length === 0}
				<p class="none meta">{app.loaded ? 'None yet' : 'Loading…'}</p>
			{:else}
				<ul>
					{#each app.projects as p (p.id)}
						<li>
							<a
								href="/project/{p.id}/live"
								class="proj"
								class:on={currentId === p.id && active.startsWith('/project/')}
								title={tildePath(p.root)}
							>
								<span class="run-dot" class:live={isRunning(p.id)} aria-hidden="true"></span>
								<span class="pname">{p.name}</span>
								{#if (app.statuses[p.id]?.errors_last_hour ?? 0) > 0}
									<span class="err tnum" title="Errors in the last hour"
										>{app.statuses[p.id].errors_last_hour}</span
									>
								{/if}
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</div>

		{#if sections.length}
			<div class="group">
				<div class="group-head">
					<span class="label-xs">{app.project(currentId!)?.name ?? 'Project'}</span>
				</div>
				<ul>
					{#each sections as s (s.key)}
						<li>
							<a href={s.href} class="nav" class:on={active.startsWith(s.href)}>
								{s.label}
								{#if s.key === 'live' && isRunning(currentId!)}
									<span class="live-tag is-running">live</span>
								{/if}
							</a>
						</li>
					{/each}
				</ul>
			</div>
		{/if}
	</div>

	<div class="foot">
		<a
			class="daemon"
			href="/settings"
			title={app.connected
				? `runlog daemon ${app.daemon?.version} · pid ${app.daemon?.pid} · ${fmtBytes(app.daemon?.total_db_bytes ?? 0)} stored`
				: (app.daemonError ?? 'Daemon not reachable')}
		>
			<span class="dot" class:up={app.connected} class:down={!app.connected}></span>
			<span class="dtext">
				{#if app.connected}
					daemon {app.daemon?.version}
				{:else}
					daemon down
				{/if}
			</span>
		</a>

		<div class="foot-row">
			<a class="settings" href="/settings" class:on={active.startsWith('/settings')}>Settings</a>
			<button
				type="button"
				class="theme"
				onclick={() => theme.toggle()}
				title={theme.isDark ? 'Switch to the bone theme' : 'Switch to the ink theme'}
				aria-label={theme.isDark ? 'Switch to light theme' : 'Switch to dark theme'}
			>
				{theme.isDark ? 'ink' : 'bone'}
			</button>
		</div>

		{#if app.mock}
			<p class="mockwarn" title="No Tauri backend detected — every screen is showing generated data">
				sample data
			</p>
		{/if}
	</div>
</nav>

<style>
	.rail {
		width: 208px;
		flex: none;
		display: flex;
		flex-direction: column;
		background: var(--color-base-200);
		border-right: 1px solid var(--rl-hair);
		height: 100%;
		min-height: 0;
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 9px;
		height: 46px;
		padding: 0 14px;
		border-bottom: 1px solid var(--rl-hair);
		color: var(--color-base-content);
		text-decoration: none;
		flex: none;
	}
	.wordmark {
		font-size: 15px;
		font-weight: 600;
		letter-spacing: normal;
	}
	.scroll {
		flex: 1;
		padding: 12px 0 8px;
	}
	.group + .group {
		margin-top: 18px;
		padding-top: 14px;
		border-top: 1px solid var(--rl-hair);
	}
	.group-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 14px 7px;
	}
	.all {
		font-size: 10px;
		color: var(--rl-meta);
		text-decoration: none;
		letter-spacing: 0.16em;
		text-transform: uppercase;
	}
	.all.on,
	.all:hover {
		color: var(--color-base-content);
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.proj,
	.nav {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 4px 14px 4px 12px;
		font-size: 12.5px;
		color: var(--rl-meta);
		text-decoration: none;
		border-left: 2px solid transparent;
	}
	.proj:hover,
	.nav:hover {
		color: var(--color-base-content);
		background: color-mix(in oklch, var(--color-base-content) 4%, transparent);
	}
	.proj.on,
	.nav.on {
		color: var(--color-base-content);
		border-left-color: var(--color-base-content);
		background: color-mix(in oklch, var(--color-base-content) 6%, transparent);
	}
	.pname {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.run-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--rl-hair-strong);
		flex: none;
	}
	.run-dot.live {
		background: var(--color-success);
		box-shadow: 0 0 0 3px color-mix(in oklch, var(--color-success) 22%, transparent);
	}
	.err {
		margin-left: auto;
		font-size: 10px;
		color: var(--rl-lvl-error);
	}
	.nav {
		padding-left: 14px;
	}
	.live-tag {
		margin-left: auto;
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--color-success);
	}
	.none {
		padding: 2px 14px;
		font-size: 11px;
	}
	.foot {
		flex: none;
		border-top: 1px solid var(--rl-hair);
		padding: 8px 14px 10px;
	}
	.daemon {
		display: flex;
		align-items: center;
		gap: 7px;
		text-decoration: none;
		color: var(--rl-meta);
		font-size: 11px;
		padding: 2px 0 6px;
	}
	.daemon:hover .dtext {
		color: var(--color-base-content);
	}
	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		flex: none;
	}
	.dot.up {
		background: var(--color-success);
	}
	.dot.down {
		background: var(--color-flare);
		animation: rl-breathe 2.4s ease-in-out infinite;
	}
	.foot-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
	}
	.settings {
		font-size: 12px;
		color: var(--rl-meta);
		text-decoration: none;
	}
	.settings.on,
	.settings:hover {
		color: var(--color-base-content);
	}
	.theme {
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--rl-meta);
		background: transparent;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		padding: 2px 6px;
		cursor: pointer;
	}
	.theme:hover {
		color: var(--color-base-content);
		border-color: var(--rl-hair-strong);
	}
	.mockwarn {
		margin: 8px 0 0;
		font-size: 9px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--color-warning);
		cursor: help;
	}
</style>
