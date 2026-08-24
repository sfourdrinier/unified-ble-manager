// src/public/error-bridge.ts — internal boundary between backend and application errors

import { BackendContractError, type CleanupFailure, type CleanupRecord } from '../backend-contract/errors'
import { BleError } from './errors'

export interface PublicCleanupRecord {
  readonly state: 'released' | 'release-failed'
  readonly failures: readonly unknown[]
}

export class BleCleanupError extends Error {
  readonly cleanup: PublicCleanupRecord

  constructor(cleanup: PublicCleanupRecord, message = 'BLE cleanup failed') {
    super(message)
    this.name = 'BleCleanupError'
    this.cleanup = cleanup
  }
}

/** Converts backend errors only at an application façade boundary. */
export function rehydratePublicError(error: unknown): unknown {
  if (error instanceof BleError || !(error instanceof BackendContractError)) {
    return error
  }
  const normalized = error.normalized
  return new BleError(normalized.code, normalized.domain, normalized.operation, {
    platform: normalized.platform
  })
}

export function rehydratePublicPromise<Value>(operation: Promise<Value>): Promise<Value> {
  return operation.catch(error => {
    throw rehydratePublicError(error)
  })
}

interface CleanupResultLike {
  readonly state: 'released' | 'release-failed'
  readonly failures: CleanupRecord['failures']
}

export function collectCleanupPhases(
  results: readonly { readonly error?: unknown; readonly cleanup?: CleanupResultLike }[]
): CleanupRecord {
  const thrown: unknown[] = []
  const cleanupFailures: CleanupFailure[] = []
  for (const result of results) {
    if (result.error !== undefined) thrown.push(result.error)
    if (result.cleanup?.state === 'release-failed') cleanupFailures.push(...result.cleanup.failures)
  }
  const cleanup: CleanupRecord =
    cleanupFailures.length === 0
      ? { state: 'released', failures: [] }
      : { state: 'release-failed', failures: cleanupFailures }
  if (thrown.length === 0) return cleanup
  const cleanupError = resolvedCleanupFailure(cleanup)
  throw new AggregateError(cleanupError === null ? thrown : [...thrown, cleanupError], 'BLE cleanup failed')
}

export async function runWithCleanup<Value>(
  operation: () => Promise<Value>,
  cleanup: () => Promise<CleanupResultLike | void>
): Promise<Value> {
  let outcome: { readonly kind: 'value'; readonly value: Value } | { readonly kind: 'error'; readonly error: unknown }
  try {
    outcome = { kind: 'value', value: await operation() }
  } catch (error) {
    outcome = { kind: 'error', error }
  }
  let cleanupOutcome: { readonly kind: 'ok' } | { readonly kind: 'error'; readonly error: unknown }
  try {
    const cleanupResult = await cleanup()
    const cleanupFailure = resolvedCleanupFailure(cleanupResult)
    cleanupOutcome = cleanupFailure === null ? { kind: 'ok' } : { kind: 'error', error: cleanupFailure }
  } catch (error) {
    cleanupOutcome = { kind: 'error', error }
  }
  if (cleanupOutcome.kind === 'error') {
    if (outcome.kind === 'error') {
      throw new AggregateError([outcome.error, cleanupOutcome.error], 'BLE operation and cleanup both failed')
    }
    throw cleanupOutcome.error
  }
  if (outcome.kind === 'error') {
    throw outcome.error
  }
  return outcome.value
}

function resolvedCleanupFailure(value: CleanupResultLike | void): Error | null {
  if (value === undefined || value.state !== 'release-failed') {
    return null
  }
  return new BleCleanupError({
    state: 'release-failed',
    failures: value.failures
  })
}
