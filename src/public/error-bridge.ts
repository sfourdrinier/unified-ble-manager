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
