<script lang="ts">
	import '../app.css';
	import { onMount, type Snippet } from 'svelte';
	import Rail from '$lib/components/Rail.svelte';
	import NoticeStack from '$lib/components/NoticeStack.svelte';
	import { app } from '$lib/stores/app.svelte';
	import { theme } from '$lib/stores/theme.svelte';

	let { children }: { children: Snippet } = $props();

	// One subscription to the daemon event channel for the whole app, and one
	// media-query listener for the theme. Everything else reads the stores.
	onMount(() => {
		const stopTheme = theme.init();
		const stopApp = app.init();
		return () => {
			stopApp();
			stopTheme();
		};
	});
</script>

<div class="shell">
	<Rail />
	<main class="main">
		{@render children()}
	</main>
</div>

<NoticeStack />

<style>
	.shell {
		display: flex;
		height: 100vh;
		width: 100vw;
		overflow: hidden;
	}
	.main {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}
</style>
