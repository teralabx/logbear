// Rendering structured entries back into ANSI for the xterm pane.
//
// Historic runs are replayed from `raw_stream`, which is the original bytes with
// their own colour intact. Entries that arrive live over the event channel are
// structured, not raw, so we re-colour them here by level. Keeping this in one
// place means the live tail and the replay of the same run look identical.

import type { EntryView, Level } from './api/types';

export const SGR = {
	reset: '\x1b[0m',
	bold: '\x1b[1m',
	dim: '\x1b[2m',
	red: '\x1b[31m',
	green: '\x1b[32m',
	yellow: '\x1b[33m',
	cyan: '\x1b[36m',
	grey: '\x1b[90m'
} as const;

function colourFor(level: Level): string {
	switch (level) {
		case 'error':
			return SGR.red;
		case 'warn':
			return SGR.yellow;
		case 'debug':
		case 'trace':
			return SGR.grey;
		case 'info':
			return SGR.cyan;
		default:
			return '';
	}
}

/** One entry as terminal bytes, CRLF terminated. */
export function entryToAnsi(e: EntryView): string {
	const colour = colourFor(e.level);
	return (
		e.text
			.split('\n')
			.map((l, i) => (i === 0 ? colour + l + SGR.reset : SGR.dim + l + SGR.reset))
			.join('\r\n') + '\r\n'
	);
}
