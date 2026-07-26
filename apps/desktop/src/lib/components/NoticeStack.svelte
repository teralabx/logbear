<script lang="ts">
	import { app } from '$lib/stores/app.svelte';
	import { fmtAgo } from '$lib/format';

	/**
	 * Transient notices from the daemon, bottom-right. Repeats of the same
	 * signature collapse into a count rather than stacking, so a failing loop
	 * does not bury the screen.
	 */
</script>

{#if app.notices.length}
	<div class="stack" role="status" aria-live="polite">
		{#each app.notices as n (n.replaces_key ?? n.at)}
			<div class="notice" data-level={n.level}>
				<span class="stripe" aria-hidden="true"></span>
				<div class="content">
					<div class="top">
						<a class="title" href="/project/{n.project_id}/live">{n.title}</a>
						{#if n.count > 1}<span class="count tnum">×{n.count}</span>{/if}
						<button
							type="button"
							class="x"
							onclick={() => app.dismissNotice(n.at)}
							aria-label="Dismiss">×</button
						>
					</div>
					<p class="body">{n.body}</p>
					<span class="when meta">{n.project_name} · {fmtAgo(n.at)}</span>
				</div>
			</div>
		{/each}
	</div>
{/if}

<style>
	.stack {
		position: fixed;
		right: 14px;
		bottom: 14px;
		z-index: 40;
		display: flex;
		flex-direction: column;
		gap: 8px;
		width: 330px;
		max-width: calc(100vw - 28px);
	}
	.notice {
		display: flex;
		background: var(--color-base-200);
		border: 1px solid var(--rl-hair-strong);
		border-radius: 3px;
		box-shadow: 0 6px 20px rgb(0 0 0 / 0.28);
		overflow: hidden;
	}
	.stripe {
		width: 2px;
		flex: none;
		background: var(--lvl);
	}
	.content {
		padding: 8px 10px;
		min-width: 0;
		flex: 1;
	}
	.top {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}
	.title {
		font-size: 12px;
		font-weight: 600;
		color: var(--color-base-content);
		text-decoration: none;
		flex: 1;
		min-width: 0;
	}
	.title:hover {
		text-decoration: underline;
	}
	.count {
		font-size: 11px;
		color: var(--lvl);
		font-weight: 600;
	}
	.x {
		border: 0;
		background: none;
		color: var(--rl-meta);
		cursor: pointer;
		font-size: 15px;
		line-height: 1;
		padding: 0 2px;
	}
	.x:hover {
		color: var(--color-base-content);
	}
	.body {
		margin: 3px 0 4px;
		font-size: 11.5px;
		color: var(--rl-meta);
		overflow: hidden;
		display: -webkit-box;
		-webkit-line-clamp: 3;
		line-clamp: 3;
		-webkit-box-orient: vertical;
		overflow-wrap: anywhere;
	}
	.when {
		font-size: 10px;
	}
	[data-level='error'] {
		--lvl: var(--rl-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--rl-lvl-warn);
	}
	[data-level='info'],
	[data-level='debug'],
	[data-level='trace'],
	[data-level='unknown'] {
		--lvl: var(--rl-lvl-info);
	}
</style>
