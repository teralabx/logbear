<script lang="ts">
	import { page } from '$app/state';
	import Mark from './Mark.svelte';
	import { app } from '$lib/stores/app.svelte';
	import { theme } from '$lib/stores/theme.svelte';
	import { rail } from '$lib/stores/rail.svelte';
	import { fmtBytes, tildePath } from '$lib/format';

	/**
	 * The persistent left rail. Always shows: where you are, which projects are
	 * running something right now, and whether the daemon is actually connected
	 * — because every number on every other screen is meaningless if it is not.
	 *
	 * Collapses (⌘\) to a narrow strip that keeps all three: project dots, the
	 * section initials, and the daemon dot.
	 */

	const currentId = $derived(
		page.params.id ? Number(page.params.id) : (app.projects[0]?.id ?? null)
	);

	const sections = $derived(
		currentId === null
			? []
			: [
					{ href: `/project/${currentId}/live`, label: 'Live', short: 'L', key: 'live' },
					{ href: `/project/${currentId}/runs`, label: 'Runs', short: 'R', key: 'runs' },
					{ href: `/project/${currentId}/errors`, label: 'Errors', short: 'E', key: 'errors' },
					{ href: `/project/${currentId}/diff`, label: 'Diff', short: 'D', key: 'diff' }
				]
	);

	const active = $derived(page.url.pathname);

	function isRunning(projectId: number): boolean {
		return (app.statuses[projectId]?.active_runs.length ?? 0) > 0;
	}

	// Shared shape of every rail link; the on/hover colours differ per state.
	const linkBase =
		'flex items-center gap-2 border-l-2 py-1 text-[12.5px] no-underline';
	const linkOn =
		'border-l-base-content bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)] text-base-content';
	const linkOff =
		'border-l-transparent text-meta hover:bg-[color-mix(in_oklch,var(--color-base-content)_4%,transparent)] hover:text-base-content';
</script>

<nav
	class="rule-r flex h-full min-h-0 flex-none flex-col bg-base-200 {rail.collapsed
		? 'rail-compact w-[56px]'
		: 'w-[208px]'}"
	aria-label="Primary"
>
{#if rail.collapsed}
	<div class="brand-bar rule-b flex h-[46px] flex-none items-center justify-center" data-tauri-drag-region>
		<a class="brand-mark [-webkit-app-region:no-drag]" href="/" title="LogBear — all projects">
			<Mark size={22} animate />
		</a>
	</div>

	<div class="pane flex flex-1 flex-col items-center gap-1 pt-2 pb-2">
		<button
			type="button"
			class="mb-1 grid h-6 w-8 cursor-pointer place-items-center rounded-[2px] text-[13px] text-meta hover:bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)] hover:text-base-content"
			onclick={() => rail.toggle()}
			title="Expand sidebar (⌘\)"
			aria-label="Expand sidebar"
			aria-expanded="false">»</button
		>
		{#each app.projects as p (p.id)}
			<a
				href="/project/{p.id}/live"
				class="relative grid h-7 w-9 place-items-center rounded-[2px] border-l-2 text-[11px] no-underline {currentId ===
					p.id && active.startsWith('/project/')
					? linkOn
					: linkOff}"
				title="{p.name} — {tildePath(p.root)}"
				aria-label={p.name}
			>
				{p.name.slice(0, 2)}
				<span
					class="absolute top-1 right-1 size-1.5 rounded-full {isRunning(p.id)
						? 'bg-success'
						: (app.statuses[p.id]?.errors_last_hour ?? 0) > 0
							? 'bg-lvl-error'
							: 'bg-transparent'}"
					aria-hidden="true"
				></span>
			</a>
		{/each}

		{#if sections.length}
			<div class="rule-t mt-2 w-8 pt-2"></div>
			{#each sections as s (s.key)}
				<a
					href={s.href}
					class="grid h-7 w-9 place-items-center border-l-2 text-[12px] no-underline {active.startsWith(
						s.href
					)
						? linkOn
						: linkOff}"
					title={s.label}
					aria-label={s.label}>{s.short}</a
				>
			{/each}
		{/if}
	</div>

	<div class="rule-t flex flex-none flex-col items-center gap-2 py-2.5">
		<a
			class="grid size-6 place-items-center"
			href="/settings"
			title={app.connected ? `daemon ${app.daemon?.version}` : (app.daemonError ?? 'Daemon not reachable')}
			aria-label={app.connected ? 'Daemon connected' : 'Daemon down'}
		>
			<span
				class="size-[7px] rounded-full {app.connected ? 'bg-success' : 'is-running bg-flare'}"
			></span>
		</a>
		<a
			class="text-[11px] no-underline hover:text-base-content {active.startsWith('/settings')
				? 'text-base-content'
				: 'text-meta'}"
			href="/settings"
			title="Settings">set</a
		>
	</div>
{:else}
	<!-- The whole strip is a drag handle: with an overlay title bar there is otherwise
	     nothing left to move the window by. The link itself opts back out.
	     `pl-[14px]` is the non-macOS inset; the <style> block below widens it to make
	     room for the traffic lights when the root carries data-platform="macos". -->
	<div class="brand-bar rule-b flex h-[46px] flex-none items-center pl-[14px]" data-tauri-drag-region>
		<!-- The arbitrary property opts this link out of the drag region so it stays clickable. -->
		<a
			class="flex items-center gap-[9px] text-base-content no-underline [-webkit-app-region:no-drag]"
			href="/"
			title="LogBear — all projects"
		>
			<Mark size={22} animate />
			<span class="text-[15px] font-semibold tracking-normal">LogBear</span>
		</a>
		<button
			type="button"
			class="mr-2 ml-auto grid h-6 w-7 cursor-pointer place-items-center rounded-[2px] text-[13px] text-meta [-webkit-app-region:no-drag] hover:bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)] hover:text-base-content"
			onclick={() => rail.toggle()}
			title="Collapse sidebar (⌘\)"
			aria-label="Collapse sidebar"
			aria-expanded="true">«</button
		>
	</div>

	<div class="pane flex-1 pt-3 pb-2">
		<div>
			<div class="flex items-center justify-between px-[14px] pb-[7px]">
				<span class="label-xs">Projects</span>
				<a
					class="text-[10px] tracking-[0.16em] no-underline uppercase hover:text-base-content {active ===
					'/'
						? 'text-base-content'
						: 'text-meta'}"
					href="/">all</a
				>
			</div>
			{#if app.projects.length === 0}
				<p class="meta m-0 px-[14px] py-0.5 text-[11px]">{app.loaded ? 'None yet' : 'Loading…'}</p>
			{:else}
				<ul class="m-0 list-none p-0">
					{#each app.projects as p (p.id)}
						<li>
							<a
								href="/project/{p.id}/live"
								class="{linkBase} pr-[14px] pl-3 {currentId === p.id && active.startsWith('/project/')
									? linkOn
									: linkOff}"
								title={tildePath(p.root)}
							>
								<span
									class="size-1.5 flex-none rounded-full {isRunning(p.id)
										? 'bg-success shadow-[0_0_0_3px_color-mix(in_oklch,var(--color-success)_22%,transparent)]'
										: 'bg-hair-strong'}"
									aria-hidden="true"
								></span>
								<span class="truncate">{p.name}</span>
								{#if (app.statuses[p.id]?.errors_last_hour ?? 0) > 0}
									<span
										class="tnum ml-auto text-[10px] text-lvl-error"
										title="Errors in the last hour">{app.statuses[p.id].errors_last_hour}</span
									>
								{/if}
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</div>

		{#if sections.length}
			<div class="rule-t mt-[18px] pt-[14px]">
				<div class="flex items-center justify-between px-[14px] pb-[7px]">
					<span class="label-xs">{app.project(currentId!)?.name ?? 'Project'}</span>
				</div>
				<ul class="m-0 list-none p-0">
					{#each sections as s (s.key)}
						<li>
							<a
								href={s.href}
								class="{linkBase} px-[14px] {active.startsWith(s.href) ? linkOn : linkOff}"
							>
								{s.label}
								{#if s.key === 'live' && isRunning(currentId!)}
									<span
										class="is-running ml-auto text-[9px] tracking-[0.16em] text-success uppercase"
										>live</span
									>
								{/if}
							</a>
						</li>
					{/each}
				</ul>
			</div>
		{/if}
	</div>

	<div class="rule-t flex-none px-[14px] pt-2 pb-2.5">
		<a
			class="group flex items-center gap-[7px] pt-0.5 pb-1.5 text-[11px] text-meta no-underline"
			href="/settings"
			title={app.connected
				? `logbear daemon ${app.daemon?.version} · pid ${app.daemon?.pid} · ${fmtBytes(app.daemon?.total_db_bytes ?? 0)} stored`
				: (app.daemonError ?? 'Daemon not reachable')}
		>
			<span
				class="size-[7px] flex-none rounded-full {app.connected
					? 'bg-success'
					: 'is-running bg-flare'}"
			></span>
			<span class="group-hover:text-base-content">
				{#if app.connected}
					daemon {app.daemon?.version}
				{:else}
					daemon down
				{/if}
			</span>
		</a>

		<div class="flex items-center justify-between gap-2">
			<a
				class="text-[12px] no-underline hover:text-base-content {active.startsWith('/settings')
					? 'text-base-content'
					: 'text-meta'}"
				href="/settings">Settings</a
			>
			<button
				type="button"
				class="cursor-pointer rounded-[2px] border border-hair bg-transparent px-1.5 py-0.5 text-[10px] tracking-[0.16em] text-meta uppercase hover:border-hair-strong hover:text-base-content"
				onclick={() => theme.toggle()}
				title={theme.isDark ? 'Switch to the bone theme' : 'Switch to the ink theme'}
				aria-label={theme.isDark ? 'Switch to light theme' : 'Switch to dark theme'}
			>
				{theme.isDark ? 'ink' : 'bone'}
			</button>
		</div>

		{#if app.mock}
			<p
				class="mt-2 mb-0 cursor-help text-[9px] tracking-[0.16em] text-warning uppercase"
				title="No Tauri backend detected — every screen is showing generated data"
			>
				sample data
			</p>
		{/if}
	</div>
{/if}
</nav>

<style>
	/* Room for the macOS window buttons, which are drawn over the brand bar. A
	   `:root[...]` ancestor selector is not expressible as a utility. */
	:global(:root[data-platform='macos']) .brand-bar {
		padding-left: 78px;
	}
	/* Compact on macOS: the strip widens just enough to sit under the window buttons,
	   which take the brand bar, so the mark gives way to them. */
	:global(:root[data-platform='macos']) .rail-compact {
		width: 78px;
	}
	:global(:root[data-platform='macos']) .rail-compact .brand-bar {
		padding-left: 0;
	}
	:global(:root[data-platform='macos']) .rail-compact .brand-mark {
		display: none;
	}
</style>
