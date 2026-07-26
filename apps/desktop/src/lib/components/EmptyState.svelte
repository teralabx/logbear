<script lang="ts">
	import type { Snippet } from 'svelte';

	/**
	 * Empty states teach. Every one of these says what would put content here,
	 * in the exact words you would type to make it happen.
	 */
	let {
		title,
		body,
		command = '',
		children
	}: { title: string; body: string; command?: string; children?: Snippet } = $props();

	let copied = $state(false);

	async function copy() {
		if (!command) return;
		try {
			await navigator.clipboard.writeText(command);
			copied = true;
			setTimeout(() => (copied = false), 1400);
		} catch {
			copied = false;
		}
	}
</script>

<div class="empty">
	<h3>{title}</h3>
	<p class="body">{body}</p>
	{#if command}
		<div class="cmd">
			<code>{command}</code>
			<button type="button" onclick={copy} aria-label="Copy command to clipboard">
				{copied ? 'copied' : 'copy'}
			</button>
		</div>
	{/if}
	{#if children}<div class="extra">{@render children()}</div>{/if}
</div>

<style>
	.empty {
		max-width: 520px;
		padding: 28px 20px;
	}
	h3 {
		font-size: 14px;
		font-weight: 600;
		margin: 0 0 6px;
	}
	.body {
		margin: 0;
		font-size: 12.5px;
		color: var(--rl-meta);
		line-height: 1.6;
	}
	.cmd {
		margin-top: 14px;
		display: flex;
		align-items: stretch;
		border: 1px solid var(--rl-hair);
		border-radius: 2px;
		background: var(--rl-code-bg);
		max-width: 100%;
	}
	code {
		flex: 1;
		padding: 7px 10px;
		font-size: 12px;
		overflow-x: auto;
		white-space: pre;
		min-width: 0;
	}
	code::before {
		content: '$ ';
		color: var(--rl-meta);
	}
	button {
		flex: none;
		border: 0;
		border-left: 1px solid var(--rl-hair);
		background: transparent;
		color: var(--rl-meta);
		font-size: 10px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		padding: 0 10px;
		cursor: pointer;
	}
	button:hover {
		color: var(--color-base-content);
	}
	.extra {
		margin-top: 16px;
	}
</style>
