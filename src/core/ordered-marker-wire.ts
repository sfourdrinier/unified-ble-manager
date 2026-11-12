// src/core/ordered-marker-wire.ts
import { assertPlatformFailureEnvelopeBound, contractError, type NormalizedBleError } from '../backend-contract/errors'
import { isCanonicalIngressOrdinal } from './ordered-notification-contract'
import { decodeOrderedNativeFailure } from './ordered-native-wire'

export type OrderedMarkerWritePhase =
  | { readonly state: 'initiating' }
  | { readonly state: 'not-started' | 'started' | 'settled'; readonly error: NormalizedBleError | null }
export type OrderedMarkerWriteStatus =
  | { readonly state: 'pending' }
  | { readonly state: 'not-admitted'; readonly error: NormalizedBleError | null }
  | { readonly state: 'rejected'; readonly error: NormalizedBleError }
  | { readonly state: 'admitted'; readonly ordinal: string; readonly write: OrderedMarkerWritePhase }
function bad(): never {
  throw contractError('protocol.malformed', 'boundary', 'ordered.marker.wire')
}
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return bad()
  return Object.fromEntries(Object.entries(value))
}
function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).length !== keys.length || keys.some(key => !Object.hasOwn(value, key))) bad()
}
function error(input: unknown, metadataVersion: 1 | 2): NormalizedBleError {
  return decodeOrderedNativeFailure(input, metadataVersion)
}
export function decodeOrderedMarkerWriteStatus(input: unknown, metadataVersion: 1 | 2 = 1): OrderedMarkerWriteStatus {
  if (metadataVersion !== 1 && metadataVersion !== 2) return bad()
  if (metadataVersion === 2) assertPlatformFailureEnvelopeBound(input, 'ordered.marker.wire')
  const value = object(input)
  if (value.state === 'pending') {
    exact(value, ['state'])
    return Object.freeze({ state: 'pending' })
  }
  if (value.state === 'not-admitted' || value.state === 'rejected') {
    exact(value, ['state', 'error'])
    const cause = value.error === null ? null : error(value.error, metadataVersion)
    if (value.state === 'rejected') {
      if (cause === null) return bad()
      return Object.freeze({ state: 'rejected', error: cause })
    }
    return Object.freeze({ state: 'not-admitted', error: cause })
  }
  if (value.state !== 'admitted') return bad()
  exact(value, ['state', 'ordinal', 'write'])
  if (!isCanonicalIngressOrdinal(value.ordinal)) return bad()
  const write = object(value.write)
  if (write.state === 'initiating') {
    exact(write, ['state'])
    return Object.freeze({ state: 'admitted', ordinal: value.ordinal, write: Object.freeze({ state: 'initiating' }) })
  }
  if (write.state !== 'not-started' && write.state !== 'started' && write.state !== 'settled') return bad()
  exact(write, ['state', 'error'])
  return Object.freeze({
    state: 'admitted',
    ordinal: value.ordinal,
    write: Object.freeze({
      state: write.state,
      error: write.error === null ? null : error(write.error, metadataVersion)
    })
  })
}

/** Retains one exact marker/write transaction until its evidence is consumed and forgotten. */
export interface OrderedMarkerWriteHandle<Target> {
  start(
    target: Target,
    value: Readonly<Uint8Array>,
    mode: 'with-response' | 'without-response',
    options: import('../backend-contract/operations').PublicOperationOptions
  ): Promise<OrderedMarkerWriteStatus>
  status(): Promise<OrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }>
  cancel(): Promise<OrderedMarkerWriteStatus | { readonly state: 'idle' | 'retired' }>
  forget(): Promise<void>
}
