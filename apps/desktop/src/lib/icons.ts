// Every icon in the app is Lucide, drawn at one weight.
//
// The stroke is absolute rather than scaled with the icon, so a 12px arrow beside a link
// and a 15px rail icon are both 1.5px lines: the same weight as the hairline rules and
// close to the weight of IBM Plex Mono at the sizes the UI sets it.

export const ICON = { size: 15, strokeWidth: 1.5, absoluteStrokeWidth: true } as const;

/** For icons set inline with 11–12px text. */
export const INLINE = { ...ICON, size: 12 } as const;
