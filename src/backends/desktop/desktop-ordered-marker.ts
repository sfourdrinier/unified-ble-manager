// src/backends/desktop/desktop-ordered-marker.ts
import { contractError } from '../../backend-contract/errors'
import { createOrderedMarkerReservations, type OrderedMarkerTransport } from '../../core/ordered-marker-handle'
import { decodeOrderedMarkerWriteStatus } from '../../core/ordered-marker-wire'
import {
  desktopRustCoreError,
  type DesktopRustCoreCentral,
  type DesktopRustCoreSelector
} from './desktop-rust-core-binding'

type Group = { readonly wireVersion: number; readonly peerId: string; readonly collectionId: string }
export function desktopMarkerReservations<Target>(
  central: Pick<
    DesktopRustCoreCentral,
    | 'reserveOrderedMarkerWrite'
    | 'startOrderedMarkerWrite'
    | 'orderedMarkerWriteStatus'
    | 'cancelOrderedMarkerWrite'
    | 'forgetOrderedMarkerWrite'
  >,
  group: Group,
  resolve: (target: Target) => DesktopRustCoreSelector,
  now: () => number,
  newKey: () => string
) {
  const decode = (input: string) => {
    let value: unknown
    try {
      value = JSON.parse(input)
    } catch {
      throw contractError('protocol.malformed', 'boundary', 'desktop.ordered-marker')
    }
    return decodeOrderedMarkerWriteStatus(value, 2)
  }
  const call = async <T>(invoke: () => Promise<T>) => {
    try {
      return await invoke()
    } catch (error) {
      throw desktopRustCoreError(error, 'desktop.ordered-marker')
    }
  }
  const transport: OrderedMarkerTransport<DesktopRustCoreSelector> = {
    reserve: key => {
      const invoke = central.reserveOrderedMarkerWrite
      if (!invoke) throw contractError('capability.unsupported', 'gatt', 'desktop.ordered-marker')
      return call(() => invoke.call(central, { ...group, reservationKey: key }))
    },
    start: async (id, selector, value, withResponse) => {
      const invoke = central.startOrderedMarkerWrite
      if (!invoke) throw contractError('capability.unsupported', 'gatt', 'desktop.ordered-marker')
      return decode(
        await call(() => invoke.call(central, { ...group, reservationId: id, selector, value, withResponse }))
      )
    },
    status: async id => {
      const invoke = central.orderedMarkerWriteStatus
      if (!invoke) throw contractError('capability.unsupported', 'gatt', 'desktop.ordered-marker')
      return decode(await call(() => invoke.call(central, { ...group, reservationId: id })))
    },
    cancel: async id => {
      const invoke = central.cancelOrderedMarkerWrite
      if (!invoke) throw contractError('capability.unsupported', 'gatt', 'desktop.ordered-marker')
      return decode(await call(() => invoke.call(central, { ...group, reservationId: id })))
    },
    forget: id => {
      const invoke = central.forgetOrderedMarkerWrite
      if (!invoke) throw contractError('capability.unsupported', 'gatt', 'desktop.ordered-marker')
      return call(() => invoke.call(central, { ...group, reservationId: id }))
    }
  }
  return createOrderedMarkerReservations(transport, resolve, now, newKey)
}
