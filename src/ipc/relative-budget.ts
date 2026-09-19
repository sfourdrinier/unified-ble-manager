// src/ipc/relative-budget.ts — a caller deadline crossing into another clock domain

import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'

/**
 * Replaces the caller's absolute `payload.deadline` with `payload.budgetMs`.
 *
 * The deadline is a `performance.now()` instant on the sender's monotonic
 * clock. A receiver in another process (Electron main) or runtime (the Tauri
 * Rust plugin) has its own clock with a different origin, so that instant
 * means nothing there. What crosses the boundary is the remaining budget in
 * whole milliseconds, measured just before the request is sent; the receiver
 * admits it against its own clock at receipt and charges any queueing on its
 * side to the same budget.
 *
 * `budgetMs` is a non-negative safe integer, or absent when the caller gave no
 * deadline. An expired deadline is sent as `0`, never as a negative value, so
 * the receiver reports the expiry itself.
 */
export function relativeBudgetPayload(payload: SerializableRecord, boundary: string): SerializableRecord {
  const { deadline, ...withoutDeadline } = payload
  if (deadline === null || deadline === undefined) return withoutDeadline
  if (typeof deadline !== 'number' || !Number.isFinite(deadline)) {
    throw contractError('protocol.malformed', 'ipc', `${boundary}.deadline`)
  }
  if (globalThis.performance === undefined) {
    throw contractError('capability.unavailable', 'ipc', `${boundary}.monotonic-clock`)
  }
  return { ...withoutDeadline, budgetMs: Math.max(0, Math.floor(deadline - globalThis.performance.now())) }
}
