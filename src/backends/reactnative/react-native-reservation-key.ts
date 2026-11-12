import { contractError } from '../../backend-contract/errors'

const incarnationKey = Symbol.for('unified-ble-manager.react-native.reservation-incarnation.v2')
const flightKey = Symbol.for('unified-ble-manager.react-native.reservation-incarnation-flight.v2')
const sequenceKey = Symbol.for('unified-ble-manager.react-native.reservation-sequence.v2')

function incarnation(bytes: Uint8Array): string {
  if (!(bytes instanceof Uint8Array) || bytes.length !== 16)
    throw contractError('protocol.malformed', 'boundary', 'react-native.reservations-entropy')
  return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('')
}

/** Module copies in one runtime share the native-generated incarnation. */
export async function initializeReactNativeReservationKeys(
  randomBytes: (length: number) => Promise<Uint8Array>
): Promise<void> {
  const existing: unknown = Reflect.get(globalThis, incarnationKey)
  if (typeof existing === 'string' && /^[0-9a-f]{32}$/.test(existing)) return
  if (existing !== undefined)
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  const previous: unknown = Reflect.get(globalThis, flightKey)
  if (previous instanceof Promise) return previous
  if (previous !== undefined)
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  const flight = Promise.resolve().then(async () => {
    const prefix = incarnation(await randomBytes(16))
    if (!Reflect.set(globalThis, incarnationKey, prefix))
      throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  })
  if (!Reflect.set(globalThis, flightKey, flight))
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  try {
    await flight
  } finally {
    Reflect.deleteProperty(globalThis, flightKey)
  }
}

export function nextReactNativeReservationKey(): string {
  const prefix: unknown = Reflect.get(globalThis, incarnationKey)
  if (typeof prefix !== 'string' || !/^[0-9a-f]{32}$/.test(prefix))
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  const value: unknown = Reflect.get(globalThis, sequenceKey)
  const sequence = value === undefined ? 0 : value
  if (
    typeof sequence !== 'number' ||
    !Number.isSafeInteger(sequence) ||
    sequence < 0 ||
    sequence >= Number.MAX_SAFE_INTEGER
  )
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-exhausted')
  if (!Reflect.set(globalThis, sequenceKey, sequence + 1))
    throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.reservations-unavailable')
  return `${prefix}:${sequence}`
}
