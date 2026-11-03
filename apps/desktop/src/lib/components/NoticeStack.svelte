<script lang="ts">
	import { app } from '$lib/stores/app.svelte';
	import { fmtAgo } from '$lib/format';
	import { INLINE } from '$lib/icons';
	import { X } from '@lucide/svelte';

	/**
	 * Transient notices from the daemon, bottom-right. Repeats of the same
	 * signature collapse into a count rather than stacking, so a failing loop
	 * does not bury the screen.
	 */
</script>

{#if app.notices.length}
	<div
		class="fixed right-[14px] bottom-[14px] z-40 flex w-[330px] max-w-[calc(100vw-28px)] flex-col gap-2"
		role="status"
		aria-live="polite"
	>
		{#each app.notices as n (n.replaces_key ?? n.at)}
			<div
				class="flex overflow-hidden rounded-[3px] border border-hair-strong bg-base-200 shadow-[0_6px_20px_rgb(0_0_0/0.28)]"
				data-level={n.level}
			>
				<span class="w-[2px] flex-none bg-[var(--lvl)]" aria-hidden="true"></span>
				<div class="min-w-0 flex-1 px-2.5 py-2">
					<div class="flex items-baseline gap-2">
						<a
							class="min-w-0 flex-1 text-[12px] font-semibold text-base-content no-underline hover:underline"
							href="/project/{n.project_id}/live">{n.title}</a
						>
						{#if n.count > 1}<span class="tnum text-[11px] font-semibold text-[var(--lvl)]"
								>×{n.count}</span
							>{/if}
						<button
							type="button"
							class="grid cursor-pointer place-items-center self-center bg-transparent px-0.5 text-meta hover:text-base-content"
							onclick={() => app.dismissNotice(n.at)}
							aria-label="Dismiss"
							title="Dismiss"><X {...INLINE} aria-hidden="true" /></button
						>
					</div>
					<p class="mt-[3px] mb-1 line-clamp-3 text-[11.5px] text-meta [overflow-wrap:anywhere]">
						{n.body}
					</p>
					<span class="meta text-[10px]">{n.project_name} · {fmtAgo(n.at)}</span>
				</div>
			</div>
		{/each}
	</div>
{/if}

<style>
	/* Attribute-driven custom property mapping: not expressible as a utility. */
	[data-level='error'] {
		--lvl: var(--color-lvl-error);
	}
	[data-level='warn'] {
		--lvl: var(--color-lvl-warn);
	}
	[data-level='info'],
	[data-level='debug'],
	[data-level='trace'],
	[data-level='unknown'] {
		--lvl: var(--color-lvl-info);
	}
</style>
