/* eslint-disable @typescript-eslint/no-explicit-any */
// src/public/errors.ts — public BleError (PR1 stub, full recovery catalog in PR2)

import { BackendContractError } from '../backend-contract/errors'
import type { BleErrorCode, BleErrorDomain } from '../backend-contract/errors'

export type BleRecoveryDisposition =
  | 'none'
  | 'retry-immediately'
  | 'retry-with-backoff'
  | 'after-state-change'
  | 'after-user-action'
  | 'caller-policy'

export interface BleRecovery {
  readonly disposition: BleRecoveryDisposition
  readonly actions: readonly string[]
}

/**
 * Public application error. All façade and IPC errors rehydrate to BleError.
 * Full recovery catalog (permission, settings, reconnect, etc.) lands in PR2.
 */
export class BleError extends BackendContractError {
  readonly recovery: BleRecovery

  constructor(
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: string,
    recovery: BleRecovery = { disposition: 'none', actions: [] }
  ) {
    super({ code, domain, operation, platform: null, retryability: 'never' })
    this.name = 'BleError'
    this.recovery = Object.freeze(recovery)
  }

  static fromContractError(error: BackendContractError): BleError {
    const ble = new BleError(error.normalized.code, error.normalized.domain, error.normalized.operation)
    ;(ble as any).normalized = error.normalized
    return ble
  }
}

export type { BleErrorCode, BleErrorDomain } from '../backend-contract/errors'
