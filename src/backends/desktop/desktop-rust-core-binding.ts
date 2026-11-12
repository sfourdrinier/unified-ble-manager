import { utf8ByteLength } from '../../backend-contract/serializable'
import { snapshotPlatformMetadataEntries, type PlatformMetadataEntry } from '../../backend-contract/errors'
// src/backends/desktop/desktop-rust-core-binding.ts
//
// The JS face of the shared desktop Rust core: the `UbmCentral` surface of
// the N-API addon (`bindings/napi/src/dispatch.rs`), the binding the
// provider opens centrals through, and the verbatim mapping of the addon's
// error wire form onto contract errors. Types only plus pure mapping: the
// addon itself is loaded by `src/desktop-core-addon.ts`.

import { BackendContractError, BLE_ERROR_CODES, BLE_ERROR_DOMAINS, contractError } from '../../backend-contract/errors'
import type { ObservationOrigin } from '../../backend-contract/advertisement'
import type {
  BleCommitUncertainty,
  BleErrorCode,
  BleErrorDomain,
  BleRetryability,
  CleanupRecord
} from '../../backend-contract/errors'
import type { SerializableRecord } from '../../backend-contract/primitives'

/** The three desktop radio families the one Rust provider drives. */
export type DesktopRustCorePlatform = 'bluez' | 'corebluetooth' | 'winrt'

/** Which radio an admitted central executes: production btleplug or the deterministic synthetic. */
export type DesktopRustCoreRadio = 'production' | 'synthetic'

/** Selector accepted by the dispatch surface (occurrences select among duplicates). */
export interface DesktopRustCoreSelector {
  readonly serviceUuid: string
  readonly serviceOccurrence?: number
  readonly characteristicUuid?: string
  readonly characteristicOccurrence?: number
  readonly descriptorUuid?: string
  readonly descriptorOccurrence?: number
}

/**
 * Per-call control: `timeoutMs` is the caller budget (absent = no caller
 * budget; the core's named liveness backstop bounds the call) and `ticket`
 * the id `createTicket` minted for cancelling exactly this call.
 */
export interface DesktopRustCoreControl {
  readonly timeoutMs?: number
  readonly ticket?: string
}

/** Native GATT admission retains the authenticated connection's actual lease. */
export interface DesktopRustCoreGattTarget extends DesktopRustCoreControl {
  readonly peerId: string
  readonly lease: string
  readonly selector: DesktopRustCoreSelector
}

export interface DesktopRustCoreAdvertisement {
  readonly peerId: string
  readonly address?: string | null
  readonly addressType?: 'public' | 'random' | null
  readonly rssi?: number | null
  readonly localName?: string | null
  readonly serviceUuids: readonly string[]
  readonly manufacturerData: ReadonlyArray<{ readonly companyId: number; readonly payload: unknown }>
  readonly serviceData: ReadonlyArray<{ readonly uuid: string; readonly payload: unknown }>
  readonly txPower?: number | null
  /** `null`/absent = not carried or not reported; `[]` = carried and empty. */
  readonly solicitedServiceUuids?: readonly string[] | null
  readonly overflowServiceUuids?: readonly string[] | null
  readonly connectable?: boolean | null
  /**
   * What the observation is: `advertisement` (this one advertisement's own
   * data) or `device-state` (the OS's merged device state: BlueZ `Device1`,
   * a known-device report).
   */
  readonly source?: ObservationOrigin
}

/** One observation of the live scan (finding 121). */
export interface DesktopRustCoreScanObservation {
  readonly advertisement: DesktopRustCoreAdvertisement
  /** The core operation id of the scan the core queued it for. */
  readonly scanOperationId: string
  /** Milliseconds since the core received it from the radio, measured at take. */
  readonly ageMs: number
}

/** Characteristic facts beyond the five property bits (`null` = not reported, never false). */
export interface DesktopRustCoreCharacteristicAccess {
  readonly broadcast?: boolean | null
  readonly authenticatedSignedWrites?: boolean | null
  readonly extendedProperties?: boolean | null
  readonly reliableWrite?: boolean | null
  readonly writableAuxiliaries?: boolean | null
  readonly encryptRead?: boolean | null
  readonly encryptWrite?: boolean | null
  readonly encryptAuthenticatedRead?: boolean | null
  readonly encryptAuthenticatedWrite?: boolean | null
  readonly secureRead?: boolean | null
  readonly secureWrite?: boolean | null
  readonly authorize?: boolean | null
}

export interface DesktopRustCorePath {
  readonly servicePrimary?: boolean | null
  readonly includedServices?: readonly { readonly uuid: string; readonly occurrence: number }[] | null
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid?: string | null
  readonly characteristicOccurrence?: number | null
  readonly descriptorUuid?: string | null
  readonly descriptorOccurrence?: number | null
  readonly properties: number
  readonly access?: DesktopRustCoreCharacteristicAccess | null
  /** `os-reserved` or `access-denied` on a service-level path. Absent when the service is open. */
  readonly serviceAccess?: 'os-reserved' | 'access-denied' | null
}

/** Link-security facts the OS reports. */
export interface DesktopRustCoreSecurityState {
  readonly bond: 'bonded' | 'not-bonded' | 'unknown'
  readonly pairingPossible?: boolean | null
}

export interface DesktopRustCorePairOutcome {
  readonly outcome: 'paired' | 'already-paired' | 'rejected' | 'cancelled'
  readonly state?: DesktopRustCoreSecurityState | null
  readonly reason?: string | null
}

export interface DesktopRustCoreSecurityEvent {
  readonly kind: 'state' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly state?: DesktopRustCoreSecurityState | null
  readonly missed?: number | null
}

/** Observed connection parameters. Interval and supervision timeout are microseconds. */
export interface DesktopRustCoreConnectionParameters {
  readonly intervalUs: number
  readonly latency: number
  readonly supervisionTimeoutUs: number
}

/** One connection-parameter report, or a gap marker. */
export interface DesktopRustCoreConnectionParametersEvent {
  readonly kind: 'state' | 'lagged' | 'closed' | 'source-failed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly connectionGeneration?: string | null
  readonly intervalUs?: number | null
  readonly latency?: number | null
  readonly supervisionTimeoutUs?: number | null
  readonly missed?: number | null
  readonly error?: string | null
}

/** One write-without-response readiness report, or a gap marker. */
export interface DesktopRustCoreWriteReadinessEvent {
  readonly kind: 'state' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly connectionGeneration?: string | null
  readonly ready?: boolean | null
  readonly missed?: number | null
}

/** A scan the OS ended without a stop request, or a gap marker. */
export interface DesktopRustCoreScanTerminalEvent {
  readonly kind: 'terminal' | 'stage-failed' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly operationId?: string | null
  readonly aborted?: boolean | null
  readonly detail?: string | null
  /** Private N-API stage-failure error wire, valid only with `stage-failed`. */
  readonly stageFailure?: string | null
  readonly missed?: number | null
  /** Retained native terminal awaiting canonical JS commit and explicit ACK. */
  readonly evidence?: {
    readonly reservationId: string
    readonly pendingToken: string
  } | null
}

/** One row of the core's capability registration for a desktop OS. */
export interface DesktopRustCoreCapabilityState {
  readonly id: string
  readonly state: 'supported' | 'limited' | 'unavailable' | 'unsupported'
  readonly limitation?: string | null
}

/** One decoder for static diagnostic rows and instantiated native capability truth. */
export function decodeDesktopRustCoreCapabilityStates(
  value: unknown,
  operation: string
): readonly DesktopRustCoreCapabilityState[] {
  const malformed = () => contractError('protocol.malformed', 'core', operation)
  if (!Array.isArray(value)) throw malformed()
  const ids = new Set<string>()
  return Object.freeze(
    Array.from(value, (row: unknown): DesktopRustCoreCapabilityState => {
      if (
        typeof row !== 'object' ||
        row === null ||
        !('id' in row) ||
        typeof row.id !== 'string' ||
        row.id.length === 0 ||
        !('state' in row) ||
        (row.state !== 'supported' &&
          row.state !== 'limited' &&
          row.state !== 'unavailable' &&
          row.state !== 'unsupported') ||
        ids.has(row.id)
      )
        throw malformed()
      const limitation = 'limitation' in row ? row.limitation : null
      if (
        limitation !== null &&
        limitation !== undefined &&
        (typeof limitation !== 'string' || limitation.length === 0)
      )
        throw malformed()
      ids.add(row.id)
      return Object.freeze({ id: row.id, state: row.state, limitation: limitation ?? null })
    })
  )
}

export type DesktopRustCoreAdapterAuthorization = 'granted' | 'denied' | 'restricted' | 'not-determined'

/** One consumer's cumulative stream accounting in the core (finding 131). */
export interface DesktopRustCoreConsumerCounters {
  readonly droppedItems: number
  readonly droppedBytes: number
  readonly replacedItems: number
  /** Values lost before they reached the core (radio intake or forwarder). */
  readonly upstreamLost: number
  readonly terminated: boolean
}

/** Opaque V2 capability for a retained oversized peer-source failure. */
export interface DesktopRustCorePeerSourceFailureRecoveryDescriptor {
  readonly protocol: 1
  readonly representation: 'ubm-peer-source-failure/1'
  readonly host: string
  readonly peer: string
  readonly sourceGenerationIdentity: string
  readonly nativeGroup: string
  readonly transfer: string
  readonly recovery: string
  readonly encodedLength: string
  readonly chunkBytes: number
  /** V2-only opaque source-time recipient authority from pollNotificationV2. */
  readonly recipientLease?: string | null
}

export interface DesktopRustCorePeerSourceFailureRecoveryChunk {
  readonly offset: string
  readonly nextOffset: string
  readonly bytes: Uint8Array
  readonly done: boolean
}

/**
 * A trusted, restored recipient handle. It keeps the source-time route as
 * provenance and never implies that a fresh subscription owns that route.
 * The descriptor plus peer/selector/consumer/lease tuple is the authority
 * for bounded reads and an eventual acknowledgement.
 */
export interface DesktopRustCoreDurablePeerSourceFailureRecoveryHandle {
  readonly descriptor: DesktopRustCorePeerSourceFailureRecoveryDescriptor
  readonly peerId: string
  readonly serviceUuid: string
  readonly serviceOccurrence: string
  readonly characteristicUuid: string
  readonly characteristicOccurrence: string
  readonly consumer: string
  readonly recipientLease: string
  readonly contiguousRead: string
  readonly acknowledged: boolean
  readonly producerAck: 'not-started' | 'in-flight' | 'committed'
}

/**
 * Established V1 notification poll outcome. Keep this union closed: a
 * retained source failure is deliberately surfaced as V1's existing
 * structured source failure, not an unnegotiated result discriminator.
 */
export interface DesktopRustCoreNotificationPoll {
  readonly sourceGeneration?: string | null
  readonly sourceOrdinal?: string | null
  readonly kind: 'value' | 'empty' | 'terminal' | 'invalidated' | 'closed' | 'peer-source-loss'
  readonly provenance?: import('../../backend-contract/gatt').NotificationProvenance | null
  readonly peerId?: string
  readonly generation?: string
  readonly skippedRecords?: string | null
  readonly value?: Uint8Array | null
  readonly receipt?: import('../../backend-contract/gatt').NotificationReceipt | null
  readonly cause?: string | null
  readonly droppedItems?: number | null
  readonly droppedBytes?: number | null
  readonly replacedItems?: number | null
  readonly evidence?: {
    readonly reservationId: string
    readonly pendingToken: string
  } | null
}

/**
 * Explicit V2 notification poll outcome. Call only after the caller has
 * selected the V2 retained-recovery capability; its new discriminator is
 * never accepted on the V1 method above.
 */
export interface DesktopRustCoreNotificationPollV2 {
  readonly sourceGeneration?: string | null
  readonly sourceOrdinal?: string | null
  readonly kind:
    | 'value'
    | 'empty'
    | 'terminal'
    | 'invalidated'
    | 'closed'
    | 'peer-source-loss'
    | 'source-failure-recovery'
  readonly recovery?: DesktopRustCorePeerSourceFailureRecoveryDescriptor | null
  /** Exact native producer-error diagnostic for a retained V2 recovery offer. */
  readonly recoveryError?: string | null
  readonly provenance?: import('../../backend-contract/gatt').NotificationProvenance | null
  readonly peerId?: string
  readonly generation?: string
  readonly skippedRecords?: string | null
  readonly value?: Uint8Array | null
  readonly receipt?: import('../../backend-contract/gatt').NotificationReceipt | null
  readonly cause?: string | null
  readonly droppedItems?: number | null
  readonly droppedBytes?: number | null
  readonly replacedItems?: number | null
  readonly evidence?: {
    readonly reservationId: string
    readonly pendingToken: string
  } | null
}

/** One connection-lifecycle event from the core, or a gap marker. */
export interface DesktopRustCoreLifecycleEvent {
  /** `adapter-lost`: the core released this link because the adapter was lost (LEGACY-AUDIT-1 #57). */
  readonly kind: 'link-lost' | 'released' | 'services-changed' | 'adapter-lost' | 'host-reset' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly peerKey?: string | null
  readonly connectionGeneration?: string | null
  readonly requested?: boolean | null
  readonly missed?: number | null
  readonly platform?: string | null
}

/** A native callback with no accepted graph or connection attribution. */
export interface DesktopRustCoreSourceUncertaintyEvent {
  readonly kind: 'source-uncertain' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly code?: string | null
  readonly operation?: string | null
  readonly detail?: string | null
  /** Typed platform-detail JSON, when the native callback supplied it. */
  readonly platform?: string | null
  readonly missed?: number | null
}

/**
 * One resolved radio peer with the core's connection facts: what a lagged
 * lifecycle receiver re-reads (LEGACY-AUDIT-2 N5). An absent fact means the
 * core holds no connection record for the peer.
 */
export interface DesktopRustCorePeerRecord {
  readonly peerId: string
  readonly peerKey: string
  readonly connectionState?: 'connecting' | 'connected' | 'disconnecting' | 'disconnected' | 'lost' | 'invalid' | null
  readonly connectionGeneration?: string | null
  readonly databaseGeneration?: string | null
  readonly databaseState?: 'undiscovered' | 'discovering' | 'current' | 'changed' | 'invalid' | null
}

/** OS directory observation, not a connection lease or advertisement. */
export interface DesktopRustCoreDirectoryPeer {
  readonly peerId: string
  readonly name: string | null
  readonly connection: 'connected' | 'disconnected' | 'unknown'
}

/** The core's own attachment identities (one generation). */
export interface DesktopRustCoreAttachmentTuple {
  readonly attachmentId: string
  readonly backendInstanceId: string
  readonly backendGeneration: string
  readonly adapterId: string
  readonly adapterGeneration: string
}

/** Why the adapter was lost to live work. */
export type DesktopRustCoreAdapterLossCause =
  | 'powered-off'
  | 'resetting'
  | 'unsupported'
  | 'unauthorized'
  | 'removed'
  | 'daemon-restarted'
  | 'source-observation-overflow'

/**
 * One adapter reset (LEGACY-AUDIT-1 #57), published after the core settled
 * every live operation `operation.reset`, ended the owned scan (an aborted
 * scan terminal), released every link (`adapter-lost` lifecycle events),
 * invalidated every subscription (`adapter-reset`) and advanced its
 * generation; or a gap marker.
 */
export interface DesktopRustCoreAdapterResetEvent {
  readonly kind: 'reset' | 'lagged' | 'closed'
  readonly sequence?: number | null
  /** The core's power fact at this reset boundary, never a later status read. */
  readonly power?: DesktopRustCoreAdapterPower | null
  /** The matching adapter-event sequence, if this reset followed one. */
  readonly adapterSequence?: number | null
  readonly cause?: DesktopRustCoreAdapterLossCause | null
  readonly previous?: DesktopRustCoreAttachmentTuple | null
  readonly current?: DesktopRustCoreAttachmentTuple | null
  readonly cancelledOperations?: number | null
  readonly endedScan?: string | null
  readonly releasedLinks?: readonly string[] | null
  readonly endedSubscriptions?: number | null
  /** OS releases that did not complete, in the error wire form. */
  readonly releaseFailures?: readonly string[] | null
  readonly missed?: number | null
}

/** The adapter facts the core admits against; an unreported fact is absent. */
export interface DesktopRustCoreAdapterStatus {
  readonly power?: DesktopRustCoreAdapterPower | null
  readonly authorization?: DesktopRustCoreAdapterAuthorization | null
  readonly availability: 'available' | 'unavailable' | 'unsupported' | 'unknown'
  readonly lost: boolean
}

/** One adapter power-state change from the OS, or a gap marker. */
export interface DesktopRustCoreAdapterEvent {
  readonly kind: 'state' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly state?: DesktopRustCoreAdapterPower | null
  readonly missed?: number | null
}

/** The OS adapter state as the core reports it (`resetting`/`unsupported`: LEGACY-AUDIT-1 #60). */
export type DesktopRustCoreAdapterPower =
  | 'powered-on'
  | 'powered-off'
  | 'resetting'
  | 'unsupported'
  | 'unauthorized'
  | 'unknown'

export interface DesktopRustCoreCancelInfo {
  readonly outcome: string
  readonly kind?: string | null
  readonly cause?: string | null
  readonly reachedRadio?: boolean | null
}

export interface DesktopRustCoreTicketCancel {
  readonly outcome: 'recorded-before-admission' | 'forwarded' | 'already-settled'
  readonly operationId?: string | null
  readonly cancel?: DesktopRustCoreCancelInfo | null
}

export interface DesktopRustCoreCloseReport {
  readonly state: 'released' | 'release-failed'
  readonly failures: ReadonlyArray<{ readonly resourceKind: string; readonly error: string }>
}

/** Rust-side execution counts (the "Rust ingress" witness). */
export type DesktopRustCoreDispatchCounters = Readonly<Record<string, number>>

export interface DesktopRustCoreAdapterListing {
  readonly index: number
  readonly label?: string | null
  readonly error?: string | null
  /** The OS's descriptive name, when it gives one. */
  readonly displayName?: string | null
  /** Whether an unnamed open selects this adapter (the OS default). */
  readonly default?: boolean
  /** Windows: how this process is deployed (the legacy per-adapter `deployment`). */
  readonly deployment?: 'packaged' | 'unpackaged' | null
}

/**
 * The `UbmCentral` JS surface this provider executes. The default binding
 * resolves it from the packaged addon; tests inject a faithful double.
 * Synthetic staging methods are deliberately absent: production traffic
 * never stages.
 */
export interface DesktopOrderedDiscardReceipt {
  readonly receiptId: string
  readonly discardedDeliveryId: string
  readonly evidence: string
}

export interface DesktopRustCoreCentral {
  reserveTerminalEvidence?(options: {
    readonly producer: 'scan' | 'subscription'
    readonly maximumBytes: string
    readonly formatVersion: 1
    readonly scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
    readonly scopeUtf16le?: Uint8Array
  }): Promise<{ readonly reservationId: string; readonly maximumBytes: string }>
  commitTerminalEvidence?(options: {
    readonly reservationId: string
    readonly canonicalBytes: Uint8Array
    readonly digest: string
  }): Promise<{
    readonly kind: 'committed' | 'refused'
    readonly reason?: 'quota-exhausted' | 'storage-unavailable' | 'capability-unsupported'
    readonly ref?: string
    readonly state: 'staged' | 'committed'
    readonly canonicalBytes: Uint8Array
    readonly byteLength?: string
    readonly digest?: string
    readonly formatVersion?: number
    readonly durability?: 'committed'
  }>
  commitRecoveredTerminalEvidence?(options: {
    readonly reservationId: string
    readonly leaseId: string
    readonly canonicalBytes: Uint8Array
    readonly digest: string
  }): Promise<{
    readonly kind: 'committed' | 'refused'
    readonly reason?: 'quota-exhausted' | 'storage-unavailable' | 'capability-unsupported'
    readonly ref?: string
    readonly byteLength?: string
    readonly digest?: string
    readonly formatVersion?: number
    readonly durability?: 'committed'
  }>
  readTerminalEvidence?(
    reference: string,
    maximumBytes: string
  ): Promise<{
    readonly kind: 'committed' | 'missing' | 'unresolved'
    readonly canonicalBytes?: Uint8Array
    readonly digest?: string
  }>
  /** Releases only a transient reservation lease; committed evidence remains recoverable. */
  releaseTerminalEvidence?(reservationId: string): Promise<void>
  /** Authenticated recovery acknowledgement is the sole durable retirement path. */
  acknowledgeRecoveredTerminalEvidence?(options: {
    readonly ref: string
    readonly reservationId: string
    readonly leaseId: string
    readonly producer: 'scan' | 'subscription'
    readonly recipient: string
  }): Promise<void>
  /**
   * Binds the durable row to a host-authenticated recovery recipient before
   * the source is activated. Electron main derives this value from its
   * authenticated renderer resources; it is never renderer IPC input.
   */
  bindTerminalEvidenceRecipient?(reservationId: string, recipient: string): Promise<void>
  stageTerminalEvidence?(reservationId: string, recipient: string, canonicalBytes: Uint8Array): Promise<void>
  recoverTerminalEvidence?(options: {
    readonly producer: 'scan' | 'subscription'
    readonly recipient: string
    readonly leaseId: string
  }): Promise<
    readonly {
      readonly reservationId: string
      readonly producer: 'scan' | 'subscription'
      readonly scopeUtf16le?: Uint8Array
      readonly byteLength?: string
      readonly digest?: string
      readonly formatVersion?: number
      readonly ref?: string
      readonly state: 'staged' | 'committed'
      readonly canonicalBytes: Uint8Array
    }[]
  >
  recoverTerminalEvidenceWithStatus?(options: {
    readonly producer: 'scan' | 'subscription'
    readonly recipient: string
    readonly leaseId: string
  }): Promise<{
    readonly entries: readonly {
      readonly reservationId: string
      readonly producer: 'scan' | 'subscription'
      readonly scopeUtf16le?: Uint8Array
      readonly byteLength?: string
      readonly digest?: string
      readonly formatVersion?: number
      readonly ref?: string
      readonly state: 'staged' | 'committed'
      readonly canonicalBytes: Uint8Array
    }[]
    readonly busy: boolean
  }>
  releaseTerminalEvidenceRecoveryLease?(leaseId: string): Promise<void>
  acknowledgeTerminalEvidence?(options: { readonly pendingToken: string }): Promise<void>
  /** Negotiated before any ordered group can enable notifications. */
  orderedGroupWireVersion?(): number
  orderedCapacityWireVersion?(): number
  increaseOrderedByteCapacity?(options: {
    readonly wireVersion: 1
    readonly peerId: string
    readonly collectionId: string
    readonly sourceGeneration: import('../../core/ordered-notification-contract').OrderedSourceGeneration
    readonly maxBytes: number
  }): Promise<import('../../core/ordered-notification-contract').OrderedByteCapacityChange>
  /** Exact ordered failure metadata revision required by this addon. */
  orderedMetadataVersion?(): number
  /** Exact version of both ordered start and stop receipts emitted by this owner. */
  orderedRecordingReceiptVersion?(): number
  reserveOrderedRecordingStart?(): string
  issueOrderedRecordingRecovery?(startHandle: string): Promise<string>
  orderedRecordingReclaimSupported?(): boolean
  orderedRecordingReclaimWireVersion?(): number
  forgetOrderedRecordingSession?(recordingId: string): void
  orderedRecordingReclaim?(
    action:
      | 'next'
      | 'reserve'
      | 'start'
      | 'status'
      | 'commit'
      | 'cancel'
      | 'release'
      | 'retire'
      | 'ack-retirement'
      | 'forget-retirement',
    options: {
      readonly wireVersion: 2
      readonly recordingId: string
      readonly capability: string
      readonly lease: string
      readonly callerKey: string
      readonly requestId?: string
    }
  ): Promise<string>
  confirmOrderedRecordingReclaim?(options: {
    readonly wireVersion: 2
    readonly recordingId: string
    readonly capability: string
    readonly lease: string
    readonly callerKey: string
    readonly requestId: string
  }): void
  orderedRecordingStartStatus?(startHandle: string): string
  retireOrderedRecordingStart?(startHandle: string): Promise<void>
  startOrderedRecording?(
    options: {
      readonly startHandle: string
      readonly wireVersion: number
      readonly metadataVersion: 2
      readonly recordingId: string
      readonly recordingMaxBytes: number
      readonly recordingMaxRecords: number
      readonly peerId: string
      readonly lease: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string
      readonly members: readonly {
        readonly memberId: string
        readonly publicMemberId: string
        readonly selector: DesktopRustCoreSelector
        readonly deliveryMode?: string
        readonly preferredDeliveryMode?: string
      }[]
      readonly maxItems: number
      readonly maxBytes: number
      readonly maxControls: number
    } & DesktopRustCoreControl
  ): Promise<string>
  orderedMarkerWireVersion?(): number
  orderedMarkerWriteSupported?(): boolean
  reserveOrderedMarkerWrite?(options: {
    readonly wireVersion: number
    readonly peerId: string
    readonly collectionId: string
    readonly reservationKey: string
  }): Promise<string>
  startOrderedMarkerWrite?(options: {
    readonly wireVersion: number
    readonly peerId: string
    readonly collectionId: string
    readonly reservationId: string
    readonly selector: DesktopRustCoreSelector
    readonly value: Uint8Array
    readonly withResponse: boolean
  }): Promise<string>
  orderedMarkerWriteStatus?(options: {
    readonly wireVersion: number
    readonly peerId: string
    readonly collectionId: string
    readonly reservationId: string
  }): Promise<string>
  cancelOrderedMarkerWrite?(options: {
    readonly wireVersion: number
    readonly peerId: string
    readonly collectionId: string
    readonly reservationId: string
  }): Promise<string>
  forgetOrderedMarkerWrite?(options: {
    readonly wireVersion: number
    readonly peerId: string
    readonly collectionId: string
    readonly reservationId: string
  }): Promise<void>
  reserveRecordingMarkerWrite?(options: {
    readonly wireVersion: number
    readonly recordingId: string
    readonly peerId: string
    readonly lease: string
    readonly reservationKey: string
  }): Promise<string>
  startRecordingMarkerWrite?(options: {
    readonly wireVersion: number
    readonly recordingId: string
    readonly peerId: string
    readonly lease: string
    readonly reservationId: string
    readonly selector: DesktopRustCoreSelector
    readonly value: Uint8Array
    readonly withResponse: boolean
  }): Promise<string>
  recordingMarkerWriteStatus?(options: {
    readonly wireVersion: number
    readonly recordingId: string
    readonly peerId: string
    readonly lease: string
    readonly reservationId: string
  }): Promise<string>
  cancelRecordingMarkerWrite?(options: {
    readonly wireVersion: number
    readonly recordingId: string
    readonly peerId: string
    readonly lease: string
    readonly reservationId: string
  }): Promise<string>
  forgetRecordingMarkerWrite?(options: {
    readonly wireVersion: number
    readonly recordingId: string
    readonly peerId: string
    readonly lease: string
    readonly reservationId: string
  }): Promise<void>
  reserveOrderedGroup?(peerId: string): string
  openOrderedGroup?(
    options: {
      readonly wireVersion: number
      readonly metadataVersion: 2
      readonly peerId: string
      readonly collectionId: string
      readonly lease: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string
      readonly members: readonly {
        readonly memberId: string
        readonly selector: DesktopRustCoreSelector
        readonly deliveryMode?: string
        readonly preferredDeliveryMode?: string
      }[]
      readonly maxItems: number
      readonly maxBytes: number
      readonly maxControls: number
    } & DesktopRustCoreControl
  ): Promise<void>
  pollOrderedGroup?(
    options: { readonly peerId: string; readonly collectionId: string } & DesktopRustCoreControl
  ): Promise<{ readonly deliveryId?: string | null; readonly record: string }>
  acknowledgeOrderedDelivery?(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<void>
  discardOrderedDelivery?(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<DesktopOrderedDiscardReceipt>
  pollOrderedDiscardReceipt?(options: {
    readonly peerId: string
    readonly collectionId: string
  }): Promise<DesktopOrderedDiscardReceipt | null | undefined>
  acknowledgeOrderedDiscardReceipt?(options: {
    readonly peerId: string
    readonly collectionId: string
    readonly deliveryId: string
  }): Promise<void>
  orderedGroupStatus?(
    options: { readonly peerId: string; readonly collectionId: string } & DesktopRustCoreControl
  ): Promise<string>
  closeOrderedGroup?(
    options: { readonly peerId: string; readonly collectionId: string } & DesktopRustCoreControl
  ): Promise<'released' | 'pending-source' | 'pending-drain'>
  acknowledgeOrderedGroup?(
    options: { readonly peerId: string; readonly collectionId: string } & DesktopRustCoreControl
  ): Promise<void>
  forgetOrderedGroup?(
    options: { readonly peerId: string; readonly collectionId: string } & DesktopRustCoreControl
  ): Promise<void>
  /** Actual registered instance capabilities, including runtime authority limitations. */
  runtimeCapabilityStates(): Promise<readonly DesktopRustCoreCapabilityState[]>
  /** Optional on injected backends; the trusted-host controller fails closed when absent. */
  continuationExecute?(peerId: string, declarationJson: string): Promise<string>
  continuationPrepareClaim?(maxItems: number, maxBytes: number): Promise<string>
  continuationAcknowledgeClaim?(claimToken: string): Promise<string>
  continuationPrepareRetainedClaimV1?(maxItems: number, maxBytes: number): Promise<string>
  continuationAcknowledgeRetainedClaimV1?(claimToken: string): Promise<string>
  /**
   * Additive retained-prefix V2. Presence alone is insufficient: consumers
   * also require all three descriptor transport methods below before they
   * may negotiate this richer claim shape.
   */
  continuationPrepareRetainedClaimV2?(maxItems: number, maxBytes: number): Promise<string>
  continuationAcknowledgeRetainedClaimV2?(claimToken: string): Promise<string>
  continuationRetireRetainedClaimReceiptV1?(claimToken: string): Promise<string>
  continuationDescribeBacklog?(): Promise<string>
  continuationConfigureRecordingDirectory?(directory: string): Promise<string>
  continuationRecordingStore?(): DesktopRustCoreRecordingStore
  continuationRecordingStatus?(id: string): Promise<string>
  continuationRecordingPrepare?(id: string, maxItems: number, maxBytes: number): Promise<string>
  continuationRecordingAcknowledge?(id: string, token: string): Promise<string>
  continuationRecordingStop?(id: string): Promise<string>
  continuationRecordingClear?(id: string): Promise<string>
  createTicket(gattPeerId?: string): string
  cancelTicket(ticket: string): Promise<DesktopRustCoreTicketCancel>
  releaseTicket(ticket: string): boolean
  adapterName(): Promise<string>
  adapterState(options?: DesktopRustCoreControl): Promise<DesktopRustCoreAdapterPower>
  takeAdapterEvent(): Promise<DesktopRustCoreAdapterEvent | null>
  takeLifecycleEvent(): Promise<DesktopRustCoreLifecycleEvent | null>
  takeSourceUncertaintyEvent?(): Promise<DesktopRustCoreSourceUncertaintyEvent | null>
  dispatchCounters(): DesktopRustCoreDispatchCounters
  resourceCounters(): Promise<{
    readonly nativeGattAdmissions: number
    readonly acquiredGattTransports: number
    readonly pendingGattAcquisitions: number
    readonly routedSubscriptions: number
    readonly pendingDisables: number
    readonly retainedEnablements: number
    readonly liveOperations: number
    readonly liveConnections: number
    readonly liveConsumers: number
    readonly scanOwned: boolean
  }>
  startScan(
    options: {
      readonly owner: string
      readonly serviceUuids: string[]
      /** The OS duplicate filter policy (`all` when absent). */
      readonly duplicatePolicy?: 'all' | 'first' | 'merged'
      readonly winrtScanningMode?: 'active' | 'passive' | 'none'
      readonly winrtAllowExtendedAdvertisements?: boolean
      /**
       * The local-name prefix the OS filter narrows by (BlueZ
       * `SetDiscoveryFilter` `Pattern`; CoreBluetooth and WinRT ignore it).
       * It only narrows: the caller's software match stays the final filter.
       */
      readonly localNamePrefix?: string
      readonly terminalEvidenceScope?: {
        readonly managerId: string
        readonly operationId: string
        readonly incarnation: string
      }
      /** Lossless packed UTF-16LE scope sidecar for native durable identity. */
      readonly terminalEvidenceScopeUtf16le?: Uint8Array
      /** Trusted operation identity for source-time terminal evidence. */
      readonly terminalEvidenceOperation?: string
    } & DesktopRustCoreControl
  ): Promise<{ operationId: string }>
  stopScan(operationId: string, options?: DesktopRustCoreControl): Promise<'stopped' | 'not-active'>
  /** The live scan's next observation with its scan id and age (`null` when none waits). */
  takeScanObservation(): Promise<DesktopRustCoreScanObservation | null>
  connect(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<{
    peerKey: string
    connectionGeneration: string | null
  }>
  connectWhenAvailable: DesktopRustCoreCentral['connect']
  retainConnectionLease(options: {
    readonly peerId: string
    readonly sourceLease: string
    readonly destinationLease: string
    readonly connectionGeneration: string
  }): Promise<void>
  disconnect(
    options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl
  ): Promise<DesktopRustCoreConnectionReleaseReport>
  readRssi(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<number>
  /** Observed connection parameters. Interval and supervision timeout are microseconds. */
  requestPriority(
    options: {
      readonly peerId: string
      readonly lease: string
      readonly priority: 'balanced' | 'high-throughput' | 'low-power'
    } & DesktopRustCoreControl
  ): Promise<boolean>
  readPhy(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<{
    readonly txPhy: 'le-1m' | 'le-2m' | 'le-coded'
    readonly rxPhy: 'le-1m' | 'le-2m' | 'le-coded'
  }>
  connectionParameters(
    options: {
      readonly peerId: string
      readonly lease: string
      readonly observationWatch?: boolean
    } & DesktopRustCoreControl
  ): Promise<DesktopRustCoreConnectionParameters>
  takeConnectionParameterEvent(): Promise<DesktopRustCoreConnectionParametersEvent | null>
  /**
   * Effective ATT MTU of the live link when the OS observed one.
   * Windows reports `GattSession.MaxPduSize`. Linux reports the BlueZ
   * characteristic MTU. macOS resolves `null`: CoreBluetooth write length
   * is not an ATT MTU.
   */
  readEffectiveMtu(
    options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl
  ): Promise<number | null>
  /**
   * Registers the whole snapshot or rejects with a typed error: a malformed
   * platform UUID is `protocol.malformed` (`discovery.snapshot.uuid`), a
   * database past the ATT handle space `capability.limited`
   * (`discovery.database-bound`). Nothing is skipped.
   */
  discover(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<{
    pathsRegistered: number
    readonly connectionGeneration?: string | null
    readonly databaseGeneration?: string | null
  }>
  discoveredPaths(peerId: string): Promise<DesktopRustCorePath[]>
  /** The value and the radio's provenance word (`read-response` | `read-or-notification`). */
  read(options: DesktopRustCoreGattTarget): Promise<{ readonly value: Uint8Array; readonly provenance: string }>
  write(
    options: {
      readonly value: Uint8Array
      readonly mode?: string
    } & DesktopRustCoreGattTarget
  ): Promise<void>
  writeWhenReady(
    options: { readonly value: Uint8Array; readonly mode: 'without-response' } & DesktopRustCoreGattTarget
  ): Promise<void>
  acquireGatt(
    options: DesktopRustCoreGattTarget,
    kind: 'write' | 'notify'
  ): Promise<{ readonly handle: string; readonly mtu: number }>
  acquiredWrite(
    options: DesktopRustCoreControl & {
      readonly peerId: string
      readonly lease: string
      readonly handle: string
      readonly value: Uint8Array
    }
  ): Promise<void>
  acquiredReceive(
    options: DesktopRustCoreControl & { readonly lease: string; readonly handle: string }
  ): Promise<Uint8Array>
  closeAcquired(options: DesktopRustCoreControl & { readonly lease: string; readonly handle: string }): Promise<void>
  readDescriptor(options: DesktopRustCoreGattTarget): Promise<Uint8Array>
  writeDescriptor(
    options: {
      readonly value: Uint8Array
    } & DesktopRustCoreGattTarget
  ): Promise<void>
  subscribe(
    options: {
      readonly consumer: string
      readonly deliveryMode?: 'notification' | 'indication'
      /** A soft preference; unsupported modes fall back to the platform's supported mode. */
      readonly preferredDeliveryMode?: 'notification' | 'indication'
      /** The consumer's overflow policy in the core (`error` when absent). */
      readonly overflowPolicy?: 'error' | 'drop-oldest' | 'drop-newest' | 'latest'
      readonly terminalEvidenceScope?: {
        readonly managerId: string
        readonly operationId: string
        readonly incarnation: string
      }
      readonly terminalEvidenceScopeUtf16le?: Uint8Array
    } & DesktopRustCoreGattTarget
  ): Promise<{ delivery: 'notification' | 'indication' | 'unknown' }>
  /** One consumer's cumulative accounting in the core (`null` when it holds no record). */
  consumerCounters(options: {
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
  }): Promise<DesktopRustCoreConsumerCounters | null>
  pollNotification(options: {
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
  }): Promise<DesktopRustCoreNotificationPoll>
  /**
   * V2-only retained peer-source failure poll. Its absence means the loaded
   * binary cannot negotiate the capability; callers must not substitute the
   * V1 poll and reinterpret its result.
   */
  pollNotificationV2?(options: {
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
  }): Promise<DesktopRustCoreNotificationPollV2>
  /** V2-only retained peer-source failure evidence. Absent means unsupported. */
  readPeerNotificationFailureRecovery?(options: {
    readonly descriptor: DesktopRustCorePeerSourceFailureRecoveryDescriptor
    readonly offset: string
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
    readonly recipientLease: string
  }): Promise<DesktopRustCorePeerSourceFailureRecoveryChunk>
  acknowledgePeerNotificationFailureRecovery?(options: {
    readonly descriptor: DesktopRustCorePeerSourceFailureRecoveryDescriptor
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
    /** Exact opaque authority received from `pollNotificationV2`. */
    readonly recipientLease: string
  }): Promise<void>
  /** Trusted main-process trigger for retained durable-stage offers. */
  retryPeerFailureRecoveryStaging?(): Promise<number>
  /** Trusted main-process binding of an existing recording-owned durable ledger. */
  attachPeerFailureRecoveryRecording?(recordingId: string): Promise<void>
  /**
   * Enumerate source-time recovery handles restored from the trusted durable
   * ledger. This is deliberately separate from live subscription polling.
   */
  listPeerNotificationFailureRecoveries?(): readonly DesktopRustCoreDurablePeerSourceFailureRecoveryHandle[]
  unsubscribe(
    options: {
      readonly peerId: string
      readonly selector: DesktopRustCoreSelector
      readonly consumer: string
    } & DesktopRustCoreControl
  ): Promise<boolean>
  cancelOperation(operationId: string): Promise<DesktopRustCoreCancelInfo>
  close(): Promise<DesktopRustCoreCloseReport>
  adapterAuthorization(options?: DesktopRustCoreControl): Promise<DesktopRustCoreAdapterAuthorization>
  securityState(options: { readonly peerId: string } & DesktopRustCoreControl): Promise<DesktopRustCoreSecurityState>
  pair(
    options: { readonly peerId: string; readonly secureConnections?: 'require' | 'disallow' } & DesktopRustCoreControl
  ): Promise<DesktopRustCorePairOutcome>
  cancelPairing(options: { readonly peerId: string } & DesktopRustCoreControl): Promise<{
    outcome: 'cancelled' | 'not-pairing' | 'paired' | 'rejected'
    reason?: string | null
  }>
  unpair(options: { readonly peerId: string } & DesktopRustCoreControl): Promise<'unpaired' | 'already-unpaired'>
  takeSecurityEvent(): Promise<DesktopRustCoreSecurityEvent | null>
  resolveAddress(
    options: { readonly address: string; readonly addressType: 'public' | 'random' } & DesktopRustCoreControl
  ): Promise<string>
  addressType(options: { readonly peerId: string } & DesktopRustCoreControl): Promise<'public' | 'random' | null>
  maximumWriteLength(
    options: {
      readonly peerId: string
      readonly lease: string
      readonly selector: DesktopRustCoreSelector
      readonly withResponse: boolean
    } & DesktopRustCoreControl
  ): Promise<number>
  /** The link's largest single write for one mode, without a discovered database (LEGACY-AUDIT-1 #65). */
  connectionMaximumWriteLength(
    options: {
      readonly peerId: string
      readonly lease: string
      readonly withResponse: boolean
    } & DesktopRustCoreControl
  ): Promise<number>
  writeReadiness(
    options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl
  ): Promise<boolean>
  takeWriteReadinessEvent(): Promise<DesktopRustCoreWriteReadinessEvent | null>
  takeScanTerminalEvent(): Promise<DesktopRustCoreScanTerminalEvent | null>
  /**
   * Install the host's wake callback: called (coalesced, on the JS thread)
   * whenever the core queues an advertisement, a notification value, or a
   * lifecycle, adapter, reset, security, write-readiness or scan-end report.
   * It never keeps the process alive.
   */
  setEventWaker(wake: () => void): void
  /** Wakes the addon could not queue to the host since open (the pump intervals still deliver). */
  eventWakeFailures(): number
  /** Every resolved radio peer with the core's connection facts (the re-read after a lifecycle lag). */
  peerRecords(): Promise<DesktopRustCorePeerRecord[]>
  knownDirectoryPeers(options?: DesktopRustCoreControl): Promise<readonly DesktopRustCoreDirectoryPeer[]>
  connectedPeers(
    options: { readonly services: readonly string[] } & DesktopRustCoreControl
  ): Promise<DesktopRustCoreDirectoryPeer[]>
  bondedPeers(options: DesktopRustCoreControl): Promise<DesktopRustCoreDirectoryPeer[]>
  resolvePeer(
    options: { readonly peerId: string } & DesktopRustCoreControl
  ): Promise<DesktopRustCoreDirectoryPeer | null>
  /** The core operation id of the retained scan owner, `null` when none. */
  activeScanId(): string | null
  /**
   * The core operation id of the scan still operational at the radio, `null`
   * when none. Retained callback-fence cleanup after an OS-ended scan is not
   * operational (the scan-terminal lag re-read).
   */
  operationalScanId(): string | null
  takeAdapterResetEvent(): Promise<DesktopRustCoreAdapterResetEvent | null>
  adapterStatus(): DesktopRustCoreAdapterStatus
  /** Resolves once the adapter is usable; `capability.unavailable` / `adapter.initialize` after `withinMs`. */
  awaitUsableAdapter(withinMs: number): Promise<DesktopRustCoreAdapterPower>
  installPairingGenerationController(
    read: (adapterId: string) => Promise<string>,
    set: (adapterId: string, generation: string) => Promise<void>
  ): void
}

/** Operation-owned, identity-bound private release answer. */
export interface DesktopRustCoreConnectionReleaseReport {
  readonly schema: 'ubm-desktop-release/1'
  readonly state: 'released'
  readonly peerId: string
  readonly lease: string
  readonly connectionGeneration: string | null
  readonly platform?: string | null
}

/** Native Rust core entry: opens one dispatch central on the chosen radio. */
/** The BlueZ D-Bus bus a central runs on (the legacy `busKind`). */
export type DesktopRustCoreBluezBus = 'system' | 'session'

/** Host-supplied privileged pairing-generation operations (BlueZ; see `BluezPairingGenerationController`). */
export interface DesktopRustCoreGenerationController {
  read(adapterId: string): Promise<'legacy-only' | 'enabled' | 'required'>
  set(adapterId: string, generation: 'legacy-only' | 'enabled' | 'required'): Promise<void>
}

/** Trusted host-only native store. Methods return canonical control envelopes. */
export interface DesktopRustCoreRecordingStore {
  orderedReceiptVersion?(id: string): Promise<number>
  configureDirectory(directory: string): Promise<string>
  status(id: string): Promise<string>
  prepare(id: string, maxItems: number, maxBytes: number): Promise<string>
  acknowledge(id: string, token: string): Promise<string>
  diagnosticSummaryPage?(id: string, offset: string, maxBytes: number): Promise<string>
  diagnosticOccurrencesPage?(id: string, after: string | null, limit: number): Promise<string>
  diagnosticOccurrencePage?(id: string, occurrence: string, offset: string, maxBytes: number): Promise<string>
  retryDiagnosticOccurrence?(id: string, occurrence: string): Promise<string>
  acknowledgeDiagnosticOccurrence?(id: string, occurrence: string, payload: string): Promise<string>
  retireDiagnosticOccurrence?(id: string, occurrence: string, payload: string): Promise<string>
  confirmDiagnosticRetirement?(id: string, occurrence: string, payload: string): Promise<string>
  releaseDiagnosticConfirmation?(id: string, occurrence: string, payload: string): Promise<string>
  transitionDiagnosticOccurrenceChunk?(
    id: string,
    occurrence: string,
    transition: string,
    offset: string,
    bytes: readonly number[]
  ): Promise<string>
  /** Trusted native compatibility controls for a pending pre-v5 page. */
  legacyRecordingPrepare?(id: string, maxItems: number, maxBytes: number): Promise<string>
  legacyRecordingAcknowledge?(id: string, token: string): Promise<string>
  legacyRecordingPrepareExact?(id: string, maxItems: number, maxBytes: number): Promise<string>
  legacyRecordingRecoverExact?(id: string): Promise<string>
  legacyRecordingAcknowledgeExact?(id: string, page: string): Promise<string>
  legacyRecordingRetireExact?(id: string, page: string): Promise<string>
  stop(id: string): Promise<string>
  stopOrdered?(id: string): Promise<string>
  orderedContext?(id: string): Promise<string>
  clear(id: string): Promise<string>
  /**
   * Trusted main-process scan-recovery owner. Its returned ids are
   * process-local opaque handles, never persisted capabilities or renderer
   * authority. Electron and Tauri must wrap it in caller/generation grants.
   */
  scanFailureRecovery?(): DesktopRustCoreScanFailureRecovery
}

/** Opaque trusted-host scan recovery bridge exported by the N-API addon. */
export interface DesktopRustCoreScanFailureRecovery {
  attach(recordingId: string): Promise<string>
  owners(leaseId: string): Promise<string>
  member(ownerHandle: string, index: number): Promise<string>
  head(ownerHandle: string, index: number): Promise<string>
  read(headHandle: string, offset: string): Promise<string>
  acknowledge(headHandle: string): Promise<void>
  retire(headHandle: string): Promise<void>
  release(headHandle: string): Promise<void>
  finalizeRelease(headHandle: string): Promise<string>
  pendingCleanupReceipts(leaseId: string): Promise<string>
  retireCleanup(cleanupReceiptId: string): Promise<void>
  rawInfo(ownerHandle: string): Promise<string>
  rawMember(ownerHandle: string, index: number): Promise<string>
  rawToken(ownerHandle: string, index: number, offset: number, maxBytes: number): Promise<string>
  rawRetire(ownerHandle: string): Promise<void>
  rawRelease(ownerHandle: string): Promise<void>
  rawFinalize(ownerHandle: string): Promise<string>
}

export interface DesktopRustCoreBinding {
  /** Exact ordered receipt version emitted by the independently opened store. */
  orderedRecordingReceiptVersion?(): number
  /** Opens durable data without initializing, enumerating or authorizing a radio. */
  openRecordingStore?(directory: string): Promise<DesktopRustCoreRecordingStore>
  openProduction(options: {
    readonly owner: string
    readonly platform: DesktopRustCorePlatform
    readonly adapterId: string | null
    readonly bluezBus?: DesktopRustCoreBluezBus
    readonly connectionPolicy?: import('./bluez-connection-policy').BluezConnectionPolicy
    readonly pairingGeneration?: boolean
    /**
     * Trusted host-owned private storage for raw WinRT source-failure
     * evidence. Renderer code never receives this path; the N-API boundary
     * creates the store before the Windows radio opens.
     */
    readonly failureEvidenceDirectory?: string
    /** Decimal positive byte budget, represented as text to preserve u64 precision. */
    readonly failureEvidenceMaxBytes?: string
  }): Promise<DesktopRustCoreCentral>
  openSynthetic(
    owner: string,
    options?: { readonly pairingGeneration?: boolean; readonly platform?: DesktopRustCorePlatform }
  ): Promise<DesktopRustCoreCentral>
  listAdapters(bluezBus?: DesktopRustCoreBluezBus): Promise<readonly DesktopRustCoreAdapterListing[]>
  /**
   * The core's capability registration for a platform's OS (what the Rust
   * path implements there); `pairingGeneration` states whether the host
   * supplies a generation controller.
   */
  capabilityStates(
    platform: DesktopRustCorePlatform,
    pairingGeneration?: boolean
  ): readonly DesktopRustCoreCapabilityState[]
  /** Load facts the backend reports as identity diagnostics (path, mode, build identity). */
  readonly diagnostics?: Readonly<Record<string, string>>
}

/**
 * The operation id a public desktop error reports, in the 4.x vocabulary of
 * each host (LEGACY-AUDIT-5 S5): `direct-gatt.*` (CoreBluetooth), `winrt.*`
 * and `bluez.*`, named after the operation in flight — never the core's own
 * internal operation. `prefix` is the host's legacy prefix
 * (`DesktopRustCoreProfile.operationPrefix`); `name` the provider's name for
 * the operation. Names with a 4.x equivalent under another name are renamed;
 * the rest keep their name under the host's prefix.
 */
export function desktopRustCoreOperation(prefix: string, name: string): string {
  const exact = (prefix === 'bluez' ? BLUEZ_LEGACY_OPERATION_NAMES[name] : undefined) ?? LEGACY_OPERATION_NAMES[name]
  if (exact !== undefined) return `${prefix}.${exact}`
  for (const [from, to] of LEGACY_OPERATION_PREFIXES) {
    if (name.startsWith(from)) return `${prefix}.${to}${name.slice(from.length)}`
  }
  return `${prefix}.${name}`
}

/** Provider operation names whose 4.x id differs (every host). */
const LEGACY_OPERATION_NAMES: Readonly<Record<string, string>> = Object.freeze({
  // Loading the native core: the 4.x native-boundary vocabulary.
  platform: 'native-boundary.load',
  'rust-core-missing': 'native-boundary.load',
  'rust-core-addon-path': 'native-boundary.load',
  'native-identity': 'native-boundary.version',
  'rust-core-patches': 'native-boundary.version',
  'rust-core-open': 'native-boundary.create',
  'rust-core-open-synthetic': 'native-boundary.create',
  'adapter.initialize': 'native-boundary.initialize',
  'open.adapter-state': 'native-boundary.initialize',
  'open.event-waker': 'native-boundary.initialize',
  // Provider construction and adapter selection.
  'select-adapter': 'provider.select-adapter',
  owner: 'provider.owner',
  'owner-id': 'provider.owner-id',
  'bluez-only-option': 'provider.bluez-only-option',
  'bus-kind': 'provider.bus-kind',
  // Backend operations.
  'connection.connect': 'connect',
  'connection.rssi': 'connection.read-rssi',
  'gatt.write-readiness': 'connection.write-readiness',
  'peer.address-targeting': 'peer-from-address',
  'attach-before-open': 'attach',
  'session.dispose': 'destroy',
  'counters-unavailable': 'destroy.resource-counters'
})

/** BlueZ had no database-handle operation ids: its handles reported the backend verb. */
const BLUEZ_LEGACY_OPERATION_NAMES: Readonly<Record<string, string>> = Object.freeze({
  'gatt.database-read': 'gatt.read',
  'gatt.database-write': 'gatt.write',
  'gatt.database-read-descriptor': 'gatt.read-descriptor',
  'gatt.database-write-descriptor': 'gatt.write-descriptor',
  'gatt.database-subscribe': 'gatt.subscribe',
  // B-R2: BlueZ kept the `.pair` segment WinRT never had
  // (`bluez.security.pair.custom-ceremony` vs `winrt.security.custom-ceremony`).
  'security.custom-ceremony': 'security.pair.custom-ceremony'
})

const LEGACY_OPERATION_PREFIXES: ReadonlyArray<readonly [string, string]> = Object.freeze([
  ['list-adapters', 'provider.list-adapters'],
  ['capability-states.', 'native-boundary.surface.'],
  ['connection.connect.', 'connect.'],
  ['session.dispose.', 'destroy.']
])

const WIRE_FORM = /^([^|]+)\|([^|]+)\|([^|]*)\|(never|caller-decides)\|([a-z-]*)\|([^|]*)\|(.*)$/su

function errorMessage(error: unknown): string {
  // Never `instanceof Error`: dispatch rejections may arrive cross-realm
  // (the addon builds them outside the caller realm), where prototype
  // identity fails but the `message` data property still reads.
  if (typeof error === 'object' && error !== null) {
    const message: unknown = Reflect.get(error, 'message')
    if (typeof message === 'string') return message
  }
  return String(error)
}

function knownCode(value: string): BleErrorCode | undefined {
  return BLE_ERROR_CODES.find(code => code === value)
}

function knownDomain(value: string): BleErrorDomain | undefined {
  return BLE_ERROR_DOMAINS.find(domain => domain === value)
}

/**
 * The OS's own answer behind a core error (LEGACY-AUDIT-4 B2), in the 4.x
 * platform vocabulary: CoreBluetooth `{domain:'corebluetooth', code:<NSError
 * code>}`, WinRT `{domain:'winrt', code, metadata:{hresult, gattStatus}}`,
 * BlueZ `{domain:'bluez-dbus', code:<D-Bus error name>}`.
 */
export interface DesktopRustCorePlatformDetail {
  readonly domain: string
  readonly code: string
  readonly message: string | null
  readonly metadata: Readonly<Record<string, string | number | boolean>>
  readonly metadataEntries?: readonly PlatformMetadataEntry[]
}

/** The parsed `ubm-napi-error/3` wire form, or null when the text is not one. */
export interface DesktopRustCoreWireError {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly retryability: BleRetryability
  readonly commit: string
  /** The platform detail, when the core carried one. */
  readonly platform: DesktopRustCorePlatformDetail | null
  readonly detail: string
}

export function parseDesktopRustCoreWireError(message: string): DesktopRustCoreWireError | null {
  const parsed = WIRE_FORM.exec(message)
  if (parsed === null) return null
  const [
    ,
    codeText = '',
    domainText = '',
    operation = '',
    retryability = '',
    commit = '',
    platformText = '',
    detail = ''
  ] = parsed
  const code = knownCode(codeText)
  const domain = knownDomain(domainText)
  if (code === undefined || domain === undefined) return null
  const platform = platformText.length === 0 ? null : parsePlatformDetail(platformText)
  if (platform === undefined) return null
  return Object.freeze({
    code,
    domain,
    operation,
    retryability: retryability === 'caller-decides' ? 'caller-decides' : 'never',
    commit,
    platform,
    detail
  })
}

/** The wire's platform JSON, or `undefined` when it is not exactly the frozen shape. */
function parsePlatformDetail(text: string): DesktopRustCorePlatformDetail | undefined {
  if (text.length > 1048576 || utf8ByteLength(text) > 1048576) return undefined
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    return undefined
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return undefined
  const domain: unknown = Reflect.get(value, 'domain')
  const code: unknown = Reflect.get(value, 'code')
  const message: unknown = Reflect.get(value, 'message')
  const metadata: unknown = Reflect.get(value, 'metadata')
  if (typeof domain !== 'string' || domain.length === 0 || typeof code !== 'string' || code.length === 0) {
    return undefined
  }
  if (message !== null && typeof message !== 'string') return undefined
  if (typeof metadata !== 'object' || metadata === null || Array.isArray(metadata)) return undefined
  const entries: Record<string, string | number | boolean> = Object.create(null)
  for (const key of Object.keys(metadata)) {
    const entry: unknown = Reflect.get(metadata, key)
    if (typeof entry !== 'string' && typeof entry !== 'number' && typeof entry !== 'boolean') return undefined
    if (key !== '__proto__' && key !== 'constructor' && key !== 'prototype') entries[key] = entry
  }
  try {
    const metadataEntries = Object.hasOwn(value, 'metadataEntries')
      ? snapshotPlatformMetadataEntries(Reflect.get(value, 'metadataEntries'), 'desktop.platform.metadataEntries')
      : undefined
    return Object.freeze({
      domain,
      code,
      message,
      metadata: Object.freeze(entries),
      ...(metadataEntries === undefined ? {} : { metadataEntries })
    })
  } catch {
    return undefined
  }
}

export function parseDesktopRustCoreLifecyclePlatform(
  text: string | null | undefined
): import('../../backend-contract/errors').PlatformErrorDetail | undefined {
  if (text === null || text === undefined) return undefined
  const detail = parsePlatformDetail(text)
  if (detail === undefined) throw contractError('protocol.malformed', 'boundary', 'desktop.lifecycle.platform')
  return Object.freeze({
    domain: detail.domain,
    code: detail.code,
    safeMessage: detail.message ?? '',
    metadata: detail.metadata,
    ...(detail.metadataEntries === undefined ? {} : { metadataEntries: detail.metadataEntries })
  })
}

export function parseDesktopRustCoreReleaseReport(
  value: unknown,
  expected: { readonly peerId: string; readonly lease: string; readonly connectionGeneration: string | null }
): import('../../backend-contract/errors').PlatformErrorDetail | undefined {
  const malformed = () => contractError('protocol.malformed', 'boundary', 'desktop.connection.release-report')
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw malformed()
  if (Reflect.get(value, 'schema') !== 'ubm-desktop-release/1' || Reflect.get(value, 'state') !== 'released')
    throw malformed()
  for (const key of ['peerId', 'lease', 'connectionGeneration']) {
    if (Reflect.get(value, key) !== Reflect.get(expected, key)) throw malformed()
  }
  const platform: unknown = Reflect.get(value, 'platform')
  if (platform !== undefined && platform !== null && typeof platform !== 'string') throw malformed()
  return parseDesktopRustCoreLifecyclePlatform(platform)
}

/**
 * Reinterpret a dispatch rejection with the exact Rust-issued identity: code,
 * domain, retryability and commit state cross unchanged (the core decided
 * retryability from the settled outcome, so a dispatched write that may have
 * committed stays `never` whatever its code says). The public operation is
 * `operation`, the caller's 4.x id ({@link desktopRustCoreOperation}); the
 * core's internal operation rides the core detail. A rejection that is
 * already a contract error returns untouched; anything else reports
 * `platform.transport` loudly rather than inventing a core identity.
 */
export function desktopRustCoreError(error: unknown, operation: string): BackendContractError {
  if (error instanceof BackendContractError) return error
  const message = errorMessage(error)
  const wire = parseDesktopRustCoreWireError(message)
  if (wire !== null) {
    // The core's own operation stays internal: it rides the core detail for
    // diagnosis, and the public id is the caller's 4.x operation.
    const coreRecord: Record<string, string> = {}
    if (wire.commit.length > 0) coreRecord.commit = wire.commit
    if (wire.operation.length > 0 && wire.operation !== operation) coreRecord.coreOperation = wire.operation
    const metadata: SerializableRecord = Object.freeze(coreRecord)
    // The OS's answer, when the core carried one, is the error's platform
    // identity exactly as the 4.x backend reported it; otherwise the core's
    // own detail rides as `desktop-rust-core` / `core-detail`.
    const platform =
      wire.platform !== null
        ? {
            domain: wire.platform.domain,
            code: wire.platform.code,
            safeMessage: wire.platform.message ?? wire.detail,
            metadata: wire.platform.metadata,
            ...(wire.platform.metadataEntries === undefined ? {} : { metadataEntries: wire.platform.metadataEntries })
          }
        : wire.detail.length > 0 || Object.keys(coreRecord).length > 0
          ? { domain: 'desktop-rust-core', code: 'core-detail', safeMessage: wire.detail, metadata }
          : null
    const base = contractError(wire.code, wire.domain, operation, platform)
    const commit = commitUncertainty(wire.commit)
    return new BackendContractError({
      ...base.normalized,
      retryability: wire.retryability,
      ...(commit === undefined ? {} : { commit })
    })
  }
  return contractError('platform.transport', 'platform', operation, {
    domain: 'desktop-rust-core',
    code: 'binding-rejection',
    safeMessage: message.slice(0, 512),
    metadata: Object.freeze({})
  })
}

/**
 * The core's commit state in the contract's uncertainty vocabulary: a write
 * that may have reached the peer is `uncertain`, one refused before the
 * radio is `not-dispatched`. Committed/released outcomes are not
 * uncertainties, so no field is set.
 */
function commitUncertainty(commit: string): BleCommitUncertainty | undefined {
  if (commit === 'unknown') return 'uncertain'
  if (commit === 'not-dispatched') return 'not-dispatched'
  return undefined
}

/** Throwing form of {@link desktopRustCoreError} for `catch` blocks. */
export function throwDesktopRustCoreError(error: unknown, fallbackOperation: string): never {
  throw desktopRustCoreError(error, fallbackOperation)
}

/** Map the core's shutdown report onto the contract cleanup record, failures intact. */
export function cleanupRecordFromCloseReport(report: DesktopRustCoreCloseReport, operation: string): CleanupRecord {
  if (report.state === 'released' && report.failures.length === 0) {
    return Object.freeze({ state: 'released', failures: Object.freeze([]) })
  }
  const failures = report.failures.map(failure =>
    Object.freeze({
      resourceKind: failure.resourceKind,
      error: desktopRustCoreError(new Error(failure.error), operation).normalized
    })
  )
  if (failures.length === 0) {
    failures.push(
      Object.freeze({
        resourceKind: 'central',
        error: contractError('lifecycle.invariant-violation', 'cleanup', operation).normalized
      })
    )
  }
  return Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
}
