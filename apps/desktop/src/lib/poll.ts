/**
 * Call `tick` every `ms` while the document is visible, and once on coming back, so a
 * window nobody can see does no work and shows fresh numbers the moment it is seen again.
 * Returns a disposer.
 */
export function pollWhileVisible(tick: () => void, ms: number, doc: Document = document): () => void {
	const fire = () => {
		if (doc.visibilityState === 'visible') tick();
	};
	const timer = setInterval(fire, ms);
	doc.addEventListener('visibilitychange', fire);
	return () => {
		clearInterval(timer);
		doc.removeEventListener('visibilitychange', fire);
	};
}
