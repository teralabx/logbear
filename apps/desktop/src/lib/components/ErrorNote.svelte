<script lang="ts">
	/**
	 * Failure surface. Says what failed, and what the reader can do about it.
	 * Never "Something went wrong".
	 */
	let {
		what,
		detail = '',
		retry = undefined
	}: { what: string; detail?: string; retry?: (() => void) | undefined } = $props();
</script>

<div class="note" role="alert">
	<span class="stripe" aria-hidden="true"></span>
	<div class="body">
		<p class="what">{what}</p>
		{#if detail}<pre class="detail">{detail}</pre>{/if}
	</div>
	{#if retry}
		<button type="button" onclick={retry}>Try again</button>
	{/if}
</div>

<style>
	.note {
		display: flex;
		align-items: flex-start;
		gap: 10px;
		border: 1px solid color-mix(in oklch, var(--color-flare) 40%, transparent);
		border-radius: 3px;
		background: color-mix(in oklch, var(--color-flare) 8%, transparent);
		padding: 0;
		margin: 12px 0;
		overflow: hidden;
	}
	.stripe {
		width: 2px;
		align-self: stretch;
		background: var(--color-flare);
		flex: none;
	}
	.body {
		padding: 9px 0;
		flex: 1;
		min-width: 0;
	}
	.what {
		margin: 0;
		font-size: 12.5px;
		font-weight: 500;
	}
	.detail {
		margin: 5px 0 0;
		font-size: 11px;
		color: var(--rl-meta);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	button {
		flex: none;
		margin: 9px 10px 9px 0;
		border: 1px solid var(--rl-hair-strong);
		border-radius: 2px;
		background: transparent;
		color: var(--color-base-content);
		font: inherit;
		font-size: 11px;
		padding: 3px 9px;
		cursor: pointer;
	}
	button:hover {
		border-color: var(--color-base-content);
	}
</style>
