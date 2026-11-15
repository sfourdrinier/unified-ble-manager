// src/backends/reactnative/react-native-ordered-recording-reclaim.ts
import { createOrderedRecordingReclaimReservations } from '../../core/ordered-recording-reclaim'
import { contractError } from '../../backend-contract/errors'
import { remoteFailureError } from './rust-core-wire'
import type { OrderedSessionInvoke } from './react-native-ordered-transport'
import type { WireRecordingReclaimStatus } from './rust-core-wire'
import type { ReactNativeOrderedRecordingRecoveryHandle } from './react-native-ordered-recovery'

/** The capability remains in this closure and is authenticated on every phase. */
export function createReactNativeRecordingReclaims(
  invoke: OrderedSessionInvoke,
  handoff: ReactNativeOrderedRecordingRecoveryHandle,
  newKey: () => string,
  onReleased: () => Promise<void>,
  commitWithFence: <T>(commit: () => Promise<T>) => Promise<T> = async commit => commit()
) {
  const common = {
    peerId: handoff.peerId,
    collectionId: handoff.collectionId,
    recordingId: handoff.recordingId,
    reclaimCapability: handoff.reclaimCapability
  }
  let published = false
  const commitAttempts = new Set<string>()
  const publishIfReleased = async (value: WireRecordingReclaimStatus) => {
    if ((value.state === 'released' || value.state === 'retired') && value.mappingCommitted && !published) {
      await onReleased()
      published = true
    }
  }
  const convert = async (value: WireRecordingReclaimStatus, publish = true) => {
    if (publish) await publishIfReleased(value)
    return {
      state: value.state,
      mappingCommitted: value.mappingCommitted,
      error: value.error === null ? null : remoteFailureError(value.error).normalized
    }
  }
  const args = (requestId: string, callerKey: string) => ({ ...common, requestId, callerKey })
  return createOrderedRecordingReclaimReservations(
    {
      next: async key => {
        const result = await invoke('gatt.ordered.recording-reclaim.next', { ...common, callerKey: key })
        return 'requestId' in result
          ? result.requestId
          : { state: 'not-reserved', error: remoteFailureError(result.error).normalized }
      },
      reserve: async (id, key) => {
        const result = await invoke('gatt.ordered.recording-reclaim.reserve', args(id, key))
        if (result.state === 'not-reserved')
          return { state: 'not-reserved', error: remoteFailureError(result.error).normalized }
        if (result.state !== 'reserved')
          throw contractError('protocol.malformed', 'boundary', 'gatt.ordered.recording-reclaim.reserve')
        return undefined
      },
      start: async (id, key) => convert(await invoke('gatt.ordered.recording-reclaim.start', args(id, key))),
      // Status must retain the native receipt even when graph activation fails.
      // Release or retirement can retry activation separately.
      status: async (id, key) => convert(await invoke('gatt.ordered.recording-reclaim.status', args(id, key)), false),
      commit: async (id, key) => {
        if (commitAttempts.has(key)) {
          const current = await invoke('gatt.ordered.recording-reclaim.status', args(id, key))
          if (current.mappingCommitted) return convert(current)
        }
        return commitWithFence(async () => {
          commitAttempts.add(key)
          return convert(await invoke('gatt.ordered.recording-reclaim.commit', args(id, key)))
        })
      },
      cancel: async (id, key) => convert(await invoke('gatt.ordered.recording-reclaim.cancel', args(id, key))),
      release: async (id, key) => convert(await invoke('gatt.ordered.recording-reclaim.release', args(id, key))),
      retire: async (id, key) => {
        await publishIfReleased(await invoke('gatt.ordered.recording-reclaim.retire', args(id, key)))
      },
      acknowledgeRetirement: async (id, key) => {
        await invoke('gatt.ordered.recording-reclaim.ack-retire', args(id, key))
      },
      forgetRetirement: async (id, key) => {
        await invoke('gatt.ordered.recording-reclaim.forget-retire', args(id, key))
      },
      acknowledgeForget: async (id, key) => {
        await invoke('gatt.ordered.recording-reclaim.ack-forget', args(id, key))
        commitAttempts.delete(key)
      }
    },
    newKey
  )
}
