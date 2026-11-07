// The wording of every confirmation for something that cannot be undone.

import { fmtCommand, pluralise } from '#lib/format';
import type { Run } from '#lib/api/types';

export interface ConfirmCopy {
	title: string;
	body: string;
	action: string;
}

export const confirmDeleteRun = (run: Pick<Run, 'id' | 'command'>): ConfirmCopy => ({
	title: `Delete run #${run.id}?`,
	body: `${fmtCommand(run.command)} and all of its stored output will be permanently deleted, and it stops counting towards its error signatures. This cannot be undone.`,
	action: 'Delete run'
});

export const confirmSquashRun = (run: Pick<Run, 'id'>): ConfirmCopy => ({
	title: `Squash run #${run.id}?`,
	body: 'Its stored output is rewritten to the first occurrence of each distinct entry. Repeats that differ only in timestamps, ids or numbers are deleted for good; counts and signatures are kept. This cannot be undone.',
	action: 'Squash run'
});

export const confirmSquashProject = (name: string, runsStored: number): ConfirmCopy => ({
	title: `Squash finished runs in ${name}?`,
	body: `Every finished run among the ${pluralise(runsStored, 'stored run')} is rewritten to the first occurrence of each distinct entry, now rather than when the cap is reached. Repeats are deleted for good; counts and signatures are kept. This cannot be undone.`,
	action: 'Squash finished runs'
});
