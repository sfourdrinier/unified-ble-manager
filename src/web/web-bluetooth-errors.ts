// src/web/web-bluetooth-errors.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import { assertScanFilter } from '../backend-contract/advertisement'
import type { BleErrorCode, BleErrorDomain, CleanupRecord } from '../backend-contract/errors'
import type { ChooserRequest } from '../backend-contract/host/web'
import type { WebLinkEnd } from './web-bluetooth-handles'

export interface WebErrorContext {
  readonly fallbackCode: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
}

export function normalizeWebBluetoothError(error: Error, context: WebErrorContext): BackendContractError {
  if (error instanceof BackendContractError) {
    return error
  }
  const namedCode = normalizedNamedErrorCode(error.name, context)
  const domain = namedCode === 'connection.lost' ? 'connection' : context.domain
  const normalized = contractError(namedCode, domain, context.operation, {
    domain: 'web-bluetooth',
    code: error.name.length === 0 ? 'Error' : error.name,
    safeMessage: 'The Web Bluetooth operation failed.',
    metadata: { browserErrorName: error.name.length === 0 ? 'Error' : error.name }
  })
  return isTransientEstablishmentFailure(error.name, context)
    ? new BackendContractError({ ...normalized.normalized, retryability: 'caller-decides' })
    : normalized
}

/**
 * `BluetoothRemoteGATTServer.connect()` rejects with a NetworkError when the
 * browser could not establish the link — the Web answer to Android GATT 133
 * or `CBError.connectionFailed`. Nothing was committed, so repeating it is
 * the caller's policy (owner decision, 5.0); the backend never retries it.
 */
function isTransientEstablishmentFailure(name: string, context: WebErrorContext): boolean {
  return name === 'NetworkError' && context.operation === WEB_CONNECT_OPERATION
}

export const WEB_CONNECT_OPERATION = 'web-connection.connect'

/**
 * Finding 161: a dispatched connect whose deadline expires before any link
 * came up is the peer not answering — `connection.failed`
 * (`caller-decides`) on every backend, the same physical event as Android
 * GATT 133/147, which the browser never reports on its own. The deadline
 * fact rides in `platform`. Every other operation keeps
 * `operation.timed-out`, and a caller-supplied AbortSignal abort stays
 * `operation.aborted`.
 */
export function webConnectDeadlineError(deadlineMs: number): BackendContractError {
  const safeMessage = `The ${deadlineMs} ms connect deadline expired before any link came up.`
  const normalized = contractError('connection.failed', 'connection', WEB_CONNECT_OPERATION, {
    domain: 'web-bluetooth',
    code: 'DeadlineExpired',
    safeMessage,
    metadata: { deadlineMs }
  })
  return new BackendContractError({ ...normalized.normalized, retryability: 'caller-decides' })
}

/**
 * The error an operation in flight reports when its link ends, one word per
 * event on every host (5.0): the browser reported the link gone
 * (`connection.lost`), the app released it (`operation.disconnected`), or
 * Bluetooth became unavailable (`operation.reset`).
 */
export const WEB_LINK_END_CODES = Object.freeze({
  'connection-lost': 'connection.lost',
  'owner-released': 'operation.disconnected',
  'adapter-loss': 'operation.reset'
} as const satisfies Record<WebLinkEnd, BleErrorCode>)

function normalizedNamedErrorCode(name: string, context: WebErrorContext): BleErrorCode {
  if (name === 'AbortError') {
    return context.domain === 'chooser' ? 'chooser.cancelled' : 'operation.aborted'
  }
  if (name === 'NotFoundError') {
    return context.domain === 'chooser' ? 'chooser.cancelled' : 'gatt.not-found'
  }
  if (name === 'NotAllowedError') {
    return context.domain === 'chooser' ? 'chooser.cancelled' : 'permission.denied'
  }
  if (name === 'SecurityError') {
    return context.domain === 'chooser' ? 'permission.denied' : 'platform.security'
  }
  if (name === 'NetworkError') {
    // A connect the browser could not establish; any other call the browser
    // failed because the GATT server is disconnected is a link loss (5.0,
    // the word every host uses).
    return context.domain === 'connection' ? 'connection.failed' : 'connection.lost'
  }
  if (name === 'NotSupportedError') {
    return 'gatt.property-not-supported'
  }
  if (name === 'InvalidStateError') {
    return 'lifecycle.invalid-state'
  }
  return context.fallbackCode
}

export function webCleanupFailure(resourceKind: string, operation: string): CleanupRecord {
  return {
    state: 'release-failed',
    failures: [{ resourceKind, error: contractError('platform.failure', 'cleanup', operation).normalized }]
  }
}

export function validateWebChooserRequest(request: ChooserRequest): void {
  const hasFilters = request.filters.length > 0
  if (request.acceptAllDevices === hasFilters) {
    throw contractError('scan.filter-invalid', 'chooser', 'web-chooser.request')
  }
  for (const filter of request.filters) {
    assertScanFilter(filter, 'web-chooser.request')
    if (filter.serviceUuids.length === 0 && filter.manufacturerData.length === 0 && filter.localNamePrefix === null) {
      throw contractError('scan.filter-invalid', 'chooser', 'web-chooser.request')
    }
  }
}
