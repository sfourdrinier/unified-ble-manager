// src/backends/corebluetooth/corebluetooth-read-notify-provenance.ts

import { BackendContractError, contractError, type BleErrorCode } from '../../backend-contract/errors'

export const COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE = 1031
export const COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE = 413
export const COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE = 1011
export const COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE = 414
export const COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE = 1032
export const COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE = 415

export const COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE = 'gatt.read-failed' as const
export const COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE = 'gatt.subscribe-failed' as const

export type ReadNotifyValueUpdateRoute = 'completePendingRead' | 'rejectPendingRead' | 'deliverNotification' | 'ignore'

export type SubscribeAdmission = 'admit' | 'rejectPendingRead' | 'rejectPendingNotify'

export function independentReadIsAmbiguous(input: {
  isNotifying: boolean
  hasInstalledSubscription: boolean
  pendingNotifyEnable: boolean
  pendingCancellationCleanup: boolean
}): boolean {
  return (
    input.isNotifying || input.hasInstalledSubscription || input.pendingNotifyEnable || input.pendingCancellationCleanup
  )
}

export function admitSubscribe(input: { hasPendingRead: boolean; hasPendingNotify: boolean }): SubscribeAdmission {
  if (input.hasPendingRead) return 'rejectPendingRead'
  if (input.hasPendingNotify) return 'rejectPendingNotify'
  return 'admit'
}

export function routeValueUpdate(input: {
  hasPendingRead: boolean
  isNotifying: boolean
  hasInstalledSubscription: boolean
  pendingNotifyEnable: boolean
  pendingCancellationCleanup: boolean
  hasError: boolean
  hasValue: boolean
}): ReadNotifyValueUpdateRoute {
  if (
    input.hasPendingRead &&
    !input.isNotifying &&
    !input.hasInstalledSubscription &&
    !input.pendingCancellationCleanup
  ) {
    return 'completePendingRead'
  }
  if (input.hasPendingRead) {
    return 'rejectPendingRead'
  }
  if (
    !input.hasError &&
    input.hasValue &&
    (input.hasInstalledSubscription || input.pendingNotifyEnable) &&
    !input.pendingCancellationCleanup
  ) {
    return 'deliverNotification'
  }
  return 'ignore'
}

export function occurrenceValueUpdateShouldReturn(input: {
  occurrenceAmbiguous: boolean
  occurrenceStatePresent: boolean
}): boolean {
  return input.occurrenceAmbiguous || input.occurrenceStatePresent
}

export function bleErrorCodeForCoreBluetoothNativeCode(nativeCode: string): BleErrorCode | null {
  if (
    nativeCode === String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE) ||
    nativeCode === String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE) ||
    nativeCode === String(COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE) ||
    nativeCode === String(COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE)
  ) {
    return COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE
  }
  if (
    nativeCode === String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE) ||
    nativeCode === String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE)
  ) {
    return COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE
  }
  return null
}

export function mapCoreBluetoothNativeFailure(
  error: unknown,
  operation: string,
  fallback: typeof COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE | typeof COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE
): Error {
  if (error instanceof BackendContractError) return error
  if (error instanceof Error && 'normalized' in error) return error
  const nativeCode = nativeCodeFromUnknown(error)
  const mapped =
    nativeCode === null
      ? bleErrorCodeForCoreBluetoothMessage(error)
      : bleErrorCodeForCoreBluetoothNativeCode(nativeCode)
  const safeMessage = error instanceof Error ? error.message : 'CoreBluetooth boundary rejected with a non-Error value'
  return contractError(mapped ?? fallback, 'gatt', operation, {
    domain: 'corebluetooth',
    code: nativeCode ?? 'native-error',
    safeMessage,
    metadata: Object.freeze({})
  })
}

function nativeCodeFromUnknown(error: unknown): string | null {
  if (typeof error !== 'object' || error === null || !('code' in error)) return null
  const code = error.code
  if (typeof code === 'number' && Number.isSafeInteger(code)) return String(code)
  if (typeof code === 'string' && /^\d+$/.test(code)) return code
  return null
}

function bleErrorCodeForCoreBluetoothMessage(error: unknown): BleErrorCode | null {
  const message = error instanceof Error ? error.message : ''
  if (message.includes('Independent read is ambiguous while this characteristic is notifying')) {
    return COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE
  }
  if (message.includes('A read is already pending for this characteristic')) {
    return COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE
  }
  if (message.includes('A notification state change cannot start while a read is pending')) {
    return COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE
  }
  return null
}
