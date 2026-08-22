// src/public/error-bridge.ts — internal boundary between backend and application errors

import { BackendContractError } from '../backend-contract/errors'
import { BleError } from './errors'

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
  readonly failures: readonly object[]
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
  const error = new Error('BLE cleanup failed')
  Object.defineProperty(error, 'cleanup', { value, enumerable: true })
  return error
}
