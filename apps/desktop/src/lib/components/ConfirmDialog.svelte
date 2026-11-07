<script lang="ts">
	import type { ConfirmCopy } from '$lib/confirm';

	/**
	 * Asks before something that cannot be undone. Set `request` to open it. Focus lands on
	 * Cancel and Escape cancels, so a stray Enter or keystroke can never confirm.
	 */
	let {
		request,
		onconfirm,
		oncancel
	}: {
		request: ConfirmCopy | null;
		onconfirm: () => void;
		oncancel: () => void;
	} = $props();

	let dialog = $state<HTMLDialogElement | undefined>();

	$effect(() => {
		if (!dialog) return;
		if (request && !dialog.open) dialog.showModal();
		else if (!request && dialog.open) dialog.close();
	});
</script>

<dialog
	bind:this={dialog}
	class="m-auto w-[min(440px,calc(100vw-32px))] rounded-[3px] border border-hair-strong bg-base-200 p-4 text-base-content shadow-[0_8px_24px_rgb(0_0_0/0.3)] backdrop:bg-[rgb(0_0_0/0.45)]"
	aria-labelledby="confirm-title"
	oncancel={(e) => {
		e.preventDefault();
		oncancel();
	}}
>
	{#if request}
		<h3 id="confirm-title" class="text-[13px] font-semibold">{request.title}</h3>
		<p class="meta mt-2 text-[12px] leading-[1.6] [overflow-wrap:anywhere]">{request.body}</p>
		<div class="mt-4 flex justify-end gap-2">
			<!-- svelte-ignore a11y_autofocus -->
			<button
				type="button"
				autofocus
				class="btn btn-outline btn-sm h-auto border-hair-strong px-[11px] py-[4px] text-[12px] font-normal hover:border-base-content hover:bg-transparent"
				onclick={oncancel}>Cancel</button
			>
			<button
				type="button"
				class="cursor-pointer rounded-[2px] bg-flare px-[11px] py-[4px] text-[12px] font-medium text-flare-content"
				onclick={onconfirm}>{request.action}</button
			>
		</div>
	{/if}
</dialog>
