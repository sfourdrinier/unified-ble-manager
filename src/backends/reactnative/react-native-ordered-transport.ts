// src/backends/reactnative/react-native-ordered-transport.ts
// Session-owned ordered operations; the reclaim capability never crosses diagnostics.
import { BackendContractError, contractError } from '../../backend-contract/errors'
import { remoteFailureError } from './rust-core-wire'
import type { WireJsonObject, WireOp, WireOpResults } from './rust-core-wire'
import type { OrderedNativeTransport } from '../../core/ordered-native-stream'

export type OrderedSessionInvoke = <Op extends WireOp>(op: Op, args: WireJsonObject) => Promise<WireOpResults[Op]>

export function createReactNativeOrderedTransport(
  invoke: OrderedSessionInvoke,
  peerId: string,
  collectionId: string,
  reclaimCapability: string,
  memberIds: readonly string[],
  operationId: () => string
): OrderedNativeTransport & { inventoryEmpty(): boolean } {
  const key = { peerId, collectionId }
  let emptyInventory = false
  const normalizeError = (error: unknown, operation: string) =>
    error instanceof BackendContractError ? error : contractError('platform.failure', 'boundary', operation)
  return {
    inventoryEmpty: () => emptyInventory,
    increaseByteCapacity: async (sourceGeneration, maxBytes) => {
      const version = await invoke('gatt.ordered.capacity.version', {}).catch((error: unknown) => {
        if (
          error instanceof BackendContractError &&
          error.normalized.code === 'argument.invalid' &&
          error.normalized.domain === 'core' &&
          error.normalized.operation === 'ubm-mobile.wire.op'
        )
          throw contractError('capability.unsupported', 'gatt', 'react-native.ordered.increase-capacity')
        throw error
      })
      if (version.wireVersion !== 1)
        throw contractError('capability.unsupported', 'gatt', 'react-native.ordered.increase-capacity')
      if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > version.maximumMaxBytes)
        throw contractError('argument.invalid', 'gatt', 'react-native.ordered.increase-capacity')
      const result = await invoke('gatt.ordered.increase-capacity', {
        ...key,
        sourceGeneration,
        maxBytes,
        wireVersion: 1
      })
      if (result.maxBytes !== maxBytes || result.maximumMaxBytes !== version.maximumMaxBytes)
        throw contractError('protocol.malformed', 'boundary', 'react-native.ordered.increase-capacity')
      return result
    },
    pollOrderedGroup: () => invoke('gatt.ordered.poll', key),
    acknowledgeOrderedDelivery: async ({ deliveryId }) => {
      await invoke('gatt.ordered.ack-delivery', { ...key, deliveryId })
    },
    status: async () => {
      const result = await invoke('gatt.ordered.status', key)
      if (result.members.length !== 0 && result.members.length !== memberIds.length)
        throw contractError('protocol.malformed', 'boundary', 'react-native.ordered.members')
      // A reserved group has no native member inventory until Open is admitted.
      // Before cleanup completes, avoid asserting that an in-flight Open had no effect.
      const unopened = result.members.length === 0
      emptyInventory = unopened
      return {
        ...result,
        setupError: result.setupError === null ? null : remoteFailureError(result.setupError).normalized,
        fenceError: result.fenceError === null ? null : remoteFailureError(result.fenceError).normalized,
        members: unopened
          ? memberIds.map(memberId => ({
              memberId,
              acquisition: result.cleanupComplete ? ('not-started' as const) : ('indeterminate' as const),
              effectiveDelivery: 'unknown' as const,
              cleanupReleased: result.cleanupComplete,
              cleanupError: null
            }))
          : result.members.map((member, index) => {
              const publicId = memberIds[index]
              if (publicId === undefined || member.memberId !== String(index))
                throw contractError('protocol.malformed', 'boundary', 'react-native.ordered.member')
              return {
                ...member,
                memberId: publicId,
                cleanupError: member.cleanupError === null ? null : remoteFailureError(member.cleanupError).normalized
              }
            })
      }
    },
    closeOrderedGroup: () => invoke('gatt.ordered.close', { ...key, operationId: operationId() }),
    acknowledgeOrderedGroup: async () => {
      await invoke('gatt.ordered.ack-group', key)
    },
    forgetOrderedGroup: async () => {
      await invoke('gatt.ordered.forget-group', { ...key, reclaimCapability })
    },
    discardOrderedDelivery: ({ deliveryId }) => invoke('gatt.ordered.discard', { ...key, deliveryId }),
    pollOrderedDiscardReceipt: () => invoke('gatt.ordered.poll-discard', key),
    acknowledgeOrderedDiscardReceipt: async ({ deliveryId }) => {
      await invoke('gatt.ordered.ack-discard', { ...key, deliveryId })
    },
    normalizeError
  }
}
