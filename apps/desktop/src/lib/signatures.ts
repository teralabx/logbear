// Reading and ordering the Errors screen's signatures.

import { fingerprints } from '$lib/api';
import type { Fingerprint, ProjectRef } from '$lib/api/types';

/** Signatures read per screen. The server applies the level filter before this limit. */
export const SIGNATURE_LIMIT = 200;

export type SignatureSort = 'frequency' | 'recency' | 'spread';

/**
 * Filtering has to happen on the server. The limit keeps the most recently seen signatures
 * of all levels, so filtering that page here would lose any error pushed out by 200 newer
 * warnings.
 */
export const readSignatures = (project: ProjectRef, errorsOnly: boolean): Promise<Fingerprint[]> =>
	fingerprints(project, SIGNATURE_LIMIT, errorsOnly);

export function sortSignatures(list: Fingerprint[], sort: SignatureSort): Fingerprint[] {
	return [...list].sort((a, b) => {
		if (sort === 'recency') return b.last_seen - a.last_seen;
		if (sort === 'spread') return b.run_count - a.run_count || b.total_count - a.total_count;
		return b.total_count - a.total_count || b.last_seen - a.last_seen;
	});
}
