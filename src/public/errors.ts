// src/public/errors.ts — public BleError with PR2 recovery catalog

import { BLE_ERROR_CODES, BLE_ERROR_DOMAINS } from '../backend-contract/errors'
import type { BleErrorCode, BleErrorDomain } from '../backend-contract/errors'
import type { Limitation } from '../backend-contract/capabilities'
import { recoveryForCode } from '../backend-contract/recovery'
import type { BleRecovery } from '../backend-contract/recovery'
import { toPublicPlatformErrorDetail, type PublicPlatformErrorDetail } from './cleanup'

/**
 * Public application error. All façade and IPC errors rehydrate to BleError.
 * Recovery catalog is deterministic per code; platform detail is preserved separately.
 */
export class BleError extends Error {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PublicPlatformErrorDetail | null
  readonly limitations: readonly Limitation[]
  readonly recovery: BleRecovery

  constructor(
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: string,
    options: {
      readonly platform?: PublicPlatformErrorDetail | null
      readonly limitations?: readonly Limitation[]
    } = {}
  ) {
    if (!BLE_ERROR_CODES.some(candidate => candidate === code)) {
      throw new TypeError(`unknown BleError code: ${String(code)}`)
    }
    if (!BLE_ERROR_DOMAINS.some(candidate => candidate === domain)) {
      throw new TypeError(`unknown BleError domain: ${String(domain)}`)
    }
    if (typeof operation !== 'string' || operation.length === 0) {
      throw new TypeError('operation must be non-empty')
    }
    const recovery = recoveryForCode(code, operation)
    const platform = toPublicPlatformErrorDetail(options.platform ?? null)
    super(`${code}: ${operation}`)
    this.name = 'BleError'
    this.code = code
    this.domain = domain
    this.operation = operation
    this.platform = platform
    this.limitations = Object.freeze((options.limitations ?? []).map(limitation => Object.freeze({ ...limitation })))
    this.recovery = Object.freeze({
      disposition: recovery.disposition,
      actions: Object.freeze(recovery.actions.map(action => Object.freeze(action)))
    })
  }
}

export type { BleErrorCode, BleErrorDomain } from '../backend-contract/errors'
export type { BleRecovery, BleRecoveryDisposition, RecoveryAction } from '../backend-contract/recovery'
