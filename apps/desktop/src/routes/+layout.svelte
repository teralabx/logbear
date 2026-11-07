<script lang="ts">
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import Rail from '#lib/components/Rail.svelte';
	import NoticeStack from '#lib/components/NoticeStack.svelte';
	import FirstRun from '#lib/components/FirstRun.svelte';
	import { firstRunPending } from '#lib/api';
	import { app } from '#lib/stores/app.svelte';
	import { theme } from '#lib/stores/theme.svelte';
	import { rail } from '#lib/stores/rail.svelte';

	let { children }: { children: Snippet } = $props();

	// Nothing that talks to the daemon may mount until the first-run question has been
	// answered: reaching for the daemon starts it.
	let gate = $state<'checking' | 'first-run' | 'ready'>('checking');
	let stopApp: (() => void) | null = null;

	function start() {
		gate = 'ready';
		stopApp = app.init();
	}

	// One subscription to the daemon event channel for the whole app, and one
	// media-query listener for the theme. Everything else reads the stores.
	onMount(() => {
		// macOS draws its window buttons *over* the content when the title bar is an
		// overlay, so the rail has to reserve room for them. Marking the platform on the
		// root keeps that as one CSS rule rather than a conditional in every component.
		if (navigator.platform.startsWith('Mac') || navigator.userAgent.includes('Mac OS X')) {
			document.documentElement.dataset.platform = 'macos';
		}
		const stopTheme = theme.init();
		const stopRail = rail.init();
		let unmounted = false;
		firstRunPending()
			.catch(() => false)
			.then((pending) => {
				if (unmounted) return;
				if (pending) gate = 'first-run';
				else start();
			});
		return () => {
			unmounted = true;
			stopRail();
			stopApp?.();
			stopTheme();
		};
	});
</script>

{#if gate === 'first-run'}
	<FirstRun ondone={start} />
{:else if gate === 'ready'}
	<div class="flex h-screen w-screen overflow-hidden">
		<Rail />
		<main class="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
			{@render children()}
		</main>
	</div>

	<NoticeStack />
{/if}
