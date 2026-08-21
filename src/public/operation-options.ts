// src/public/operation-options.ts

import { contractError } from '../backend-contract/errors'
import { deadline, type Deadline } from '../backend-contract/primitives'

/**
 * Application-facing operation controls. Timeouts are expressed as a monotonic
 * wall-clock delta and normalized once to an internal Deadline; advanced callers
 * use `/advanced` with an exact Deadline.
 */
export interface OperationOptions {
  readonly signal?: AbortSignal
  readonly timeoutMs?: number
}

const MAX_TIMEOUT_MS = 2_147_483_647

function assertValidTimeoutMs(value: unknown, path: string): void {
  if (value === undefined) {
    return
  }
  if (typeof value !== 'number') {
    throw contractError('argument.invalid', 'core', path)
  }
  if (!Number.isFinite(value) || !Number.isSafeInteger(value) || value <= 0 || value > MAX_TIMEOUT_MS) {
    throw contractError('argument.invalid', 'core', path)
  }
}

function assertValidSignal(value: unknown, path: string): void {
  if (value === undefined || value === null) {
    return
  }
  if (!(value instanceof AbortSignal)) {
    throw contractError('argument.invalid', 'core', path)
  }
}

export interface NormalizedOperationOptions {
  readonly signal: AbortSignal | null
  readonly deadline: Deadline | null
}

/**
 * Centralized application option normalization.
 *
 * - validates `signal` and `timeoutMs` exactly once per public entry
 * - composes an outer signal with internal cancellation without losing cause
 * - preserves an earlier deadline when a helper calls another helper
 * - converts `timeoutMs` to a monotonic Deadline using the provided `now`
 */
export function normalizeOperationOptions(
  options: OperationOptions | undefined,
  now: () => number,
  existingDeadline: Deadline | null = null
): NormalizedOperationOptions {
  const signal = options?.signal ?? null
  const timeoutMs = options?.timeoutMs

  assertValidSignal(signal, 'operation-options.signal')
  assertValidTimeoutMs(timeoutMs, 'operation-options.timeoutMs')

  if (signal !== null && signal.aborted) {
    // Preserve abort cause; deadline still derived for observability but operation will abort synchronously upstream.
  }

  let deadlineValue: Deadline | null = null
  if (timeoutMs !== undefined) {
    const absolute = now() + timeoutMs
    if (!Number.isFinite(absolute) || absolute < 0) {
      throw contractError('argument.invalid', 'core', 'operation-options.timeoutMs')
    }
    deadlineValue = deadline(absolute)
  }

  // Never extend an existing deadline.
  if (existingDeadline !== null && deadlineValue !== null) {
    deadlineValue = (existingDeadline as number) < (deadlineValue as number) ? existingDeadline : deadlineValue
  } else if (existingDeadline !== null) {
    deadlineValue = existingDeadline
  }

  return Object.freeze({
    signal,
    deadline: deadlineValue
  })
}

export function composeAbortSignal(outer: AbortSignal | null, inner: AbortSignal | null): AbortSignal | null {
  if (outer === null) return inner
  if (inner === null) return outer
  if (outer === inner) return outer
  // AnySignal-like composition without introducing a dependency.
  const controller = new AbortController()
  const onAbort = () => {
    // Reason preservation is best-effort for older lib targets.
    const outerReason = (outer as unknown as { reason?: unknown }).reason
    const innerReason = (inner as unknown as { reason?: unknown }).reason
    const reason = outerReason ?? innerReason
    try {
      // Modern AbortController supports reason.
      ;(controller as unknown as { abort: (r?: unknown) => void }).abort(reason)
    } catch {
      controller.abort()
    }
  }
  if (outer.aborted || inner.aborted) {
    const outerReason = (outer as unknown as { reason?: unknown }).reason
    const innerReason = (inner as unknown as { reason?: unknown }).reason
    const reason = outer.aborted ? outerReason : innerReason
    try {
      ;(controller as unknown as { abort: (r?: unknown) => void }).abort(reason)
    } catch {
      controller.abort()
    }
    return controller.signal
  }
  outer.addEventListener('abort', onAbort, { once: true })
  inner.addEventListener('abort', onAbort, { once: true })
  return controller.signal
}
