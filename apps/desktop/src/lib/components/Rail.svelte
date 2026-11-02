<script lang="ts">
	import { onMount } from 'svelte';
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
	 * Expanded and collapsed (⌘\) are one layout: every row keeps its icon and
	 * label, and only the widths and the label opacity change, so the transition
	 * is a slide rather than a swap. Collapsed, the labels are gone, so
	 * each row names itself in a tooltip on hover or focus.
	 */

	const currentId = $derived(
		page.params.id ? Number(page.params.id) : (app.projects[0]?.id ?? null)
	);

	type Icon = 'live' | 'runs' | 'errors' | 'diff';
	const sections = $derived(
		currentId === null
			? []
			: ([
					{ href: `/project/${currentId}/live`, label: 'Live', key: 'live' },
					{ href: `/project/${currentId}/runs`, label: 'Runs', key: 'runs' },
					{ href: `/project/${currentId}/errors`, label: 'Errors', key: 'errors' },
					{ href: `/project/${currentId}/diff`, label: 'Diff', key: 'diff' }
				] satisfies { href: string; label: string; key: Icon }[])
	);

	const active = $derived(page.url.pathname);

	function isRunning(projectId: number): boolean {
		return (app.statuses[projectId]?.active_runs.length ?? 0) > 0;
	}

	const daemonTip = $derived(
		app.connected
			? `logbear daemon ${app.daemon?.version} · pid ${app.daemon?.pid} · ${fmtBytes(app.daemon?.total_db_bytes ?? 0)} stored`
			: (app.daemonError ?? 'Daemon not reachable')
	);

	// Widths animate only after the stored collapsed state has been applied, so the
	// window does not open with the rail sliding shut.
	// Reading the width first commits the starting layout, so turning transitions on
	// afterwards cannot animate it.
	let nav: HTMLElement;
	let ready = $state(false);
	onMount(() => {
		const t = setTimeout(() => {
			void nav.offsetWidth;
			ready = true;
		});
		return () => clearTimeout(t);
	});

	// One tooltip for the whole rail, positioned beside the hovered row. It is
	// fixed-position so the scrolling middle section cannot clip it, and it only
	// shows while collapsed — expanded, the row's own label says the same thing.
	let tipState = $state<{ text: string; x: number; y: number } | null>(null);

	function tip(node: HTMLElement, text: string) {
		let current = text;
		const show = () => {
			if (!rail.collapsed) return;
			const r = node.getBoundingClientRect();
			tipState = { text: current, x: r.right + 8, y: r.top + r.height / 2 };
		};
		const hide = () => (tipState = null);
		node.addEventListener('pointerenter', show);
		node.addEventListener('pointerleave', hide);
		node.addEventListener('focus', show);
		node.addEventListener('blur', hide);
		node.addEventListener('click', hide);
		return {
			update(next: string) {
				current = next;
				if (tipState && node.matches(':hover, :focus')) show();
			},
			destroy() {
				node.removeEventListener('pointerenter', show);
				node.removeEventListener('pointerleave', hide);
				node.removeEventListener('focus', show);
				node.removeEventListener('blur', hide);
				node.removeEventListener('click', hide);
				// Teardown runs mid-render, where state cannot be written; clear just after.
				queueMicrotask(hide);
			}
		};
	}

	// A tooltip left over from the other state would point at the wrong place.
	$effect(() => {
		void rail.collapsed;
		tipState = null;
	});

	// Shared shape of every rail row; the on/hover colours differ per state.
	const linkBase = 'item border-l-2 text-[12.5px] no-underline';
	const linkOn =
		'border-l-base-content bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)] text-base-content';
	const linkOff =
		'border-l-transparent text-meta hover:bg-[color-mix(in_oklch,var(--color-base-content)_4%,transparent)] hover:text-base-content';
	const quiet =
		'text-meta hover:bg-[color-mix(in_oklch,var(--color-base-content)_4%,transparent)] hover:text-base-content';
</script>

{#snippet icon(name: Icon | 'settings' | 'theme')}
	<svg
		viewBox="0 0 16 16"
		width="15"
		height="15"
		fill="none"
		stroke="currentColor"
		stroke-width="1.4"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		{#if name === 'live'}
			<path d="M1.5 8h3l2-4.5 3 9 2-4.5h3" />
		{:else if name === 'runs'}
			<path d="M5.5 4h9M5.5 8h9M5.5 12h9" /><path d="M1.5 4h1M1.5 8h1M1.5 12h1" />
		{:else if name === 'errors'}
			<path d="M8 1.8 14.6 13.5H1.4Z" /><path d="M8 6.2v3.4M8 11.6v.1" />
		{:else if name === 'diff'}
			<path d="M4.5 1.5v13M11.5 1.5v13" /><path d="M2 5h5M9 11h5M11.5 8.5v5" />
		{:else if name === 'settings'}
			<circle cx="8" cy="8" r="2.2" />
			<path
				d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4"
			/>
		{:else if theme.isDark}
			<path d="M13.5 9.6A5.8 5.8 0 0 1 6.4 2.5a5.8 5.8 0 1 0 7.1 7.1Z" />
		{:else}
			<circle cx="8" cy="8" r="3" />
			<path
				d="M8 1v1.6M8 13.4V15M1 8h1.6M13.4 8H15M3 3l1.1 1.1M11.9 11.9 13 13M3 13l1.1-1.1M11.9 4.1 13 3"
			/>
		{/if}
	</svg>
{/snippet}

<nav
	bind:this={nav}
	class="rail rule-r flex h-full min-h-0 flex-none flex-col overflow-hidden bg-base-200"
	class:collapsed={rail.collapsed}
	class:ready
	aria-label="Primary"
>
	<!-- The title bar strip. With an overlay title bar this is where macOS draws its
	     window buttons, and the only surface left to drag the window by; its height
	     matches the page header so the two hairlines line up. -->
	<div class="titlebar rule-b flex h-[46px] flex-none items-center justify-end pr-2" data-tauri-drag-region>
		<button
			type="button"
			class="fade grid h-6 w-7 cursor-pointer place-items-center rounded-[2px] text-meta [-webkit-app-region:no-drag] hover:bg-[color-mix(in_oklch,var(--color-base-content)_6%,transparent)] hover:text-base-content"
			onclick={() => rail.toggle()}
			title="Collapse sidebar (⌘\)"
			aria-label="Collapse sidebar"
			aria-expanded="true"
			tabindex={rail.collapsed ? -1 : 0}
		>
			<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
				<rect x="1.5" y="2.5" width="13" height="11" rx="1.5" /><path d="M6 2.5v11" />
			</svg>
		</button>
	</div>

	<!-- Brand row, below the window buttons. Collapsed, the mark doubles as the
	     expand control and shows the sidebar glyph on hover to say so. -->
	{#if rail.collapsed}
		<button
			type="button"
			class="brand item group h-[44px] flex-none cursor-pointer border-l-2 border-l-transparent bg-transparent text-base-content"
			onclick={() => rail.toggle()}
			aria-label="Expand sidebar"
			aria-expanded="false"
			use:tip={'Expand sidebar  ⌘\\'}
		>
			<span class="ico">
				<span class="group-hover:hidden group-focus-visible:hidden"><Mark size={22} animate /></span>
				<svg class="hidden text-meta group-hover:block group-focus-visible:block" viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
					<rect x="1.5" y="2.5" width="13" height="11" rx="1.5" /><path d="M6 2.5v11" />
				</svg>
			</span>
		</button>
	{:else}
		<a class="brand item h-[44px] flex-none border-l-2 border-l-transparent text-base-content no-underline" href="/" title="LogBear — all projects">
			<span class="ico"><Mark size={22} animate /></span>
			<span class="lbl text-[15px] font-semibold tracking-normal">LogBear</span>
		</a>
	{/if}

	<div class="pane flex-1 overflow-x-hidden pb-2">
		<div class="flex h-[26px] items-center justify-between pr-[14px] pl-[14px]">
			<span class="fade label-xs">Projects</span>
			<a
				class="fade text-[10px] tracking-[0.16em] no-underline uppercase hover:text-base-content {active ===
				'/'
					? 'text-base-content'
					: 'text-meta'}"
				href="/"
				tabindex={rail.collapsed ? -1 : 0}>all</a
			>
		</div>
		{#if app.projects.length === 0}
			<p class="fade meta m-0 px-[14px] py-0.5 text-[11px]">{app.loaded ? 'None yet' : 'Loading…'}</p>
		{:else}
			<ul class="m-0 list-none p-0">
				{#each app.projects as p (p.id)}
					{@const errs = app.statuses[p.id]?.errors_last_hour ?? 0}
					<li>
						<a
							href="/project/{p.id}/live"
							class="{linkBase} {currentId === p.id && active.startsWith('/project/') ? linkOn : linkOff}"
							title={rail.collapsed ? undefined : tildePath(p.root)}
							aria-label={p.name}
							use:tip={`${p.name}${isRunning(p.id) ? ' · running' : ''}${errs ? ` · ${errs} errors in the last hour` : ''}`}
						>
							<span class="ico">
								<span
									class="relative grid h-[20px] w-[24px] place-items-center rounded-[2px] border border-hair text-[10px] lowercase"
									>{p.name.slice(0, 2)}<span
										class="absolute -top-[3px] -right-[3px] size-1.5 rounded-full {isRunning(p.id)
											? 'bg-success shadow-[0_0_0_2px_var(--color-base-200)]'
											: errs > 0 && rail.collapsed
												? 'bg-lvl-error shadow-[0_0_0_2px_var(--color-base-200)]'
												: 'bg-transparent'}"
										aria-hidden="true"
									></span></span
								>
							</span>
							<span class="lbl truncate">{p.name}</span>
							{#if errs > 0}
								<span class="lbl tnum mr-[14px] ml-auto text-[10px] text-lvl-error">{errs}</span>
							{/if}
						</a>
					</li>
				{/each}
			</ul>
		{/if}

		{#if sections.length}
			<div class="rule-t mt-3 pt-1.5">
				<div class="flex h-[26px] items-center px-[14px]">
					<span class="fade label-xs truncate">{app.project(currentId!)?.name ?? 'Project'}</span>
				</div>
				<ul class="m-0 list-none p-0">
					{#each sections as s (s.key)}
						<li>
							<a
								href={s.href}
								class="{linkBase} {active.startsWith(s.href) ? linkOn : linkOff}"
								aria-label={s.label}
								use:tip={s.key === 'live' && isRunning(currentId!) ? 'Live · running' : s.label}
							>
								<span class="ico">
									<span class="relative grid place-items-center">
										{@render icon(s.key)}
										{#if s.key === 'live' && isRunning(currentId!)}
											<span
												class="is-running absolute -top-[3px] -right-[4px] size-1.5 rounded-full bg-success"
												aria-hidden="true"
											></span>
										{/if}
									</span>
								</span>
								<span class="lbl">{s.label}</span>
								{#if s.key === 'live' && isRunning(currentId!)}
									<span class="lbl is-running mr-[14px] ml-auto text-[9px] tracking-[0.16em] text-success uppercase"
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

	<div class="rule-t flex-none py-1.5">
		<a
			class="item text-[11px] no-underline {quiet}"
			href="/settings"
			title={rail.collapsed ? undefined : daemonTip}
			aria-label={app.connected ? 'Daemon connected' : 'Daemon down'}
			use:tip={daemonTip}
		>
			<span class="ico">
				<span class="size-[7px] rounded-full {app.connected ? 'bg-success' : 'is-running bg-flare'}"></span>
			</span>
			<span class="lbl">{app.connected ? `daemon ${app.daemon?.version}` : 'daemon down'}</span>
		</a>
		<a
			class="{linkBase} {active.startsWith('/settings') ? linkOn : linkOff}"
			href="/settings"
			aria-label="Settings"
			use:tip={'Settings'}
		>
			<span class="ico">{@render icon('settings')}</span>
			<span class="lbl">Settings</span>
		</a>
		<button
			type="button"
			class="item w-full cursor-pointer border-l-2 border-l-transparent bg-transparent text-[12.5px] {quiet}"
			onclick={() => theme.toggle()}
			aria-label={theme.isDark ? 'Switch to light theme' : 'Switch to dark theme'}
			use:tip={theme.isDark ? 'Ink theme · switch to bone' : 'Bone theme · switch to ink'}
		>
			<span class="ico">{@render icon('theme')}</span>
			<span class="lbl">{theme.isDark ? 'Ink' : 'Bone'}</span>
		</button>

		{#if app.mock}
			<p
				class="fade mt-1 mb-0 cursor-help px-[14px] text-[9px] tracking-[0.16em] whitespace-nowrap text-warning uppercase"
				title="No Tauri backend detected — every screen is showing generated data"
			>
				sample data
			</p>
		{/if}
	</div>
</nav>

{#if tipState}
	<div
		class="rail-tip pointer-events-none fixed z-50 -translate-y-1/2 rounded-[3px] border border-hair-strong bg-base-100 px-2 py-1 text-[11.5px] whitespace-nowrap text-base-content shadow-[0_4px_14px_rgb(0_0_0/0.14)]"
		style:left="{tipState.x}px"
		style:top="{tipState.y}px"
		role="tooltip"
	>
		{tipState.text}
	</div>
{/if}

<style>
	/* Collapsed width, and the icon column every row shares. On macOS the strip has
	   to stay wide enough for the window buttons drawn over the title bar. */
	.rail {
		--rail-c: 52px;
		width: 184px;
	}
	:global(:root[data-platform='macos']) .rail {
		--rail-c: 76px;
	}
	.rail.collapsed {
		width: var(--rail-c);
	}
	.rail.ready {
		transition: width 200ms cubic-bezier(0.2, 0, 0, 1);
	}

	/* A row: an icon column and a label. Both widths animate on the same curve, so
	   the icon glides to the centre of the strip as the rail closes. */
	.rail :global(.item) {
		display: flex;
		align-items: center;
		height: 30px;
		padding: 0;
		white-space: nowrap;
		text-align: left;
	}
	.rail :global(.ico) {
		display: grid;
		flex: none;
		place-items: center;
		width: 36px;
		height: 100%;
	}
	/* Collapsed, the column fills the strip so the icon sits centred in it. The 2px
	   active border is part of the row, hence the 2px less. */
	.rail.collapsed :global(.ico) {
		width: calc(var(--rail-c) - 2px);
	}
	.rail.ready :global(.ico) {
		transition: width 200ms cubic-bezier(0.2, 0, 0, 1);
	}

	/* Labels fade on the way out before the width closes over them, and in after it
	   has opened, so text is never seen being clipped mid-slide. */
	.rail :global(.lbl),
	.rail :global(.fade) {
		opacity: 1;
		transition: opacity 140ms ease 60ms;
	}
	.rail.collapsed :global(.lbl),
	.rail.collapsed :global(.fade) {
		opacity: 0;
		pointer-events: none;
		transition: opacity 80ms ease;
	}

	.rail-tip {
		animation: tip-in 90ms ease-out;
	}
	@keyframes tip-in {
		from {
			opacity: 0;
			margin-left: -3px;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.rail.ready,
		.rail.ready :global(.ico),
		.rail :global(.lbl),
		.rail :global(.fade),
		.rail-tip {
			transition: none;
			animation: none;
		}
	}
</style>
