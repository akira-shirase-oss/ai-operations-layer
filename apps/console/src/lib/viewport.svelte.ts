// Whether the screen is narrower than Tailwind's `sm` breakpoint (640 px). Below it, the
// console's tables become stacked rows with the status first, so a status or an outcome is
// never behind a sideways scroll (review U1).
class Viewport {
	narrow = $state(false);

	constructor() {
		if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
		const query = window.matchMedia('(max-width: 639.98px)');
		this.narrow = query.matches;
		query.addEventListener('change', (e) => (this.narrow = e.matches));
	}
}

export const viewport = new Viewport();
