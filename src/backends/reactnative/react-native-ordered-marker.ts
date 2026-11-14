// src/backends/reactnative/react-native-ordered-marker.ts
import { createOrderedMarkerReservations } from '../../core/ordered-marker-handle'
import { encodeBase64 } from './rust-core-wire'
import type { OrderedSessionInvoke } from './react-native-ordered-transport'
import type { WireJsonObject } from './rust-core-wire'

/** The session authenticates the exact group on every retained operation. */
export function createReactNativeOrderedMarkerReservations<Target>(
  invoke: OrderedSessionInvoke,
  peerId: string,
  collectionId: string,
  resolve: (target: Target) => WireJsonObject,
  now: () => number,
  newKey: () => string
) {
  const group = { peerId, collectionId }
  return createOrderedMarkerReservations<Target, WireJsonObject>(
    {
      reserve: async reservationId => (await invoke('gatt.ordered.marker.reserve', { ...group, reservationId })).handle,
      start: async (handle, selector, value, withResponse) => {
        const encoded = encodeBase64(value)
        if (!encoded.ok) throw encoded.error
        return invoke('gatt.ordered.marker.start', {
          ...group,
          handle,
          selector,
          valueB64: encoded.value,
          mode: withResponse ? 'with-response' : 'without-response'
        })
      },
      status: handle => invoke('gatt.ordered.marker.status', { ...group, handle }),
      cancel: handle => invoke('gatt.ordered.marker.cancel', { ...group, handle }),
      forget: async handle => {
        await invoke('gatt.ordered.marker.forget', { ...group, handle })
      }
    },
    resolve,
    now,
    newKey
  )
}
