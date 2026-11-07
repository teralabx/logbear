import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

/** Read from the stylesheet, so a token edited there cannot drift from what is tested. */
const css = readFileSync(resolve(__dirname, '../app.css'), 'utf8');
const lightPlugin = css.slice(css.indexOf('name: "logbear-light"'), css.indexOf('name: "logbear-dark"'));
const lightTokens = css.slice(css.indexOf('[data-theme="logbear-light"] {'));

function token(block: string, name: string): string {
	const found = block.match(new RegExp(`--color-${name}:\\s*(#[0-9a-f]{6})`));
	if (!found) throw new Error(`no --color-${name}`);
	return found[1];
}

function luminance(hex: string): number {
	const [r, g, b] = [1, 3, 5]
		.map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
		.map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
	return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string): number {
	const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
	return (hi + 0.05) / (lo + 0.05);
}

describe('light theme contrast', () => {
	for (const text of ['meta', 'lvl-debug', 'lvl-trace', 'lvl-unknown']) {
		for (const ground of ['base-100', 'base-200']) {
			it(`${text} on ${ground} reaches 4.5:1`, () => {
				const ratio = contrast(token(lightTokens, text), token(lightPlugin, ground));
				expect(ratio).toBeGreaterThanOrEqual(4.5);
			});
		}
	}
});
