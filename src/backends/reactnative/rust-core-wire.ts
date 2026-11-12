// src/backends/reactnative/rust-core-wire.ts
// React Native operation wire validation and shared recording wire exports.
import { decodeOrderedMarkerWriteStatus, type OrderedMarkerWriteStatus } from '../../core/ordered-marker-wire'
import {
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  BLE_RETRYABILITIES,
  contractError,
  type BleErrorCode,
  type BleErrorDomain
} from '../../backend-contract/errors'
import { type BlePeerState, type PeerSource, type ResourceCounters } from '../../backend-contract/backend'
import {
  READ_PROVENANCES,
  type CancellationAcknowledgement,
  type ReadProvenance,
  type WriteMode
} from '../../backend-contract/operations'
import {
  type WireResult,
  DELIVERY_KINDS,
  type WireDelivery,
  STREAM_END_REASONS,
  LINK_REASONS,
  INGRESS_CLASSES,
  type WirePlatformDetail,
  type WireRemoteFailure,
  type WireInvokeEnvelope,
  type WireAdapterState,
  type WireSecurityState,
  type WireRestoredPeer,
  type WireDrainRecord as SharedDrainRecord,
  operationName,
  malformed,
  capture,
  decodeBase64OrThrow,
  terminalEvidenceOrThrow,
  assertTextWithinBound,
  parseWireTextOrThrow,
  isPlainObject,
  exactObject,
  arrayOf,
  nullable,
  type IntegerRange,
  NON_NEGATIVE,
  COMPANY_ID,
  SIGNED_BYTE,
  integerOrThrow,
  enumOrThrow,
  stringOrThrow,
  optionalTextOrThrow,
  booleanOrThrow,
  uuidOrThrow,
  platformDetailOrThrow,
  remoteFailureOrThrow,
  invokeEnvelopeOrThrow,
  adapterStateOrThrow,
  securityStateOrThrow,
  restoredPeerOrThrow,
  drainBatchOrThrow,
  drainRecordOrThrow
} from '../../core/recording-wire'
export {
  MAX_OPERATION_BYTES,
  MAX_BASE64_LENGTH,
  MAX_WIRE_TEXT_BYTES,
  type WireResult,
  DELIVERY_KINDS,
  type WireDelivery,
  COMMIT_STATES,
  type WireCommit,
  SCAN_END_REASONS,
  STREAM_END_REASONS,
  LINK_REASONS,
  INGRESS_CLASSES,
  type WirePlatformDetail,
  type WireRemoteFailure,
  type WireInvokeEnvelope,
  type WireAdapterState,
  type WireSecurityState,
  type WireRestoredPeer,
  type WireAdvertisementRecord,
  encodeBase64,
  parseNativeControlEnvelope,
  parseRecordingControlEnvelope,
  remotePlatformDetail,
  remoteFailureError,
  failureEnvelopeError
} from '../../core/recording-wire'

/**
 * A producer may implement `negotiateScanEndVersion(sessionId, '[1,2]')` and
 * atomically return JSON `{ "version": 1 | 2 }` for that session. A missing
 * method means v1. Only a selected v2 producer emits this extension, with its
 * exact scan membership and start identities. Generation is a non-secret,
 * canonical decimal counter, never a capability token. Native error strings
 * and metadata must be safe for application-visible diagnostics.
 */
export type WireScanEndV2 = {
  readonly t: 'scan-end'
  readonly ordinal: number
  readonly operationId: string
  readonly startOperationId: string
  readonly reason: 'source-failed'
  readonly version: 2
  readonly source: 'adapter' | 'radio'
  readonly generation: string
  readonly error: import('../../core/recording-wire').WireRemoteFailure & {
    readonly retryability: import('../../backend-contract/errors').BleRetryability
  }
  readonly evidence?: import('../../backend-contract/terminal-evidence').TerminalEvidenceCommit
}
export type WireDrainRecord = SharedDrainRecord | WireScanEndV2
export interface WireDrainBatch {
  readonly more: boolean
  readonly records: readonly WireDrainRecord[]
  readonly controlLost: number
}

/** The producer selects exactly one mutually supported session version. */
export function parseScanEndVersion(text: unknown): WireResult<number> {
  return capture(() => {
    const fields = exactObject(parseWireTextOrThrow(text, 'scan-end-version'), ['version'], 'scan-end-version')
    const version = integerOrThrow(fields.get('version'), NON_NEGATIVE, 'scan-end-version.version')
    if (version !== 1 && version !== 2) throw contractError('protocol.incompatible', 'boundary', 'scan-end-version')
    return version
  })
}

/** Strict mobile extension of the shared legacy drain codec. */
export function parseDrainText(
  text: unknown,
  lastOrdinal: number | null,
  scanEndVersion = 1
): WireResult<WireDrainBatch> {
  return capture(() => {
    if (scanEndVersion !== 1 && scanEndVersion !== 2) throw malformed('drain.scan-end.version')
    const value = parseWireTextOrThrow(text, 'drain')
    if (scanEndVersion === 1) return drainBatchOrThrow(value, lastOrdinal, 'drain')
    const fields = exactObject(value, ['more', 'records', 'controlLost'], 'drain')
    const records = arrayOf(fields.get('records'), 'drain.records', (entry, path): WireDrainRecord => {
      if (typeof entry !== 'object' || entry === null || Array.isArray(entry) || !Object.hasOwn(entry, 'version')) {
        const legacy = drainRecordOrThrow(entry, path)
        if (legacy.t === 'scan-end' && legacy.reason === 'source-failed') throw malformed(path)
        return legacy
      }
      const hasEvidence = typeof entry === 'object' && entry !== null && Object.hasOwn(entry, 'evidence')
      const f = exactObject(
        entry,
        [
          't',
          'ordinal',
          'operationId',
          'startOperationId',
          'reason',
          'version',
          'source',
          'generation',
          'error',
          ...(hasEvidence ? ['evidence'] : [])
        ],
        path
      )
      if (f.get('t') !== 'scan-end' || f.get('version') !== 2 || f.get('reason') !== 'source-failed')
        throw malformed(path)
      const generation = stringOrThrow(f.get('generation'), `${path}.generation`)
      if (!/^(0|[1-9][0-9]{0,19})$/.test(generation)) throw malformed(`${path}.generation`)
      const errorFields = exactObject(
        f.get('error'),
        ['code', 'domain', 'operation', 'detail', 'platform', 'retryability'],
        `${path}.error`
      )
      const error = remoteFailureOrThrow(
        {
          code: errorFields.get('code'),
          domain: errorFields.get('domain'),
          operation: errorFields.get('operation'),
          detail: errorFields.get('detail'),
          platform: errorFields.get('platform')
        },
        `${path}.error`
      )
      if (
        error.platform !== null &&
        (Object.keys(error.platform.metadata).some(key => key.startsWith('ubm.scan.')) ||
          error.platform.metadataEntries?.some(metadataEntry => metadataEntry.key.startsWith('ubm.scan.')) === true)
      )
        throw malformed(`${path}.error.platform.metadata`)
      return Object.freeze({
        t: 'scan-end',
        ordinal: integerOrThrow(f.get('ordinal'), NON_NEGATIVE, `${path}.ordinal`),
        operationId: stringOrThrow(f.get('operationId'), `${path}.operationId`),
        startOperationId: stringOrThrow(f.get('startOperationId'), `${path}.startOperationId`),
        reason: 'source-failed',
        version: 2,
        source: enumOrThrow(f.get('source'), ['adapter', 'radio'] as const, `${path}.source`),
        generation,
        error: Object.freeze({
          ...error,
          retryability: enumOrThrow(errorFields.get('retryability'), BLE_RETRYABILITIES, `${path}.error.retryability`)
        }),
        ...(f.get('evidence') === undefined
          ? {}
          : { evidence: terminalEvidenceOrThrow(f.get('evidence'), `${path}.evidence`) })
      })
    })
    let previous = lastOrdinal
    records.forEach((record, index) => {
      if (previous !== null && record.ordinal <= previous) throw malformed(`drain.records[${index}].ordinal`)
      previous = record.ordinal
    })
    return Object.freeze({
      more: booleanOrThrow(fields.get('more'), 'drain.more'),
      records,
      controlLost: integerOrThrow(fields.get('controlLost'), NON_NEGATIVE, 'drain.controlLost')
    })
  })
}

export const WIRE_REVISION = 'ubm-mobile-wire/3'

export const GATT_PROPERTY_MASK = 0x1f

export const WIRE_OPS = Object.freeze([
  'adapter.state',
  'counters.describe',
  'terminal-evidence.reserve',
  'terminal-evidence.commit',
  'terminal-evidence.read',
  'terminal-evidence.release',
  'scan.start',
  'scan.stop',
  'peers.resolve',
  'peers.known',
  'peers.connected',
  'peers.bonded',
  'peers.restored',
  'peers.claim-restored',
  'connection.connect',
  'connection.disconnect',
  'connection.rssi',
  'connection.effective-mtu',
  'connection.request-mtu',
  'connection.request-priority',
  'connection.request-subrate',
  'connection.control-capabilities',
  'connection.read-phy',
  'connection.request-phy',
  'connection.maximum-write-length',
  'connection.write-readiness',
  'security.state',
  'security.pair',
  'security.cancel-pairing',
  'background.acquire',
  'background.release',
  'background.update-notification',
  'companion.associate',
  'companion.list',
  'companion.disassociate',
  'presence.observe',
  'presence.unobserve',
  'gatt.discover',
  'gatt.ordered.version',
  'gatt.ordered.failure-metadata.version',
  'gatt.ordered.capacity.version',
  'gatt.ordered.increase-capacity',
  'gatt.ordered.recording-reclaim.version',
  'gatt.ordered.recording-reclaim.next',
  'gatt.ordered.recording-reclaim.reserve',
  'gatt.ordered.recording-reclaim.start',
  'gatt.ordered.recording-reclaim.status',
  'gatt.ordered.recording-reclaim.commit',
  'gatt.ordered.recording-reclaim.cancel',
  'gatt.ordered.recording-reclaim.release',
  'gatt.ordered.recording-reclaim.retire',
  'gatt.ordered.recording-reclaim.ack-retire',
  'gatt.ordered.recording-reclaim.forget-retire',
  'gatt.ordered.recording-reclaim.ack-forget',
  'gatt.ordered.marker.version',
  'gatt.ordered.marker.reserve',
  'gatt.ordered.marker.start',
  'gatt.ordered.marker.status',
  'gatt.ordered.marker.cancel',
  'gatt.ordered.marker.forget',
  'gatt.ordered.reserve',
  'gatt.ordered.reclaim',
  'gatt.ordered.open',
  'gatt.ordered.status',
  'gatt.ordered.poll',
  'gatt.ordered.ack-delivery',
  'gatt.ordered.discard',
  'gatt.ordered.poll-discard',
  'gatt.ordered.ack-discard',
  'gatt.ordered.close',
  'gatt.ordered.ack-group',
  'gatt.ordered.forget-group',
  'gatt.read',
  'gatt.read-descriptor',
  'gatt.write',
  'gatt.write-when-ready',
  'gatt.write-descriptor',
  'gatt.subscribe',
  'gatt.subscribe-recorded',
  'gatt.unsubscribe',
  'op.cancel',
  'session.reconcile',
  'session.dispose'
] as const)

export type WireOp = (typeof WIRE_OPS)[number]

const EFFECTFUL_OPS: readonly WireOp[] = Object.freeze([
  'gatt.write',
  'gatt.write-when-ready',
  'gatt.write-descriptor',
  'connection.request-mtu',
  'connection.request-priority',
  'connection.request-subrate',
  'connection.request-phy'
])

export const WRITE_COMMIT_STATES = Object.freeze(['confirmed', 'unknown'] as const)

export type WireWriteCommitState = (typeof WRITE_COMMIT_STATES)[number]

const PEER_SOURCES = Object.freeze([
  'scan-observed',
  'app-reference',
  'system-connected',
  'system-bonded',
  'origin-authorized',
  'restored',
  'backend-cache'
] as const satisfies readonly PeerSource[])

const PEER_REACHABILITY = Object.freeze([
  'reachable',
  'unreachable',
  'unknown'
] as const satisfies readonly BlePeerState['reachability'][])

const PEER_BOND = Object.freeze([
  'bonded',
  'not-bonded',
  'unknown',
  'unsupported'
] as const satisfies readonly BlePeerState['bond'][])

export const PHYS = Object.freeze(['le-1m', 'le-2m', 'le-coded'] as const)

export type WirePhy = (typeof PHYS)[number]

const PAIR_OUTCOMES = Object.freeze(['paired', 'already-paired', 'rejected'] as const)

const ATT_MTU: IntegerRange = { min: 23, max: 517 }

const ATT_WRITE_LENGTH: IntegerRange = { min: 1, max: 512 }

const PEER_CONNECTION = Object.freeze([
  'connected',
  'disconnected',
  'unknown'
] as const satisfies readonly BlePeerState['connection'][])

const CANCELLATION_STATES = Object.freeze([
  'cancellation-requested',
  'already-terminal',
  'not-cancellable'
] as const satisfies readonly CancellationAcknowledgement<string>['state'][])

const CLEANUP_STATES = Object.freeze(['released', 'release-failed'] as const)

export const DATABASE_STATES = Object.freeze(['undiscovered', 'discovering', 'current', 'changed', 'invalid'] as const)

export type WireResourceCounters = { readonly [Key in keyof ResourceCounters]: number }

export interface WireCounters {
  readonly counters: WireResourceCounters
  readonly native: {
    readonly pendingRadioRequests: number
    readonly liveOps: number
  }
  readonly process: {
    readonly counters: WireResourceCounters
    readonly native: {
      readonly nativeGattAdmissions: number
      readonly acquiredGattTransports: number
      readonly pendingGattAcquisitions: number
      readonly pendingRadioRequests: number
      readonly lateRadioCompletions: number
      readonly ingressDrops: { readonly advertisement: number; readonly notification: number; readonly control: number }
      readonly connectSections: number
      readonly liveOps: number
    }
  }
}

export interface WirePeerRecord {
  readonly peerId: string
  /** The restored/observed name the owner holds, or null when it holds none (Android presence wake restores a bare address). */
  readonly name: string | null
  readonly rssi: number | null
  readonly source: PeerSource
  readonly reachability: BlePeerState['reachability']
  readonly connection: BlePeerState['connection']
  readonly bond: BlePeerState['bond']
  readonly lastSeenAtMonotonicMs: number | null
}

export interface WirePhyObservation {
  readonly tx: WirePhy
  readonly rx: WirePhy
}

export type WireReconcileLink =
  | {
      readonly peerId: string
      readonly connectionGeneration: string
      readonly state: 'connected'
      readonly reason: null
      readonly databaseGeneration: string | null
      /** The database generation the latest `db-changed` on this link invalidated. */
      readonly databaseChange: string | null
      readonly databaseState: (typeof DATABASE_STATES)[number] | null
    }
  | {
      readonly peerId: string
      readonly connectionGeneration: string
      readonly state: 'ended'
      readonly reason: (typeof LINK_REASONS)[number]
      readonly databaseGeneration: string | null
      readonly databaseChange: null
      readonly databaseState: null
    }

export type WireReconcileSubscription =
  | { readonly consumer: string; readonly state: 'live' }
  | {
      readonly consumer: string
      readonly state: 'ended'
      readonly error?: WireRemoteFailure
      readonly reason: (typeof STREAM_END_REASONS)[number]
      readonly droppedItems: number
      readonly droppedBytes: number
    }

export interface WireReconcile {
  readonly adapter: WireAdapterState
  readonly links: readonly WireReconcileLink[]
  readonly subscriptions: readonly WireReconcileSubscription[]
  readonly security: readonly { readonly peerId: string; readonly state: WireSecurityState }[]
  readonly securityFailures: readonly { readonly peerId: string | null; readonly error: WireRemoteFailure }[]
  readonly restored: readonly WireRestoredPeer[]
  /** This session's scan membership, or `null` when it holds none. */
  readonly scan: string | null
}

export interface WireCleanupFailure {
  readonly resourceKind: string
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly detail: string | null
  readonly platform: WirePlatformDetail | null
}

export interface WireCleanupRecord {
  readonly state: (typeof CLEANUP_STATES)[number]
  readonly failures: readonly WireCleanupFailure[]
}

export interface WireDescriptor {
  readonly uuid: string
  readonly occurrence: number
}

export interface WireCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly properties: number
  readonly descriptors: readonly WireDescriptor[]
}

export interface WireService {
  readonly primary: boolean | null
  readonly includedServices: readonly WireDescriptor[] | null
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: readonly WireCharacteristic[]
}

export interface WireCompanionRecord {
  readonly associationId: number
  readonly peerId: string | null
  readonly displayName: string | null
}

export interface WireDiscovery {
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  /**
   * The whole database: the owner registers every attribute or fails the
   * discovery with a typed error (a malformed platform UUID is
   * `protocol.malformed`, a database past the ATT handle space
   * `capability.limited`); nothing is skipped.
   */
  readonly services: readonly WireService[]
}

export interface WireRecordingReclaimStatus {
  readonly state:
    | 'reserved'
    | 'pending'
    | 'ready'
    | 'committed'
    | 'releasing'
    | 'release-failed'
    | 'released'
    | 'failed'
    | 'retired'
  readonly error: WireRemoteFailure | null
  readonly mappingCommitted: boolean
}
export interface WireRecordingReclaimRefusal {
  readonly state: 'not-reserved'
  readonly error: WireRemoteFailure
}
export interface WireOrderedReclaimContext {
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly publicConnectionGeneration: string
  readonly publicDatabaseGeneration: string
  readonly maxItems: number
  readonly maxBytes: number
  readonly maxControls: number
  readonly members: readonly {
    readonly memberId: string
    readonly publicMemberId: string
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
    readonly deliveryMode: 'notification' | 'indication' | null
    readonly preferredDeliveryMode: 'notification' | 'indication' | null
  }[]
}
export interface WireOrderedReceipt {
  readonly receiptId: string
  readonly discardedDeliveryId: string
  readonly evidence: string
}
export interface WireOrderedStatus {
  readonly sourceGeneration: { readonly kind: 'counter' | 'opaque'; readonly value: string } | null
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
    readonly effectiveDelivery: WireDelivery
    readonly cleanupReleased: boolean
    readonly cleanupError: WireRemoteFailure | null
  }[]
  readonly cleanupComplete: boolean
  readonly setupError: WireRemoteFailure | null
  readonly fenceError: WireRemoteFailure | null
}

export interface WireOpResults {
  readonly 'adapter.state': WireAdapterState
  readonly 'counters.describe': WireCounters
  readonly 'terminal-evidence.reserve': { readonly maximumBytes: number; readonly reservationId: string }
  readonly 'terminal-evidence.commit':
    | {
        readonly kind: 'committed'
        readonly ref: string
        readonly byteLength: number
        readonly digest: string
        readonly formatVersion: 1
        readonly durability: 'committed'
      }
    | {
        readonly kind: 'refused'
        readonly reason: 'quota-exhausted' | 'storage-unavailable' | 'capability-unsupported'
      }
  readonly 'terminal-evidence.read': { readonly canonicalBytesBase64: string; readonly digest: string }
  /**
   * Runtime release abandons a reservation; committed evidence remains
   * recoverable until the distinct opaque-handle recovery acknowledgement.
   */
  readonly 'terminal-evidence.release': { readonly released: true; readonly retainedForRecovery: boolean }
  readonly 'scan.start': { readonly operationId: string }
  readonly 'scan.stop': WireCleanupRecord
  readonly 'peers.resolve': WirePeerRecord | null
  readonly 'peers.known': readonly WirePeerRecord[]
  readonly 'peers.connected': readonly WirePeerRecord[]
  readonly 'peers.bonded': readonly WirePeerRecord[]
  readonly 'peers.restored': readonly WirePeerRecord[]
  /** The restored peers this session adopted; empty once another adopter in the process claimed them. */
  readonly 'peers.claim-restored': { readonly peers: readonly WirePeerRecord[] }
  readonly 'connection.connect': { readonly peerKey: string; readonly connectionGeneration: string }
  readonly 'connection.disconnect': WireCleanupRecord
  readonly 'connection.rssi': { readonly rssi: number }
  readonly 'connection.effective-mtu': { readonly mtu: number | null }
  readonly 'connection.request-mtu': { readonly mtu: number }
  readonly 'connection.request-priority': { readonly accepted: boolean }
  readonly 'connection.request-subrate': { readonly accepted: boolean }
  readonly 'connection.control-capabilities': { readonly subrate: boolean }
  readonly 'connection.read-phy': WirePhyObservation
  readonly 'connection.request-phy': { readonly accepted: boolean; readonly observation: WirePhyObservation | null }
  /** The platform's largest single write in the requested mode, bounded by the ATT maximum attribute value. */
  readonly 'connection.maximum-write-length': { readonly maximumWriteLength: number }
  /** CoreBluetooth `canSendWriteWithoutResponse`. Android answers `capability.unsupported`. */
  readonly 'connection.write-readiness': { readonly ready: boolean }
  readonly 'security.state': WireSecurityState
  readonly 'security.pair': { readonly outcome: (typeof PAIR_OUTCOMES)[number]; readonly state: WireSecurityState }
  readonly 'security.cancel-pairing': { readonly state: 'requested' }
  readonly 'background.acquire': { readonly leaseId: string }
  readonly 'background.release': WireCleanupRecord
  readonly 'background.update-notification': { readonly state: 'updated' }
  readonly 'presence.observe': { readonly state: 'observing' }
  readonly 'presence.unobserve': { readonly state: 'idle' }
  readonly 'companion.associate': {
    readonly source: 'associated' | 'already-associated'
    readonly associationId: number
    readonly peerId: string | null
    readonly displayName: string | null
  }
  readonly 'companion.list': {
    readonly associations: readonly WireCompanionRecord[]
  }
  readonly 'companion.disassociate': {
    readonly state: 'disassociated'
    readonly associationId: number
  }
  readonly 'gatt.discover': WireDiscovery
  readonly 'gatt.ordered.recording-reclaim.version': { readonly wireVersion: number }
  /** Requests may include callerKey to replay that caller's retained ID. */
  readonly 'gatt.ordered.recording-reclaim.next': { readonly requestId: string } | WireRecordingReclaimRefusal
  readonly 'gatt.ordered.recording-reclaim.reserve': WireRecordingReclaimStatus | WireRecordingReclaimRefusal
  readonly 'gatt.ordered.recording-reclaim.start': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.status': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.commit': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.cancel': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.release': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.retire': WireRecordingReclaimStatus
  readonly 'gatt.ordered.recording-reclaim.ack-retire': { readonly state: 'acknowledged' }
  readonly 'gatt.ordered.recording-reclaim.forget-retire': { readonly state: 'forgotten' }
  readonly 'gatt.ordered.recording-reclaim.ack-forget': { readonly state: 'acknowledged' | 'absent' }
  readonly 'gatt.ordered.marker.version': { readonly wireVersion: number }
  readonly 'gatt.ordered.marker.reserve': { readonly handle: string }
  readonly 'gatt.ordered.marker.start': OrderedMarkerWriteStatus
  readonly 'gatt.ordered.marker.status': OrderedMarkerWriteStatus
  readonly 'gatt.ordered.marker.cancel': OrderedMarkerWriteStatus
  readonly 'gatt.ordered.marker.forget': null
  readonly 'gatt.ordered.version': { readonly wireVersion: number }
  readonly 'gatt.ordered.capacity.version': { readonly wireVersion: number; readonly maximumMaxBytes: number }
  readonly 'gatt.ordered.increase-capacity': {
    readonly previousMaxBytes: number
    readonly maxBytes: number
    readonly maximumMaxBytes: number
  }
  readonly 'gatt.ordered.failure-metadata.version': { readonly versions: readonly number[] }
  readonly 'gatt.ordered.reserve': { readonly collectionId: string; readonly reclaimCapability: string }
  readonly 'gatt.ordered.reclaim': WireOrderedReclaimContext | { readonly recordingId: string } | null
  readonly 'gatt.ordered.open': null
  readonly 'gatt.ordered.status': WireOrderedStatus
  readonly 'gatt.ordered.poll': { readonly deliveryId: string | null; readonly record: string }
  readonly 'gatt.ordered.ack-delivery': null
  readonly 'gatt.ordered.discard': WireOrderedReceipt
  readonly 'gatt.ordered.poll-discard': WireOrderedReceipt | null
  readonly 'gatt.ordered.ack-discard': null
  readonly 'gatt.ordered.close': 'released' | 'pending-source' | 'pending-drain'
  readonly 'gatt.ordered.ack-group': null
  readonly 'gatt.ordered.forget-group': null
  readonly 'gatt.read': { readonly value: Uint8Array; readonly provenance: ReadProvenance }
  readonly 'gatt.read-descriptor': { readonly value: Uint8Array }
  readonly 'gatt.write': { readonly commitState: WireWriteCommitState }
  readonly 'gatt.write-when-ready': { readonly commitState: WireWriteCommitState }
  readonly 'gatt.write-descriptor': { readonly commitState: WireWriteCommitState }
  readonly 'gatt.subscribe': { readonly consumer: string; readonly delivery: WireDelivery }
  readonly 'gatt.subscribe-recorded': { readonly consumer: string; readonly delivery: WireDelivery }
  readonly 'gatt.unsubscribe': { readonly state: 'released'; readonly physicalDisabled: boolean }
  readonly 'op.cancel': { readonly state: CancellationAcknowledgement<string>['state'] }
  readonly 'session.reconcile': WireReconcile
  readonly 'session.dispose': WireCleanupRecord
}

export type WireJson = string | number | boolean | null | readonly WireJson[] | { readonly [key: string]: WireJson }

export type WireJsonObject = { readonly [key: string]: WireJson }

export function decodeBase64(text: unknown, path: string): WireResult<Uint8Array> {
  return capture(() => decodeBase64OrThrow(text, path))
}

export function parseWireText(text: unknown, path: string): WireResult<unknown> {
  return capture(() => parseWireTextOrThrow(text, path))
}

function assertWireJson(value: unknown, path: string): void {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw contractError('argument.invalid', 'core', operationName(path))
    return
  }
  if (Array.isArray(value)) {
    value.forEach((entry: unknown, index: number) => assertWireJson(entry, `${path}[${index}]`))
    return
  }
  if (typeof value === 'object' && isPlainObject(value)) {
    for (const [key, entry] of Object.entries(value)) assertWireJson(entry, `${path}.${key}`)
    return
  }
  throw contractError('argument.invalid', 'core', operationName(path))
}

export function serializeInvokeArgs(args: WireJsonObject): WireResult<string> {
  return capture(() => {
    if (typeof args !== 'object' || args === null || Array.isArray(args) || !isPlainObject(args)) {
      throw contractError('argument.invalid', 'core', operationName('args'))
    }
    assertWireJson(args, 'args')
    const text = JSON.stringify(args)
    assertTextWithinBound(text, 'args', 'core')
    return text
  })
}

function propertyMaskOrThrow(value: unknown, path: string): number {
  const mask = integerOrThrow(value, { min: 0, max: 0xff }, path)
  if ((mask & ~GATT_PROPERTY_MASK) !== 0) throw malformed(path)
  return mask
}

function assertUniqueInstances(items: readonly { readonly uuid: string; readonly occurrence: number }[], path: string) {
  const seen = new Set<string>()
  for (const item of items) {
    const key = `${item.uuid}#${item.occurrence}`
    if (seen.has(key)) throw malformed(path)
    seen.add(key)
  }
}

export function parseOccurrence(value: unknown, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, NON_NEGATIVE, path))
}

export function parseSafeInteger(value: unknown, range: IntegerRange, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, range, path))
}

export function parseCompanyId(value: unknown, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, COMPANY_ID, path))
}

export function parsePropertyMask(value: unknown, path: string): WireResult<number> {
  return capture(() => propertyMaskOrThrow(value, path))
}

export function parseEnum<Member extends string>(
  value: unknown,
  members: readonly Member[],
  path: string
): WireResult<Member> {
  return capture(() => enumOrThrow(value, members, path))
}

export function parseGeneration(value: unknown, path: string): WireResult<string> {
  return capture(() => stringOrThrow(value, path))
}

function wireOpOrThrow(op: unknown, path: string): WireOp {
  const member = WIRE_OPS.find(candidate => candidate === op)
  if (member === undefined) throw contractError('argument.invalid', 'core', operationName(path))
  return member
}

export function parseInvokeEnvelope(text: unknown, op: WireOp): WireResult<WireInvokeEnvelope> {
  return capture(() => {
    const knownOp = wireOpOrThrow(op, 'envelope.op')
    return invokeEnvelopeOrThrow(text, `${knownOp}.envelope`, EFFECTFUL_OPS.includes(knownOp))
  })
}

export function parseRemoteFailureText(text: unknown, path: string): WireResult<WireRemoteFailure> {
  return capture(() => remoteFailureOrThrow(parseWireTextOrThrow(text, path), path))
}

export interface WireAdmission {
  /** The session id as the decimal string every later native call takes. */
  readonly sessionId: string
  readonly contractRevision: string
  readonly wireRevision: string
  /** The binding's `ubm-native-build-identity/1` record, still unparsed. */
  readonly buildIdentity: unknown
}

export function parseAdmissionText(text: unknown): WireResult<WireAdmission> {
  return capture(() => {
    const path = 'admission'
    const fields = exactObject(
      parseWireTextOrThrow(text, path),
      ['sessionId', 'contractRevision', 'wireRevision', 'buildIdentity'],
      path
    )
    return Object.freeze({
      sessionId: String(integerOrThrow(fields.get('sessionId'), NON_NEGATIVE, `${path}.sessionId`)),
      contractRevision: stringOrThrow(fields.get('contractRevision'), `${path}.contractRevision`),
      wireRevision: stringOrThrow(fields.get('wireRevision'), `${path}.wireRevision`),
      buildIdentity: fields.get('buildIdentity')
    })
  })
}

export const MAX_RANDOM_BYTES = 1024

export function checkRandomByteLength(length: unknown): WireResult<number> {
  return capture(() => {
    if (typeof length !== 'number' || !Number.isSafeInteger(length) || length < 1 || length > MAX_RANDOM_BYTES) {
      throw contractError('argument.invalid', 'core', operationName('random-bytes.length'))
    }
    return length
  })
}

export function parseRandomBytesText(text: unknown, length: number): WireResult<Uint8Array> {
  return capture(() => {
    const bytes = decodeBase64OrThrow(text, 'random-bytes')
    if (bytes.byteLength !== length) throw malformed('random-bytes.length')
    return bytes
  })
}

export interface WireRestorationIdentity {
  readonly applicationId: string
  readonly restorationId: string
  readonly generation: string
  readonly restoreIdentifier: string
  readonly namespaceValue: string
  readonly clientId: string
  readonly hostSessionScope: string
}

const RESTORATION_IDENTITY_KEYS = Object.freeze([
  'applicationId',
  'restorationId',
  'generation',
  'restoreIdentifier',
  'namespaceValue',
  'clientId',
  'hostSessionScope'
] as const)

export function parseRestorationIdentityText(text: unknown): WireResult<WireRestorationIdentity> {
  return capture(() => {
    const path = 'restoration-identity'
    const fields = exactObject(parseWireTextOrThrow(text, path), RESTORATION_IDENTITY_KEYS, path)
    const read = (key: (typeof RESTORATION_IDENTITY_KEYS)[number]): string =>
      stringOrThrow(fields.get(key), `${path}.${key}`)
    return Object.freeze({
      applicationId: read('applicationId'),
      restorationId: read('restorationId'),
      generation: read('generation'),
      restoreIdentifier: read('restoreIdentifier'),
      namespaceValue: read('namespaceValue'),
      clientId: read('clientId'),
      hostSessionScope: read('hostSessionScope')
    })
  })
}

export function parseConfiguredRestorationIdentityText(text: unknown): WireResult<WireRestorationIdentity | null> {
  if (text === 'null') return Object.freeze({ ok: true, value: null })
  return parseRestorationIdentityText(text)
}

const RESOURCE_COUNTER_KEYS = Object.freeze([
  'activeScanControllers',
  'scanConsumers',
  'chooserSessions',
  'connectionLeases',
  'physicalLinks',
  'databaseSnapshots',
  'physicalCccdEnablements',
  'subscriptionConsumers',
  'queuedOperations',
  'dispatchedOperations',
  'retainedByteBuffers',
  'restorationRecords',
  'orphanedIpcOwners'
] as const satisfies readonly (keyof ResourceCounters)[])

function resourceCountersOrThrow(value: unknown, path: string): WireResourceCounters {
  const counters = exactObject(value, RESOURCE_COUNTER_KEYS, path)
  const count = (key: (typeof RESOURCE_COUNTER_KEYS)[number]): number =>
    integerOrThrow(counters.get(key), NON_NEGATIVE, `${path}.${key}`)
  return Object.freeze({
    activeScanControllers: count('activeScanControllers'),
    scanConsumers: count('scanConsumers'),
    chooserSessions: count('chooserSessions'),
    connectionLeases: count('connectionLeases'),
    physicalLinks: count('physicalLinks'),
    databaseSnapshots: count('databaseSnapshots'),
    physicalCccdEnablements: count('physicalCccdEnablements'),
    subscriptionConsumers: count('subscriptionConsumers'),
    queuedOperations: count('queuedOperations'),
    dispatchedOperations: count('dispatchedOperations'),
    retainedByteBuffers: count('retainedByteBuffers'),
    restorationRecords: count('restorationRecords'),
    orphanedIpcOwners: count('orphanedIpcOwners')
  })
}

function countersOrThrow(value: unknown, path: string): WireCounters {
  const fields = exactObject(value, ['counters', 'native', 'process'], path)
  const nativePath = `${path}.native`
  const native = exactObject(fields.get('native'), ['pendingRadioRequests', 'liveOps'], nativePath)
  const processPath = `${path}.process`
  const owner = exactObject(fields.get('process'), ['counters', 'native'], processPath)
  const processNativePath = `${processPath}.native`
  const processNative = exactObject(
    owner.get('native'),
    [
      'nativeGattAdmissions',
      'acquiredGattTransports',
      'pendingGattAcquisitions',
      'pendingRadioRequests',
      'lateRadioCompletions',
      'ingressDrops',
      'connectSections',
      'liveOps'
    ],
    processNativePath
  )
  const dropsPath = `${processNativePath}.ingressDrops`
  const drops = exactObject(processNative.get('ingressDrops'), INGRESS_CLASSES, dropsPath)
  const nonNegative = (entry: unknown, entryPath: string): number => integerOrThrow(entry, NON_NEGATIVE, entryPath)
  return Object.freeze({
    counters: resourceCountersOrThrow(fields.get('counters'), `${path}.counters`),
    native: Object.freeze({
      pendingRadioRequests: nonNegative(native.get('pendingRadioRequests'), `${nativePath}.pendingRadioRequests`),
      liveOps: nonNegative(native.get('liveOps'), `${nativePath}.liveOps`)
    }),
    process: Object.freeze({
      counters: resourceCountersOrThrow(owner.get('counters'), `${processPath}.counters`),
      native: Object.freeze({
        nativeGattAdmissions: nonNegative(
          processNative.get('nativeGattAdmissions'),
          `${processNativePath}.nativeGattAdmissions`
        ),
        acquiredGattTransports: nonNegative(
          processNative.get('acquiredGattTransports'),
          `${processNativePath}.acquiredGattTransports`
        ),
        pendingGattAcquisitions: nonNegative(
          processNative.get('pendingGattAcquisitions'),
          `${processNativePath}.pendingGattAcquisitions`
        ),
        pendingRadioRequests: nonNegative(
          processNative.get('pendingRadioRequests'),
          `${processNativePath}.pendingRadioRequests`
        ),
        lateRadioCompletions: nonNegative(
          processNative.get('lateRadioCompletions'),
          `${processNativePath}.lateRadioCompletions`
        ),
        ingressDrops: Object.freeze({
          advertisement: nonNegative(drops.get('advertisement'), `${dropsPath}.advertisement`),
          notification: nonNegative(drops.get('notification'), `${dropsPath}.notification`),
          control: nonNegative(drops.get('control'), `${dropsPath}.control`)
        }),
        connectSections: nonNegative(processNative.get('connectSections'), `${processNativePath}.connectSections`),
        liveOps: nonNegative(processNative.get('liveOps'), `${processNativePath}.liveOps`)
      })
    })
  })
}

function peerRecordOrThrow(value: unknown, path: string): WirePeerRecord {
  const fields = exactObject(
    value,
    ['peerId', 'name', 'rssi', 'source', 'reachability', 'connection', 'bond', 'lastSeenAtMonotonicMs'],
    path
  )
  return Object.freeze({
    peerId: stringOrThrow(fields.get('peerId'), `${path}.peerId`),
    name: optionalTextOrThrow(fields.get('name'), `${path}.name`),
    rssi: nullable(fields.get('rssi'), `${path}.rssi`, (entry, entryPath) =>
      integerOrThrow(entry, SIGNED_BYTE, entryPath)
    ),
    source: enumOrThrow(fields.get('source'), PEER_SOURCES, `${path}.source`),
    reachability: enumOrThrow(fields.get('reachability'), PEER_REACHABILITY, `${path}.reachability`),
    connection: enumOrThrow(fields.get('connection'), PEER_CONNECTION, `${path}.connection`),
    bond: enumOrThrow(fields.get('bond'), PEER_BOND, `${path}.bond`),
    lastSeenAtMonotonicMs: nullable(
      fields.get('lastSeenAtMonotonicMs'),
      `${path}.lastSeenAtMonotonicMs`,
      (entry, entryPath) => integerOrThrow(entry, NON_NEGATIVE, entryPath)
    )
  })
}

function cleanupFailureOrThrow(value: unknown, path: string): WireCleanupFailure {
  const fields = exactObject(value, ['resourceKind', 'code', 'domain', 'operation', 'detail', 'platform'], path)
  return Object.freeze({
    resourceKind: stringOrThrow(fields.get('resourceKind'), `${path}.resourceKind`),
    code: enumOrThrow(fields.get('code'), BLE_ERROR_CODES, `${path}.code`),
    domain: enumOrThrow(fields.get('domain'), BLE_ERROR_DOMAINS, `${path}.domain`),
    operation: stringOrThrow(fields.get('operation'), `${path}.operation`),
    detail: optionalTextOrThrow(fields.get('detail'), `${path}.detail`),
    platform: nullable(fields.get('platform'), `${path}.platform`, platformDetailOrThrow)
  })
}

function cleanupRecordOrThrow(value: unknown, path: string): WireCleanupRecord {
  const fields = exactObject(value, ['state', 'failures'], path)
  const state = enumOrThrow(fields.get('state'), CLEANUP_STATES, `${path}.state`)
  const failures = arrayOf(fields.get('failures'), `${path}.failures`, cleanupFailureOrThrow)
  if ((state === 'released') !== (failures.length === 0)) throw malformed(`${path}.failures`)
  return Object.freeze({ state, failures })
}

function descriptorOrThrow(value: unknown, path: string): WireDescriptor {
  const fields = exactObject(value, ['uuid', 'occurrence'], path)
  return Object.freeze({
    uuid: uuidOrThrow(fields.get('uuid'), `${path}.uuid`),
    occurrence: integerOrThrow(fields.get('occurrence'), NON_NEGATIVE, `${path}.occurrence`)
  })
}

function characteristicOrThrow(value: unknown, path: string): WireCharacteristic {
  const fields = exactObject(value, ['uuid', 'occurrence', 'properties', 'descriptors'], path)
  const descriptors = arrayOf(fields.get('descriptors'), `${path}.descriptors`, descriptorOrThrow)
  assertUniqueInstances(descriptors, `${path}.descriptors`)
  return Object.freeze({
    uuid: uuidOrThrow(fields.get('uuid'), `${path}.uuid`),
    occurrence: integerOrThrow(fields.get('occurrence'), NON_NEGATIVE, `${path}.occurrence`),
    properties: propertyMaskOrThrow(fields.get('properties'), `${path}.properties`),
    descriptors
  })
}

function serviceOrThrow(value: unknown, path: string): WireService {
  const fields = exactObject(value, ['uuid', 'occurrence', 'primary', 'includedServices', 'characteristics'], path)
  const characteristics = arrayOf(fields.get('characteristics'), `${path}.characteristics`, characteristicOrThrow)
  assertUniqueInstances(characteristics, `${path}.characteristics`)
  return Object.freeze({
    primary: nullable(fields.get('primary'), `${path}.primary`, booleanOrThrow),
    includedServices: nullable(fields.get('includedServices'), `${path}.includedServices`, (references, fieldPath) =>
      arrayOf(references, fieldPath, descriptorOrThrow)
    ),
    uuid: uuidOrThrow(fields.get('uuid'), `${path}.uuid`),
    occurrence: integerOrThrow(fields.get('occurrence'), NON_NEGATIVE, `${path}.occurrence`),
    characteristics
  })
}

function discoveryOrThrow(value: unknown, path: string): WireDiscovery {
  const fields = exactObject(value, ['connectionGeneration', 'databaseGeneration', 'services'], path)
  const services = arrayOf(fields.get('services'), `${path}.services`, serviceOrThrow)
  assertUniqueInstances(services, `${path}.services`)
  for (const service of services) {
    for (const included of service.includedServices ?? []) {
      if (!services.some(target => target.uuid === included.uuid && target.occurrence === included.occurrence))
        throw malformed(`${path}.included-service-unresolved`)
    }
  }
  return Object.freeze({
    connectionGeneration: stringOrThrow(fields.get('connectionGeneration'), `${path}.connectionGeneration`),
    databaseGeneration: stringOrThrow(fields.get('databaseGeneration'), `${path}.databaseGeneration`),
    services
  })
}

function readValueOrThrow(value: unknown, path: string): { readonly value: Uint8Array } {
  const fields = exactObject(value, ['valueB64'], path)
  return Object.freeze({ value: decodeBase64OrThrow(fields.get('valueB64'), `${path}.valueB64`) })
}

function characteristicReadOrThrow(
  value: unknown,
  path: string
): { readonly value: Uint8Array; readonly provenance: ReadProvenance } {
  const fields = exactObject(value, ['valueB64', 'provenance'], path)
  return Object.freeze({
    value: decodeBase64OrThrow(fields.get('valueB64'), `${path}.valueB64`),
    provenance: enumOrThrow(fields.get('provenance'), READ_PROVENANCES, `${path}.provenance`)
  })
}

function writeReceiptOrThrow(value: unknown, path: string): { readonly commitState: WireWriteCommitState } {
  const fields = exactObject(value, ['commitState'], path)
  return Object.freeze({
    commitState: enumOrThrow(fields.get('commitState'), WRITE_COMMIT_STATES, `${path}.commitState`)
  })
}

function phyObservationOrThrow(value: unknown, path: string): WirePhyObservation {
  const fields = exactObject(value, ['tx', 'rx'], path)
  return Object.freeze({
    tx: enumOrThrow(fields.get('tx'), PHYS, `${path}.tx`),
    rx: enumOrThrow(fields.get('rx'), PHYS, `${path}.rx`)
  })
}

function reconcileLinkOrThrow(value: unknown, path: string): WireReconcileLink {
  const fields = exactObject(
    value,
    ['peerId', 'connectionGeneration', 'state', 'reason', 'databaseGeneration', 'databaseChange', 'databaseState'],
    path
  )
  const peerId = stringOrThrow(fields.get('peerId'), `${path}.peerId`)
  const connectionGeneration = stringOrThrow(fields.get('connectionGeneration'), `${path}.connectionGeneration`)
  const databaseGeneration = nullable(fields.get('databaseGeneration'), `${path}.databaseGeneration`, stringOrThrow)
  const state = enumOrThrow(fields.get('state'), ['connected', 'ended'] as const, `${path}.state`)
  if (state === 'connected') {
    if (fields.get('reason') !== null) throw malformed(`${path}.reason`)
    return Object.freeze({
      peerId,
      connectionGeneration,
      state,
      reason: null,
      databaseGeneration,
      databaseChange: nullable(fields.get('databaseChange'), `${path}.databaseChange`, stringOrThrow),
      databaseState: nullable(fields.get('databaseState'), `${path}.databaseState`, (entry, entryPath) =>
        enumOrThrow(entry, DATABASE_STATES, entryPath)
      )
    })
  }
  if (fields.get('databaseChange') !== null) throw malformed(`${path}.databaseChange`)
  if (fields.get('databaseState') !== null) throw malformed(`${path}.databaseState`)
  return Object.freeze({
    peerId,
    connectionGeneration,
    state,
    reason: enumOrThrow(fields.get('reason'), LINK_REASONS, `${path}.reason`),
    databaseGeneration,
    databaseChange: null,
    databaseState: null
  })
}

function reconcileSubscriptionOrThrow(value: unknown, path: string): WireReconcileSubscription {
  if (typeof value === 'object' && value !== null && Reflect.get(value, 'state') === 'live') {
    const fields = exactObject(value, ['consumer', 'state'], path)
    return Object.freeze({ consumer: stringOrThrow(fields.get('consumer'), `${path}.consumer`), state: 'live' })
  }
  const hasError = typeof value === 'object' && value !== null && Object.hasOwn(value, 'error')
  const fields = exactObject(
    value,
    ['consumer', 'state', 'reason', 'droppedItems', 'droppedBytes', ...(hasError ? ['error'] : [])],
    path
  )
  const reason = enumOrThrow(fields.get('reason'), STREAM_END_REASONS, `${path}.reason`)
  if (hasError && reason !== 'source-failed') throw malformed(`${path}.error`)
  return Object.freeze({
    consumer: stringOrThrow(fields.get('consumer'), `${path}.consumer`),
    state: enumOrThrow(fields.get('state'), ['ended'] as const, `${path}.state`),
    reason,
    droppedItems: integerOrThrow(fields.get('droppedItems'), NON_NEGATIVE, `${path}.droppedItems`),
    droppedBytes: integerOrThrow(fields.get('droppedBytes'), NON_NEGATIVE, `${path}.droppedBytes`),
    ...(hasError ? { error: remoteFailureOrThrow(fields.get('error'), `${path}.error`) } : {})
  })
}

function reconcileOrThrow(value: unknown, path: string): WireReconcile {
  const fields = exactObject(
    value,
    ['adapter', 'links', 'subscriptions', 'security', 'securityFailures', 'restored', 'scan'],
    path
  )
  return Object.freeze({
    adapter: adapterStateOrThrow(fields.get('adapter'), `${path}.adapter`),
    links: arrayOf(fields.get('links'), `${path}.links`, reconcileLinkOrThrow),
    subscriptions: arrayOf(fields.get('subscriptions'), `${path}.subscriptions`, reconcileSubscriptionOrThrow),
    security: arrayOf(fields.get('security'), `${path}.security`, (entry, entryPath) => {
      const peer = exactObject(entry, ['peerId', 'state'], entryPath)
      return Object.freeze({
        peerId: stringOrThrow(peer.get('peerId'), `${entryPath}.peerId`),
        state: securityStateOrThrow(peer.get('state'), `${entryPath}.state`)
      })
    }),
    securityFailures: arrayOf(fields.get('securityFailures'), `${path}.securityFailures`, (entry, entryPath) => {
      const peer = exactObject(entry, ['peerId', 'error'], entryPath)
      return Object.freeze({
        peerId: nullable(peer.get('peerId'), `${entryPath}.peerId`, stringOrThrow),
        error: remoteFailureOrThrow(peer.get('error'), `${entryPath}.error`)
      })
    }),
    restored: arrayOf(fields.get('restored'), `${path}.restored`, restoredPeerOrThrow),
    scan: nullable(fields.get('scan'), `${path}.scan`, stringOrThrow)
  })
}

function orderedDecimal(value: unknown, path: string): string {
  const text = stringOrThrow(value, path)
  if (!/^(0|[1-9][0-9]*)$/.test(text) || text.length > 20 || (text.length === 20 && text > '18446744073709551615'))
    throw malformed(path)
  return text
}
function orderedReclaim(value: unknown, path: string): WireOrderedReclaimContext {
  const f = exactObject(
    value,
    [
      'connectionGeneration',
      'databaseGeneration',
      'publicConnectionGeneration',
      'publicDatabaseGeneration',
      'maxItems',
      'maxBytes',
      'maxControls',
      'members'
    ],
    path
  )
  const count = (v: unknown, p: string) => integerOrThrow(v, { min: 1, max: 4294967295 }, p)
  return {
    connectionGeneration: stringOrThrow(f.get('connectionGeneration'), path),
    databaseGeneration: stringOrThrow(f.get('databaseGeneration'), path),
    publicConnectionGeneration: stringOrThrow(f.get('publicConnectionGeneration'), path),
    publicDatabaseGeneration: stringOrThrow(f.get('publicDatabaseGeneration'), path),
    maxItems: count(f.get('maxItems'), path),
    maxBytes: count(f.get('maxBytes'), path),
    maxControls: count(f.get('maxControls'), path),
    members: arrayOf(f.get('members'), path, (v, p) => {
      const m = exactObject(
        v,
        ['memberId', 'publicMemberId', 'selector', 'requestedDelivery', 'deliveryMode', 'preferredDeliveryMode'],
        p
      )
      const selector = exactObject(
        m.get('selector'),
        ['serviceUuid', 'serviceOccurrence', 'characteristicUuid', 'characteristicOccurrence'],
        p
      )
      const mode = (candidate: unknown, modePath: string) =>
        enumOrThrow(candidate, ['notification', 'indication'] as const, modePath)
      return {
        memberId: orderedDecimal(m.get('memberId'), p),
        publicMemberId: stringOrThrow(m.get('publicMemberId'), p),
        selector: {
          serviceUuid: uuidOrThrow(selector.get('serviceUuid'), p),
          serviceOccurrence: integerOrThrow(selector.get('serviceOccurrence'), NON_NEGATIVE, p),
          characteristicUuid: uuidOrThrow(selector.get('characteristicUuid'), p),
          characteristicOccurrence: integerOrThrow(selector.get('characteristicOccurrence'), NON_NEGATIVE, p)
        },
        requestedDelivery: enumOrThrow(
          m.get('requestedDelivery'),
          ['prefer-notification', 'prefer-indication', 'require-notification', 'require-indication'] as const,
          p
        ),
        deliveryMode: nullable(m.get('deliveryMode'), p, mode),
        preferredDeliveryMode: nullable(m.get('preferredDeliveryMode'), p, mode)
      }
    })
  }
}
function recordingReclaimStatus(value: unknown, path: string): WireRecordingReclaimStatus {
  const f = exactObject(value, ['state', 'mappingCommitted', 'error'], path)
  const state = enumOrThrow(
    f.get('state'),
    [
      'reserved',
      'pending',
      'ready',
      'committed',
      'releasing',
      'release-failed',
      'released',
      'failed',
      'retired'
    ] as const,
    path
  )
  const error = nullable(f.get('error'), path, remoteFailureOrThrow)
  if ((state === 'failed' || state === 'release-failed') !== (error !== null)) throw malformed(path)
  const mappingCommitted = booleanOrThrow(f.get('mappingCommitted'), path)
  if (
    (state === 'committed' && !mappingCommitted) ||
    (['reserved', 'pending', 'ready', 'failed'].includes(state) && mappingCommitted)
  )
    throw malformed(path)
  return { state, mappingCommitted, error }
}
function recordingReclaimRefusal(value: unknown, path: string): WireRecordingReclaimRefusal {
  const f = exactObject(value, ['state', 'error'], path)
  return {
    state: enumOrThrow(f.get('state'), ['not-reserved'] as const, path),
    error: remoteFailureOrThrow(f.get('error'), `${path}.error`)
  }
}
function orderedReceipt(value: unknown, path: string): WireOrderedReceipt {
  const f = exactObject(value, ['receiptId', 'discardedDeliveryId', 'evidence'], path)
  return {
    receiptId: orderedDecimal(f.get('receiptId'), path),
    discardedDeliveryId: orderedDecimal(f.get('discardedDeliveryId'), path),
    evidence: stringOrThrow(f.get('evidence'), path)
  }
}
function orderedStatus(value: unknown, path: string): WireOrderedStatus {
  const f = exactObject(value, ['sourceGeneration', 'members', 'cleanupComplete', 'setupError', 'fenceError'], path)
  return {
    sourceGeneration: nullable(f.get('sourceGeneration'), path, (v, p) => {
      const g = exactObject(v, ['kind', 'value'], p)
      const kind = enumOrThrow(g.get('kind'), ['counter', 'opaque'] as const, p)
      return { kind, value: kind === 'counter' ? orderedDecimal(g.get('value'), p) : stringOrThrow(g.get('value'), p) }
    }),
    members: arrayOf(f.get('members'), path, (v, p) => {
      const m = exactObject(v, ['memberId', 'acquisition', 'effectiveDelivery', 'cleanupReleased', 'cleanupError'], p)
      return {
        memberId: orderedDecimal(m.get('memberId'), p),
        acquisition: enumOrThrow(m.get('acquisition'), ['not-started', 'acquired', 'indeterminate'] as const, p),
        effectiveDelivery: enumOrThrow(m.get('effectiveDelivery'), DELIVERY_KINDS, p),
        cleanupReleased: booleanOrThrow(m.get('cleanupReleased'), p),
        cleanupError: nullable(m.get('cleanupError'), p, remoteFailureOrThrow)
      }
    }),
    cleanupComplete: booleanOrThrow(f.get('cleanupComplete'), path),
    setupError: nullable(f.get('setupError'), path, remoteFailureOrThrow),
    fenceError: nullable(f.get('fenceError'), path, remoteFailureOrThrow)
  }
}
function orderedNull(value: unknown, path: string): null {
  if (value !== null) throw malformed(path)
  return null
}

type OpParsers = { readonly [Op in WireOp]: (value: unknown, path: string) => WireOpResults[Op] }

const OP_PARSERS: OpParsers = Object.freeze({
  'adapter.state': adapterStateOrThrow,
  'counters.describe': countersOrThrow,
  'terminal-evidence.reserve': (value: unknown, path: string) => {
    const fields = exactObject(value, ['maximumBytes', 'reservationId'], path)
    return Object.freeze({
      maximumBytes: integerOrThrow(fields.get('maximumBytes'), NON_NEGATIVE, `${path}.maximumBytes`),
      reservationId: stringOrThrow(fields.get('reservationId'), `${path}.reservationId`)
    })
  },
  'terminal-evidence.commit': (value: unknown, path: string) => {
    if (typeof value !== 'object' || value === null || Array.isArray(value) || !isPlainObject(value))
      throw malformed(path)
    const kind = stringOrThrow(Reflect.get(value, 'kind'), `${path}.kind`)
    const fields = exactObject(
      value,
      ['kind', ...(kind === 'committed' ? ['ref', 'byteLength', 'digest', 'formatVersion', 'durability'] : ['reason'])],
      path
    )
    if (kind === 'refused') {
      const reason = stringOrThrow(fields.get('reason'), `${path}.reason`)
      if (reason !== 'quota-exhausted' && reason !== 'storage-unavailable' && reason !== 'capability-unsupported')
        throw malformed(`${path}.reason`)
      return Object.freeze({ kind, reason })
    }
    if (kind !== 'committed') throw malformed(`${path}.kind`)
    if (
      integerOrThrow(fields.get('formatVersion'), NON_NEGATIVE, `${path}.formatVersion`) !== 1 ||
      stringOrThrow(fields.get('durability'), `${path}.durability`) !== 'committed'
    )
      throw malformed(path)
    const digest = stringOrThrow(fields.get('digest'), `${path}.digest`)
    if (!/^[0-9a-f]{64}$/u.test(digest)) throw malformed(`${path}.digest`)
    return Object.freeze({
      kind,
      ref: stringOrThrow(fields.get('ref'), `${path}.ref`),
      byteLength: integerOrThrow(fields.get('byteLength'), NON_NEGATIVE, `${path}.byteLength`),
      digest,
      formatVersion: 1 as const,
      durability: 'committed' as const
    })
  },
  'terminal-evidence.read': (value: unknown, path: string) => {
    const fields = exactObject(value, ['canonicalBytesBase64', 'digest'], path)
    const digest = stringOrThrow(fields.get('digest'), `${path}.digest`)
    if (!/^[0-9a-f]{64}$/u.test(digest)) throw malformed(`${path}.digest`)
    return Object.freeze({
      canonicalBytesBase64: stringOrThrow(fields.get('canonicalBytesBase64'), `${path}.canonicalBytesBase64`),
      digest
    })
  },
  'terminal-evidence.release': (value: unknown, path: string) => {
    const fields = exactObject(value, ['released', 'retainedForRecovery'], path)
    if (fields.get('released') !== true) throw malformed(`${path}.released`)
    const retainedForRecovery = fields.get('retainedForRecovery')
    if (typeof retainedForRecovery !== 'boolean') throw malformed(`${path}.retainedForRecovery`)
    return Object.freeze({ released: true as const, retainedForRecovery })
  },
  'scan.start': (value: unknown, path: string) => {
    const fields = exactObject(value, ['operationId'], path)
    return Object.freeze({ operationId: stringOrThrow(fields.get('operationId'), `${path}.operationId`) })
  },
  'scan.stop': cleanupRecordOrThrow,
  'peers.resolve': (value: unknown, path: string) => nullable(value, path, peerRecordOrThrow),
  'peers.known': (value: unknown, path: string) => arrayOf(value, path, peerRecordOrThrow),
  'peers.connected': (value: unknown, path: string) => arrayOf(value, path, peerRecordOrThrow),
  'peers.bonded': (value: unknown, path: string) => arrayOf(value, path, peerRecordOrThrow),
  'peers.restored': (value: unknown, path: string) => arrayOf(value, path, peerRecordOrThrow),
  'peers.claim-restored': (value: unknown, path: string) => {
    const fields = exactObject(value, ['peers'], path)
    return Object.freeze({ peers: arrayOf(fields.get('peers'), `${path}.peers`, peerRecordOrThrow) })
  },
  'connection.rssi': (value: unknown, path: string) => {
    const fields = exactObject(value, ['rssi'], path)
    return Object.freeze({ rssi: integerOrThrow(fields.get('rssi'), SIGNED_BYTE, `${path}.rssi`) })
  },
  'connection.effective-mtu': (value: unknown, path: string) => {
    const fields = exactObject(value, ['mtu'], path)
    return Object.freeze({
      mtu: nullable(fields.get('mtu'), `${path}.mtu`, (entry, entryPath) => integerOrThrow(entry, ATT_MTU, entryPath))
    })
  },
  'connection.request-mtu': (value: unknown, path: string) => {
    const fields = exactObject(value, ['mtu'], path)
    return Object.freeze({ mtu: integerOrThrow(fields.get('mtu'), ATT_MTU, `${path}.mtu`) })
  },
  'connection.request-priority': (value: unknown, path: string) => {
    const fields = exactObject(value, ['accepted'], path)
    return Object.freeze({ accepted: booleanOrThrow(fields.get('accepted'), `${path}.accepted`) })
  },
  'connection.request-subrate': (value: unknown, path: string) => {
    const fields = exactObject(value, ['accepted'], path)
    return Object.freeze({ accepted: booleanOrThrow(fields.get('accepted'), `${path}.accepted`) })
  },
  'connection.control-capabilities': (value: unknown, path: string) => {
    const fields = exactObject(value, ['subrate'], path)
    return Object.freeze({ subrate: booleanOrThrow(fields.get('subrate'), `${path}.subrate`) })
  },
  'connection.read-phy': phyObservationOrThrow,
  'connection.request-phy': (value: unknown, path: string) => {
    const fields = exactObject(value, ['accepted', 'observation'], path)
    const accepted = booleanOrThrow(fields.get('accepted'), `${path}.accepted`)
    const observation = nullable(fields.get('observation'), `${path}.observation`, phyObservationOrThrow)
    // Acceptance is reported with the PHYs the controller then used; one without the other contradicts itself.
    if (accepted !== (observation !== null)) throw malformed(`${path}.observation`)
    return Object.freeze({ accepted, observation })
  },
  'connection.maximum-write-length': (value: unknown, path: string) => {
    const fields = exactObject(value, ['maximumWriteLength'], path)
    return Object.freeze({
      maximumWriteLength: integerOrThrow(
        fields.get('maximumWriteLength'),
        ATT_WRITE_LENGTH,
        `${path}.maximumWriteLength`
      )
    })
  },
  'connection.write-readiness': (value: unknown, path: string) => {
    const fields = exactObject(value, ['ready'], path)
    return Object.freeze({ ready: booleanOrThrow(fields.get('ready'), `${path}.ready`) })
  },
  'security.state': securityStateOrThrow,
  'security.pair': (value: unknown, path: string) => {
    const fields = exactObject(value, ['outcome', 'state'], path)
    return Object.freeze({
      outcome: enumOrThrow(fields.get('outcome'), PAIR_OUTCOMES, `${path}.outcome`),
      state: securityStateOrThrow(fields.get('state'), `${path}.state`)
    })
  },
  'security.cancel-pairing': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state'], path)
    return Object.freeze({ state: enumOrThrow(fields.get('state'), ['requested'] as const, `${path}.state`) })
  },
  'background.acquire': (value: unknown, path: string) => {
    const fields = exactObject(value, ['leaseId'], path)
    return Object.freeze({ leaseId: stringOrThrow(fields.get('leaseId'), `${path}.leaseId`) })
  },
  'background.release': cleanupRecordOrThrow,
  'background.update-notification': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state'], path)
    return Object.freeze({ state: enumOrThrow(fields.get('state'), ['updated'] as const, `${path}.state`) })
  },
  'presence.observe': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state'], path)
    return Object.freeze({ state: enumOrThrow(fields.get('state'), ['observing'] as const, `${path}.state`) })
  },
  'presence.unobserve': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state'], path)
    return Object.freeze({ state: enumOrThrow(fields.get('state'), ['idle'] as const, `${path}.state`) })
  },
  'companion.associate': (value: unknown, path: string) => {
    const fields = exactObject(value, ['source', 'associationId', 'peerId', 'displayName'], path)
    return Object.freeze({
      source: enumOrThrow(fields.get('source'), ['associated', 'already-associated'] as const, `${path}.source`),
      associationId: integerOrThrow(
        fields.get('associationId'),
        { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER },
        `${path}.associationId`
      ),
      peerId: optionalTextOrThrow(fields.get('peerId'), `${path}.peerId`),
      displayName: optionalTextOrThrow(fields.get('displayName'), `${path}.displayName`)
    })
  },
  'companion.list': (value: unknown, path: string) => {
    const fields = exactObject(value, ['associations'], path)
    return Object.freeze({
      associations: arrayOf(fields.get('associations'), `${path}.associations`, (entry, entryPath) => {
        const record = exactObject(entry, ['associationId', 'peerId', 'displayName'], entryPath)
        return Object.freeze({
          associationId: integerOrThrow(
            record.get('associationId'),
            { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER },
            `${entryPath}.associationId`
          ),
          peerId: optionalTextOrThrow(record.get('peerId'), `${entryPath}.peerId`),
          displayName: optionalTextOrThrow(record.get('displayName'), `${entryPath}.displayName`)
        })
      })
    })
  },
  'companion.disassociate': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state', 'associationId'], path)
    return Object.freeze({
      state: enumOrThrow(fields.get('state'), ['disassociated'] as const, `${path}.state`),
      associationId: integerOrThrow(
        fields.get('associationId'),
        { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER },
        `${path}.associationId`
      )
    })
  },
  'connection.connect': (value: unknown, path: string) => {
    const fields = exactObject(value, ['peerKey', 'connectionGeneration'], path)
    return Object.freeze({
      peerKey: stringOrThrow(fields.get('peerKey'), `${path}.peerKey`),
      connectionGeneration: stringOrThrow(fields.get('connectionGeneration'), `${path}.connectionGeneration`)
    })
  },
  'connection.disconnect': cleanupRecordOrThrow,
  'gatt.discover': discoveryOrThrow,
  'gatt.ordered.recording-reclaim.next': (value, path) => {
    if (typeof value === 'object' && value !== null && Reflect.get(value, 'state') === 'not-reserved') {
      return recordingReclaimRefusal(value, path)
    }
    const f = exactObject(value, ['requestId'], path)
    return { requestId: orderedDecimal(f.get('requestId'), path) }
  },
  'gatt.ordered.recording-reclaim.reserve': (value, path) =>
    typeof value === 'object' && value !== null && Reflect.get(value, 'state') === 'not-reserved'
      ? recordingReclaimRefusal(value, path)
      : recordingReclaimStatus(value, path),
  'gatt.ordered.recording-reclaim.start': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.status': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.commit': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.cancel': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.release': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.forget-retire': (value, path) => {
    const f = exactObject(value, ['state'], path)
    return { state: enumOrThrow(f.get('state'), ['forgotten'] as const, path) }
  },
  'gatt.ordered.recording-reclaim.ack-forget': (value, path) => {
    const f = exactObject(value, ['state'], path)
    return { state: enumOrThrow(f.get('state'), ['acknowledged', 'absent'] as const, path) }
  },
  'gatt.ordered.recording-reclaim.ack-retire': (value, path) => {
    const f = exactObject(value, ['state'], path)
    return { state: enumOrThrow(f.get('state'), ['acknowledged'] as const, path) }
  },
  'gatt.ordered.recording-reclaim.retire': recordingReclaimStatus,
  'gatt.ordered.recording-reclaim.version': (value, path) => {
    const f = exactObject(value, ['wireVersion'], path)
    return { wireVersion: integerOrThrow(f.get('wireVersion'), { min: 0, max: 65535 }, path) }
  },
  'gatt.ordered.marker.version': (value, path) => {
    const f = exactObject(value, ['wireVersion'], path)
    return { wireVersion: integerOrThrow(f.get('wireVersion'), { min: 0, max: 65535 }, path) }
  },
  'gatt.ordered.marker.reserve': (value, path) => {
    const f = exactObject(value, ['handle'], path)
    return { handle: orderedDecimal(f.get('handle'), path) }
  },
  'gatt.ordered.marker.start': value => decodeOrderedMarkerWriteStatus(value),
  'gatt.ordered.marker.status': value => decodeOrderedMarkerWriteStatus(value),
  'gatt.ordered.marker.cancel': value => decodeOrderedMarkerWriteStatus(value),
  'gatt.ordered.marker.forget': orderedNull,
  'gatt.ordered.reserve': (value, path) => {
    const f = exactObject(value, ['collectionId', 'reclaimCapability'], path)
    return {
      collectionId: orderedDecimal(f.get('collectionId'), path),
      reclaimCapability: stringOrThrow(f.get('reclaimCapability'), path)
    }
  },
  'gatt.ordered.reclaim': (value, path) =>
    nullable(value, path, (item, itemPath) => {
      if (typeof item === 'object' && item !== null && !Array.isArray(item) && Object.hasOwn(item, 'recordingId')) {
        const fields = exactObject(item, ['recordingId'], itemPath)
        return { recordingId: stringOrThrow(fields.get('recordingId'), itemPath) }
      }
      return orderedReclaim(item, itemPath)
    }),
  'gatt.ordered.open': orderedNull,
  'gatt.ordered.status': orderedStatus,
  'gatt.ordered.capacity.version': (value, path) => {
    const f = exactObject(value, ['wireVersion', 'maximumMaxBytes'], path)
    return {
      wireVersion: integerOrThrow(f.get('wireVersion'), { min: 0, max: 65535 }, path),
      maximumMaxBytes: integerOrThrow(f.get('maximumMaxBytes'), { min: 1, max: 4194304 }, path)
    }
  },
  'gatt.ordered.increase-capacity': (value, path) => {
    const f = exactObject(value, ['previousMaxBytes', 'maxBytes', 'maximumMaxBytes'], path)
    const previousMaxBytes = integerOrThrow(f.get('previousMaxBytes'), { min: 1, max: 4194304 }, path)
    const maxBytes = integerOrThrow(f.get('maxBytes'), { min: previousMaxBytes, max: 4194304 }, path)
    const maximumMaxBytes = integerOrThrow(f.get('maximumMaxBytes'), { min: maxBytes, max: 4194304 }, path)
    return { previousMaxBytes, maxBytes, maximumMaxBytes }
  },
  'gatt.ordered.poll': (value, path) => {
    const f = exactObject(value, ['deliveryId', 'record'], path)
    return {
      deliveryId: nullable(f.get('deliveryId'), path, orderedDecimal),
      record: stringOrThrow(f.get('record'), path)
    }
  },
  'gatt.ordered.ack-delivery': orderedNull,
  'gatt.ordered.discard': orderedReceipt,
  'gatt.ordered.poll-discard': (value, path) => nullable(value, path, orderedReceipt),
  'gatt.ordered.ack-discard': orderedNull,
  'gatt.ordered.close': (value, path) =>
    enumOrThrow(value, ['released', 'pending-source', 'pending-drain'] as const, path),
  'gatt.ordered.ack-group': orderedNull,
  'gatt.ordered.forget-group': orderedNull,
  'gatt.ordered.version': (value: unknown, path: string) => {
    const fields = exactObject(value, ['wireVersion'], path)
    return Object.freeze({
      wireVersion: integerOrThrow(fields.get('wireVersion'), { min: 0, max: 65535 }, `${path}.wireVersion`)
    })
  },
  'gatt.ordered.failure-metadata.version': (value: unknown, path: string) => {
    const fields = exactObject(value, ['versions'], path)
    return Object.freeze({
      versions: Object.freeze(
        arrayOf(fields.get('versions'), `${path}.versions`, (entry, entryPath) =>
          integerOrThrow(entry, { min: 0, max: 65535 }, entryPath)
        )
      )
    })
  },
  'gatt.read': characteristicReadOrThrow,
  'gatt.read-descriptor': readValueOrThrow,
  'gatt.write': writeReceiptOrThrow,
  'gatt.write-when-ready': writeReceiptOrThrow,
  'gatt.write-descriptor': writeReceiptOrThrow,
  'gatt.subscribe': (value: unknown, path: string) => {
    const fields = exactObject(value, ['consumer', 'delivery'], path)
    return Object.freeze({
      consumer: stringOrThrow(fields.get('consumer'), `${path}.consumer`),
      delivery: enumOrThrow(fields.get('delivery'), DELIVERY_KINDS, `${path}.delivery`)
    })
  },
  'gatt.subscribe-recorded': (value: unknown, path: string) => {
    const fields = exactObject(value, ['consumer', 'delivery'], path)
    return Object.freeze({
      consumer: stringOrThrow(fields.get('consumer'), `${path}.consumer`),
      delivery: enumOrThrow(fields.get('delivery'), DELIVERY_KINDS, `${path}.delivery`)
    })
  },
  'gatt.unsubscribe': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state', 'physicalDisabled'], path)
    return Object.freeze({
      state: enumOrThrow(fields.get('state'), ['released'] as const, `${path}.state`),
      physicalDisabled: booleanOrThrow(fields.get('physicalDisabled'), `${path}.physicalDisabled`)
    })
  },
  'op.cancel': (value: unknown, path: string) => {
    const fields = exactObject(value, ['state'], path)
    return Object.freeze({ state: enumOrThrow(fields.get('state'), CANCELLATION_STATES, `${path}.state`) })
  },
  'session.reconcile': reconcileOrThrow,
  'session.dispose': cleanupRecordOrThrow
})

export function parseOpValue<Op extends WireOp>(op: Op, value: unknown): WireResult<WireOpResults[Op]> {
  return capture(() => {
    wireOpOrThrow(op, 'op')
    return OP_PARSERS[op](value, op)
  })
}

export function checkWriteReceipt(
  receipt: { readonly commitState: WireWriteCommitState },
  mode: WriteMode
): WireResult<{ readonly commitState: WireWriteCommitState }> {
  return capture(() => {
    if (mode === 'without-response' && receipt.commitState === 'confirmed') {
      throw malformed('gatt.write.commitState')
    }
    return receipt
  })
}

export function parseDrainValue(value: unknown, lastOrdinal: number | null): WireResult<WireDrainBatch> {
  return capture(() => drainBatchOrThrow(value, lastOrdinal, 'drain'))
}
