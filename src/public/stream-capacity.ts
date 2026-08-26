// src/public/stream-capacity.ts

/**
 * Normative upper bound for retained item counts in application-facing bounded streams.
 *
 * A ceiling on host policy rather than the policy itself: the operating budget
 * is caller-supplied per operation through `delivery` / `stream`, and this only
 * bounds what a caller may ask for. It stays fixed because a stream budget above
 * it stops being backpressure and becomes an unbounded queue.
 */
export const MAX_PUBLIC_STREAM_ITEM_CAPACITY = 65_536

/**
 * Normative per-stream byte quota; aggregate package quotas remain independently bounded.
 *
 * As above: a ceiling on what `delivery` / `stream` may request, not a default.
 */
export const MAX_PUBLIC_STREAM_BYTE_CAPACITY = 4 * 1024 * 1024
