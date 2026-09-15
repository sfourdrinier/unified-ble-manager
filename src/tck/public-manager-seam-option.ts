// src/tck/public-manager-seam-option.ts
//
// Minimal seam-option type for public-manager construction injection.
// Kept separate from public-manager-seam.ts so the runner can import it as
// a type-only dependency without a runtime cycle.

export type PublicManagerSeamOption = { readonly kind: 'ts-reference' } | { readonly kind: 'rust-stub' }
