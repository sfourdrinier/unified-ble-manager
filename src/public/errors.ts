// src/public/errors.ts — public BleError with PR2 recovery catalog

import {
  BLE_COMMIT_UNCERTAINTIES,
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  BLE_RETRYABILITIES,
  retryabilityForCode
} from '../backend-contract/errors'
import type { BleCommitUncertainty, BleErrorCode, BleErrorDomain, BleRetryability } from '../backend-contract/errors'
import type { Limitation } from '../backend-contract/capabilities'
import { recoveryForError } from '../backend-contract/recovery'
import type { BleRecovery } from '../backend-contract/recovery'
import { toPublicPlatformErrorDetail, type PublicPlatformErrorDetail } from './cleanup'

/**
 * Public application error. All façade and IPC errors rehydrate to BleError.
 * Recovery follows the code and the operation's reported retryability; platform
 * detail is preserved separately.
 */
export class BleError extends Error {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly platform: PublicPlatformErrorDetail | null
  readonly limitations: readonly Limitation[]
  /**
   * The operation's own answer about repeating it. `never` for an aborted or
   * timed-out operation means it was dispatched and may already have
   * committed at the peripheral (for example a write): do not repeat it, verify
   * the peer's state instead. Defaults to the code's retryability when the
   * failure reported none.
   */
  readonly retryability: BleRetryability
  /**
   * The commit state the operation's owner reported: `uncertain` means it was
   * dispatched and may already have committed at the peripheral (read the
   * peer's state back, never replay it); `not-dispatched` means nothing reached
   * the radio. `null` when the owner did not know or did not say.
   */
  readonly commit: BleCommitUncertainty | null
  readonly recovery: BleRecovery

  constructor(
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: string,
    options: {
      readonly platform?: PublicPlatformErrorDetail | null
      readonly limitations?: readonly Limitation[]
      readonly retryability?: BleRetryability
      readonly commit?: BleCommitUncertainty | null
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
    const retryability = options.retryability ?? retryabilityForCode(code)
    if (!BLE_RETRYABILITIES.some(candidate => candidate === retryability)) {
      throw new TypeError(`unknown BleError retryability: ${String(retryability)}`)
    }
    const commit = options.commit ?? null
    if (commit !== null && !BLE_COMMIT_UNCERTAINTIES.some(candidate => candidate === commit)) {
      throw new TypeError(`unknown BleError commit: ${String(commit)}`)
    }
    const recovery = recoveryForError({ code, operation, retryability, commit })
    const platform = toPublicPlatformErrorDetail(options.platform ?? null)
    super(`${code}: ${operation}`)
    this.name = 'BleError'
    this.code = code
    this.domain = domain
    this.operation = operation
    this.platform = platform
    this.retryability = retryability
    this.commit = commit
    this.limitations = Object.freeze((options.limitations ?? []).map(limitation => Object.freeze({ ...limitation })))
    this.recovery = Object.freeze({
      disposition: recovery.disposition,
      actions: Object.freeze(recovery.actions.map(action => Object.freeze(action)))
    })
  }
}

export type { BleCommitUncertainty, BleErrorCode, BleErrorDomain, BleRetryability } from '../backend-contract/errors'
export type { BleRecovery, BleRecoveryDisposition, RecoveryAction } from '../backend-contract/recovery'
