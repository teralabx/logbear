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

<div class="row" class:disabled>
	<button
		type="button"
		role="switch"
		aria-checked={checked}
		aria-label={label}
		{disabled}
		onclick={flip}
		class="sw"
		class:on={checked}
	>
		<span class="knob"></span>
	</button>
	<div class="text">
		<button type="button" class="lbl" {disabled} onclick={flip}>{label}</button>
		{#if hint}<p class="hint meta">{hint}</p>{/if}
	</div>
</div>

<style>
	.row {
		display: flex;
		gap: 10px;
		align-items: flex-start;
		padding: 7px 0;
	}
	.row.disabled {
		opacity: 0.5;
	}
	.sw {
		flex: none;
		width: 30px;
		height: 17px;
		margin-top: 1px;
		border-radius: 2px;
		border: 1px solid var(--rl-hair-strong);
		background: transparent;
		position: relative;
		cursor: pointer;
		transition: background-color 140ms;
	}
	.sw.on {
		background: var(--color-base-content);
		border-color: var(--color-base-content);
	}
	.sw:disabled {
		cursor: not-allowed;
	}
	.knob {
		position: absolute;
		top: 2px;
		left: 2px;
		width: 11px;
		height: 11px;
		border-radius: 1px;
		background: var(--rl-hair-strong);
		transition:
			transform 140ms,
			background-color 140ms;
	}
	.sw.on .knob {
		transform: translateX(13px);
		background: var(--color-base-100);
	}
	.text {
		min-width: 0;
	}
	.lbl {
		background: none;
		border: 0;
		padding: 0;
		font: inherit;
		font-size: 12.5px;
		color: var(--color-base-content);
		cursor: pointer;
		text-align: left;
	}
	.lbl:disabled {
		cursor: not-allowed;
	}
	.hint {
		margin: 2px 0 0;
		font-size: 11px;
		line-height: 1.5;
		max-width: 60ch;
	}
</style>
