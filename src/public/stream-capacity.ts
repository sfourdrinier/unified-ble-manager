// src/public/stream-capacity.ts

/** Normative upper bound for retained item counts in application-facing bounded streams. */
export const MAX_PUBLIC_STREAM_ITEM_CAPACITY = 65_536

/** Normative per-stream byte quota; aggregate package quotas remain independently bounded. */
export const MAX_PUBLIC_STREAM_BYTE_CAPACITY = 4 * 1024 * 1024
