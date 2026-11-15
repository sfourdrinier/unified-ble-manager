// src/backends/reactnative/react-native-ordered-recovery.ts

/** Sensitive native ownership capability. Transfer only to a trusted replacement host. */
export interface ReactNativeOrderedRecoveryHandle {
  readonly peerId: string
  readonly collectionId: string
  readonly reclaimCapability: string
}

/** Sensitive recorder handoff; reveal only by explicit request to a trusted host. */
export interface ReactNativeOrderedRecordingRecoveryHandle extends ReactNativeOrderedRecoveryHandle {
  readonly recordingId: string
}

/** Retained authenticated reclaim; context is available after commit and release. */
export type ReactNativeOrderedRecordingReclaimHandle =
  import('../../core/ordered-recording-reclaim').OrderedRecordingReclaimHandle<{
    readonly state: import('./rust-core-wire').WireRecordingReclaimStatus['state']
    readonly mappingCommitted: boolean
    readonly error: import('../../backend-contract/errors').NormalizedBleError | null
  }> & { context(): Promise<import('../../core/ordered-recording').OrderedRecordingContext> }

export type ReactNativeOrderedRecovery =
  | { readonly state: 'unopened'; readonly close: () => Promise<'released' | 'pending-source' | 'pending-drain'> }
  | {
      readonly state: 'reclaimed'
      readonly stream: import('../../core/ordered-notification-contract').OrderedNotificationStream
      readonly connectionGeneration: string
      readonly databaseGeneration: string
      readonly members: readonly {
        readonly memberId: string
        readonly selector: {
          readonly serviceUuid: string
          readonly serviceOccurrence: number
          readonly characteristicUuid: string
          readonly characteristicOccurrence: number
        }
        readonly requestedDelivery:
          | 'prefer-notification'
          | 'prefer-indication'
          | 'require-notification'
          | 'require-indication'
      }[]
    }
