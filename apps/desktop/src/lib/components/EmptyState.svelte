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

<div class="max-w-[520px] px-5 py-7">
	<h3 class="mb-1.5 text-[14px] font-semibold">{title}</h3>
	<p class="text-[12.5px] leading-[1.6] text-meta">{body}</p>
	{#if command}
		<div class="mt-3.5 flex max-w-full items-stretch rounded-[2px] border border-hair bg-code-bg">
			<code
				class="min-w-0 flex-1 overflow-x-auto px-2.5 py-[7px] text-[12px] whitespace-pre before:text-meta before:content-['$_']"
				>{command}</code
			>
			<button
				type="button"
				class="flex-none cursor-pointer border-l border-hair bg-transparent px-2.5 text-[10px] tracking-[0.16em] text-meta uppercase hover:text-base-content"
				onclick={copy}
				aria-label="Copy command to clipboard"
			>
				{copied ? 'copied' : 'copy'}
			</button>
		</div>
	{/if}
	{#if children}<div class="mt-4">{@render children()}</div>{/if}
</div>
