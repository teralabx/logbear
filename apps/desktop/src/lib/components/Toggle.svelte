<script lang="ts">
	let {
		checked = $bindable(false),
		label,
		hint = '',
		disabled = false,
		onchange = undefined
	}: {
		checked?: boolean;
		label: string;
		hint?: string;
		disabled?: boolean;
		onchange?: ((v: boolean) => void) | undefined;
	} = $props();

	function flip() {
		if (disabled) return;
		checked = !checked;
		onchange?.(checked);
	}
</script>

<div class="flex items-start gap-2.5 py-[7px] {disabled ? 'opacity-50' : ''}">
	<button
		type="button"
		role="switch"
		aria-checked={checked}
		aria-label={label}
		{disabled}
		onclick={flip}
		class="relative mt-px h-[17px] w-[30px] flex-none cursor-pointer rounded-[2px] border transition-[background-color] duration-[140ms] disabled:cursor-not-allowed {checked
			? 'border-base-content bg-base-content'
			: 'border-hair-strong bg-transparent'}"
	>
		<span
			class="absolute top-0.5 left-0.5 h-[11px] w-[11px] rounded-[1px] transition-[transform,background-color] duration-[140ms] {checked
				? 'translate-x-[13px] bg-base-100'
				: 'bg-hair-strong'}"
		></span>
	</button>
	<div class="min-w-0">
		<button
			type="button"
			class="cursor-pointer bg-transparent p-0 text-left text-[12.5px] text-base-content disabled:cursor-not-allowed"
			{disabled}
			onclick={flip}>{label}</button
		>
		{#if hint}<p class="meta mt-0.5 max-w-[60ch] text-[11px] leading-[1.5]">{hint}</p>{/if}
	</div>
</div>
