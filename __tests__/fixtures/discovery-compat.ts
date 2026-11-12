import type {
  DiscoveryEvent,
  GattOrderedCleanup,
  GattOrderedItem,
  GattOrderedLossScope,
  GattOrderedMarkerWritePhase,
  GattOrderedMarkerWriteStatus,
  GattOrderedRecordingFailure,
  GattOrderedRecordingFailureValue,
  GattOrderedRecordingPlatformDetail,
  GattOrderedRecordingReclaim,
  GattOrderedRecordingReclaimReceipt,
  GattOrderedRecordingReclaimStatus,
  GattOrderedRecordingStartReceipt,
  GattOrderedRecordingStartResult,
  GattOrderedRecordingStorageFailure,
  GattOrderedRecordingStopFailure,
  GattOrderedRejectedLoss,
  GattOrderedSourceGeneration,
  GattOrderedSourceMembership,
  GattOrderedSourceRejection,
  LossAwareDiscoveryEventV1,
  PublicStreamItem,
  ScanSession
} from '../../src'
import type { StreamItem } from '../../src/backend-contract/streams'
import type { PortableStreamItem } from '../../src/manager/consumer-handles'

function exhaustiveStream<Value>(
  item: StreamItem<Value> | PublicStreamItem<Value> | PortableStreamItem<Value>
): string {
  switch (item.kind) {
    case 'value':
      return 'value'
    case 'overflow':
      return 'overflow'
    case 'terminal':
      return 'terminal'
    default: {
      const impossible: never = item
      return impossible
    }
  }
}

export const exhaustiveGenericStream = exhaustiveStream

export function exhaustive(event: DiscoveryEvent): string {
  switch (event.kind) {
    case 'observed':
      return event.peer.id
    case 'lost':
      return event.peer.id
    case 'presence-tracking-overflow':
      return String(event.droppedEntries)
    default: {
      const impossible: never = event
      return impossible
    }
  }
}

export function optIn(scan: ScanSession): AsyncIterable<LossAwareDiscoveryEventV1> | undefined {
  return scan.lossAwareEventsV1
}

/** The canonical root entrypoint exports every ordered public result component. */
export type OrderedPublicSurface = {
  cleanup: GattOrderedCleanup
  item: GattOrderedItem
  lossScope: GattOrderedLossScope
  markerPhase: GattOrderedMarkerWritePhase
  markerStatus: GattOrderedMarkerWriteStatus
  failure: GattOrderedRecordingFailure
  failureValue: GattOrderedRecordingFailureValue
  platformDetail: GattOrderedRecordingPlatformDetail
  reclaim: GattOrderedRecordingReclaim
  reclaimReceipt: GattOrderedRecordingReclaimReceipt
  reclaimStatus: GattOrderedRecordingReclaimStatus
  startReceipt: GattOrderedRecordingStartReceipt
  startResult: GattOrderedRecordingStartResult
  storageFailure: GattOrderedRecordingStorageFailure
  stopFailure: GattOrderedRecordingStopFailure
  rejectedLoss: GattOrderedRejectedLoss
  sourceGeneration: GattOrderedSourceGeneration
  sourceMembership: GattOrderedSourceMembership
  sourceRejection: GattOrderedSourceRejection
}
