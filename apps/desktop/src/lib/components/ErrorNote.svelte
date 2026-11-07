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

<div
	class="my-3 flex items-start gap-2.5 overflow-hidden rounded-[3px] border border-[color-mix(in_oklch,var(--color-flare)_40%,transparent)] bg-[color-mix(in_oklch,var(--color-flare)_8%,transparent)] p-0"
	role="alert"
>
	<span class="w-[2px] flex-none self-stretch bg-flare" aria-hidden="true"></span>
	<div class="min-w-0 flex-1 py-[9px]">
		<p class="text-[12.5px] font-medium">{what}</p>
		{#if detail}<pre
				class="mt-[5px] text-[12px] whitespace-pre-wrap text-meta [overflow-wrap:anywhere]">{detail}</pre>{/if}
	</div>
	{#if retry}
		<button
			type="button"
			class="my-[9px] mr-2.5 flex-none cursor-pointer rounded-[2px] border border-hair-strong bg-transparent px-[9px] py-[3px] text-[12px] text-base-content hover:border-base-content"
			onclick={retry}>Try again</button
		>
	{/if}
</div>
