<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { SearchAddon } from '@xterm/addon-search';
	import '@xterm/xterm/css/xterm.css';

	/**
	 * xterm.js pane for stored output. Read-only: stdin is disabled, because
	 * nothing here is a live TTY — it is a recording.
	 *
	 * The terminal always renders on the ink ground regardless of app theme.
	 * Terminal output carries its own ANSI colours and re-mapping them to a
	 * light palette makes stored output look nothing like what the developer
	 * actually saw.
	 */
	let {
		cols = null,
		class: klass = ''
	}: { cols?: number | null; class?: string } = $props();

	let host: HTMLDivElement | undefined = $state();
	let term: Terminal | null = null;
	let fit: FitAddon | null = null;
	let search: SearchAddon | null = null;
	let ro: ResizeObserver | null = null;

	// The ink palette, mapped onto the standard 16 ANSI slots.
	const palette = {
		background: '#0B0E10',
		foreground: '#E6E8E9',
		cursor: '#E5533D',
		cursorAccent: '#0B0E10',
		selectionBackground: 'rgba(245,244,241,0.20)',
		black: '#0F1214',
		red: '#E5533D',
		green: '#4F9D77',
		yellow: '#D9932B',
		blue: '#6E93A8',
		magenta: '#B07AA1',
		cyan: '#63A6A0',
		white: '#C9CED2',
		brightBlack: '#5D666E',
		brightRed: '#F0745F',
		brightGreen: '#6BB690',
		brightYellow: '#E8AC53',
		brightBlue: '#8DB0C4',
		brightMagenta: '#C795B9',
		brightCyan: '#82BEB8',
		brightWhite: '#F5F4F1'
	};

	export function write(data: string) {
		term?.write(data);
	}

	/** Replace everything on screen. Used when switching runs. */
	export function replace(data: string) {
		term?.reset();
		if (data) term?.write(data);
	}

	export function clear() {
		term?.reset();
	}

	export function scrollToBottom() {
		term?.scrollToBottom();
	}

	export function findNext(q: string): boolean {
		return (
			search?.findNext(q, {
				decorations: {
					matchBackground: '#3A2E1A',
					matchOverviewRuler: '#D9932B',
					activeMatchBackground: '#7A4A16',
					activeMatchColorOverviewRuler: '#E5533D'
				}
			}) ?? false
		);
	}

	export function findPrevious(q: string): boolean {
		return search?.findPrevious(q) ?? false;
	}

	export function clearSearch() {
		search?.clearDecorations();
	}

	export function refit() {
		try {
			fit?.fit();
		} catch {
			/* the pane can be zero-sized mid-transition; the observer retries */
		}
	}

	onMount(() => {
		term = new Terminal({
			fontFamily:
				'"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
			fontSize: 12,
			lineHeight: 1.35,
			letterSpacing: 0,
			theme: palette,
			cursorBlink: false,
			cursorInactiveStyle: 'none',
			disableStdin: true,
			convertEol: true,
			scrollback: 20000,
			smoothScrollDuration: 0,
			allowProposedApi: true,
			cols: cols ?? 120,
			rows: 24
		});
		fit = new FitAddon();
		search = new SearchAddon();
		term.loadAddon(fit);
		term.loadAddon(search);
		if (host) term.open(host);
		untrack(() => refit());

		if (host) {
			ro = new ResizeObserver(() => refit());
			ro.observe(host);
		}

		return () => {
			ro?.disconnect();
			ro = null;
			search = null;
			fit = null;
			term?.dispose();
			term = null;
		};
	});
</script>

<div
	class="term h-full min-h-0 w-full bg-term-bg py-2 pr-1 pl-2.5 {klass}"
	bind:this={host}
></div>

<style>
	/* Third-party (xterm.js) override — must reach into markup we do not own. */
	.term :global(.xterm) {
		height: 100%;
	}
</style>
