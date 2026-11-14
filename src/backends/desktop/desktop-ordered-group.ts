// src/backends/desktop/desktop-ordered-group.ts
import { contractError } from '../../backend-contract/errors'
import type { StreamLimits } from '../../backend-contract/streams'
import { isCanonicalIngressOrdinal } from '../../core/ordered-notification-contract'
import type { OrderedSourceGeneration } from '../../core/ordered-notification-contract'
import { OrderedNativeStream } from '../../core/ordered-native-stream'
import type { OrderedNativeTransport } from '../../core/ordered-native-stream'
import { decodeOrderedNativeFailure } from '../../core/ordered-native-wire'
import type { NormalizedBleError } from '../../backend-contract/errors'
import { desktopRustCoreError } from './desktop-rust-core-binding'
import type { DesktopRustCoreCentral } from './desktop-rust-core-binding'
export { decodeOrderedNativeItem as decodeDesktopOrderedItem } from '../../core/ordered-native-wire'

function malformed(): never {
  throw contractError('protocol.malformed', 'boundary', 'desktop.ordered.wire')
}
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
function decimal(value: unknown): string {
  if (!isCanonicalIngressOrdinal(value)) return malformed()
  return value
}
function parse(value: string): Record<string, unknown> {
  let result: unknown
  try {
    result = JSON.parse(value)
  } catch {
    return malformed()
  }
  if (!object(result)) return malformed()
  return result
}

export class DesktopOrderedStream extends OrderedNativeStream {
  constructor(
    limits: StreamLimits,
    central: DesktopRustCoreCentral,
    peerId: string,
    collectionId: string,
    connectionGeneration: string,
    databaseGeneration: string,
    members: readonly string[]
  ) {
    const transport: OrderedNativeTransport = {
      increaseByteCapacity: async (sourceGeneration, maxBytes) => {
        const fn = central.increaseOrderedByteCapacity
        if (central.orderedCapacityWireVersion?.call(central) !== 1 || fn === undefined)
          throw contractError('capability.unsupported', 'gatt', 'desktop.ordered.increase-capacity')
        const result = await fn.call(central, { wireVersion: 1, peerId, collectionId, sourceGeneration, maxBytes })
        if (
          !object(result) ||
          !Number.isSafeInteger(result.previousMaxBytes) ||
          !Number.isSafeInteger(result.maximumMaxBytes) ||
          result.previousMaxBytes < 1 ||
          result.previousMaxBytes > maxBytes ||
          result.maxBytes !== maxBytes ||
          result.maximumMaxBytes < maxBytes
        )
          return malformed()
        return result
      },
      pollOrderedGroup: options => {
        const fn = central.pollOrderedGroup
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      acknowledgeOrderedDelivery: options => {
        const fn = central.acknowledgeOrderedDelivery
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      discardOrderedDelivery: options => {
        const fn = central.discardOrderedDelivery
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      pollOrderedDiscardReceipt: options => {
        const fn = central.pollOrderedDiscardReceipt
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      acknowledgeOrderedDiscardReceipt: options => {
        const fn = central.acknowledgeOrderedDiscardReceipt
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      closeOrderedGroup: options => {
        const fn = central.closeOrderedGroup
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      acknowledgeOrderedGroup: options => {
        const fn = central.acknowledgeOrderedGroup
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      forgetOrderedGroup: options => {
        const fn = central.forgetOrderedGroup
        if (fn === undefined) return malformed()
        return fn.call(central, options)
      },
      status: async () => {
        const fn = central.orderedGroupStatus
        if (fn === undefined) return malformed()
        return parseOrderedStatus(await fn.call(central, { peerId, collectionId }), members)
      },
      normalizeError: desktopRustCoreError
    }
    super(limits, transport, peerId, collectionId, connectionGeneration, databaseGeneration, members, 2)
  }
}
interface OrderedMemberStatus {
  readonly memberId: string
  readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
  readonly effectiveDelivery: 'notification' | 'indication' | 'unknown'
  readonly cleanupError: NormalizedBleError | null
  readonly cleanupReleased: boolean
}
export function parseOrderedStatus(input: string, members: readonly string[]) {
  const status = parse(input)
  const failure = (value: unknown, operation: string): NormalizedBleError | null => {
    if (value === null) return null
    if (typeof value === 'string') return desktopRustCoreError(new Error(value), operation).normalized
    return decodeOrderedNativeFailure(value, 2)
  }
  if (
    !Array.isArray(status.members) ||
    status.members.length !== members.length ||
    typeof status.cleanupComplete !== 'boolean'
  )
    return malformed()
  const result = status.members.map((entry, index): OrderedMemberStatus => {
    if (!object(entry) || decimal(entry.memberId) !== String(index)) return malformed()
    if (
      entry.acquisition !== 'not-started' &&
      entry.acquisition !== 'acquired' &&
      entry.acquisition !== 'indeterminate'
    )
      return malformed()
    if (
      entry.effectiveDelivery !== 'notification' &&
      entry.effectiveDelivery !== 'indication' &&
      entry.effectiveDelivery !== 'unknown'
    )
      return malformed()
    if (typeof entry.cleanupReleased !== 'boolean') return malformed()
    const memberId = members[index]
    if (memberId === undefined) return malformed()
    return {
      memberId,
      acquisition: entry.acquisition,
      effectiveDelivery: entry.effectiveDelivery,
      cleanupError: failure(entry.cleanupError, 'desktop.ordered.member-cleanup'),
      cleanupReleased: entry.cleanupReleased
    }
  })
  let sourceGeneration: OrderedSourceGeneration | null = null
  if (status.sourceGeneration !== null) {
    if (
      !object(status.sourceGeneration) ||
      (status.sourceGeneration.kind !== 'counter' && status.sourceGeneration.kind !== 'opaque') ||
      typeof status.sourceGeneration.value !== 'string' ||
      status.sourceGeneration.value.length === 0
    )
      return malformed()
    if (status.sourceGeneration.kind === 'counter') decimal(status.sourceGeneration.value)
    sourceGeneration = { kind: status.sourceGeneration.kind, value: status.sourceGeneration.value }
  }
  return {
    members: result,
    sourceGeneration,
    cleanupComplete: status.cleanupComplete,
    setupError: failure(status.setupError, 'desktop.ordered.setup'),
    fenceError: failure(status.fenceError, 'desktop.ordered.fence')
  }
}
