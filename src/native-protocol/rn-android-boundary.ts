// src/native-protocol/rn-android-boundary.ts

import { contractError } from '../backend-contract/errors'
import type { SecurityPairOptions } from '../backend-contract/security'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type ConnectionControlCapabilities,
  type ConnectionPriority,
  type PhyPreference
} from '../backend-contract/connection-controls'
import type {
  NativeAttachmentIdentity,
  NativeProtocolHandshakeResult,
  Spec as NativeProtocolControl
} from '../NativeUnifiedBleProtocolControl'
import type {
  CoreBluetoothAdapterSnapshot,
  CoreBluetoothAdvertisement,
  CoreBluetoothBoundary,
  CoreBluetoothCharacteristicAddress,
  CoreBluetoothDescriptorAddress,
  CoreBluetoothGattSnapshot,
  CoreBluetoothNotificationDeliveryMode,
  CoreBluetoothPhyObservation,
  CoreBluetoothPhyRequestResult,
  CoreBluetoothScanPlatformOptions
} from '../backends/corebluetooth/corebluetooth-boundary'
import type { ConnectionIntent } from '../backend-contract/backend'
import {
  copyNativeProtocolBytes,
  releaseNativeProtocolBytes,
  retainNativeProtocolBytes,
  setNativeProtocolFatalSink,
  setNativeProtocolEventSink,
  submitNativeProtocolCommand,
  type NativeBinaryReference
} from './rn-jsi-binary-runtime'
import {
  decodeNativeProtocolRecord,
  encodeNativeProtocolRecord,
  type NativeProtocolField,
  type NativeProtocolRecord
} from './v2-codec'
import {
  adapterStateFromRecord,
  addressKey,
  advertisementBinaryReferences,
  advertisementFromRecord,
  attachmentIdentityFromRecord,
  binaryReferenceFromRecord,
  binaryReferenceRecord,
  commandEpoch,
  commandRecord,
  field,
  nativePeerIdForCommand,
  nativePhyFromPublic,
  operationKey,
  protocolRecord,
  requiredBoolean,
  requiredRecord,
  requiredSigned,
  requiredString,
  snapshotFromRecord,
  optionalRecord,
  optionalString,
  optionalUnsigned,
  publicPhyFromNative,
  parseAdvertisementRecord,
  requiredUnsigned
} from './rn-android-protocol-records'
import {
  NATIVE_PROTOCOL_ABI_VERSION,
  NATIVE_PROTOCOL_CONTROL_SURFACE_VERSION,
  NATIVE_PROTOCOL_VERSION
} from './generated/native-protocol-v2-schema'
import type { ParsedNativeAdvertisement } from './rn-android-protocol-records'

export type AndroidSecurityBondState = 'bonded' | 'bonding' | 'not-bonded' | 'unknown' | 'unsupported'

export interface AndroidSecurityState {
  readonly bond: AndroidSecurityBondState
  readonly encryption: 'encrypted' | 'not-encrypted' | 'unknown' | 'unsupported'
  readonly authentication: 'authenticated' | 'unauthenticated' | 'unknown' | 'unsupported'
  readonly secureConnections: 'yes' | 'no' | 'unknown' | 'unsupported'
  readonly pairingPossible: boolean | null
}

export interface AndroidSecurityStateChangedRecord {
  readonly nativePeerId: string
  readonly state: AndroidSecurityState
}

export interface AndroidBondedPeerSnapshot {
  readonly nativePeerId: string
  readonly displayName: string | null
}

const protocolVersion = NATIVE_PROTOCOL_VERSION
const controlSurfaceVersion = NATIVE_PROTOCOL_CONTROL_SURFACE_VERSION
const contractVersion = 1
const maximumNativePayloadBytes = 512 * 1024
const maximumExpectedLateTerminals = 64

type PendingResult = {
  readonly kind: string
  readonly nativePeerId: string | null
  readonly resolve: (record: NativeProtocolRecord) => void
  readonly reject: (error: Error) => void
}

type ExpectedLateTerminal = {
  readonly nativePeerId: string
  readonly attachment: NativeAttachmentIdentity
}

type NativeConnection = {
  readonly record: NativeProtocolRecord
  state: 'connecting' | 'connected' | 'disconnected'
}

type NativeSubscription = {
  readonly subscriptionId: string
  readonly address: CoreBluetoothCharacteristicAddress
  readonly onValue: (value: Uint8Array) => void
}

function nativeConnectionIntent(intent: ConnectionIntent): 'direct' | 'whenAvailable' {
  if (intent === 'direct') return 'direct'
  if (intent === 'when-available') return 'whenAvailable'
  throw contractError('argument.invalid', 'connection', 'rn-android-boundary.connect.intent')
}

/**
 * The React Native Android boundary owns the versioned JSI command/event transport.
 * It preserves native-only identifiers inside this file and exposes only the typed
 * direct-boundary interface consumed by the shared backend.
 */
export class ReactNativeAndroidProtocolBoundary implements CoreBluetoothBoundary {
  readonly connectionIntentCapabilities: Readonly<{ whenAvailable: 'available' | 'unsupported' }> = Object.freeze({
    whenAvailable: 'available'
  })
  readonly descriptorOperationsAvailable: boolean = true
  private phyExtensionAvailable = false
  private securityExtensionAvailable = false
  private securityCancellationExtensionAvailable = false
  private readonly securityListeners = new Set<(record: AndroidSecurityStateChangedRecord) => void>()

  get connectionControlCapabilities(): ConnectionControlCapabilities {
    return Object.freeze({
      rssi: 'available',
      requestMtu: 'available',
      effectiveMtu: 'available',
      priority: 'available',
      phy: this.phyExtensionAvailable ? 'available' : 'unavailable'
    })
  }

  get securityAvailable(): boolean {
    return this.securityExtensionAvailable
  }

  get securityCancellationAvailable(): boolean {
    return this.securityCancellationExtensionAvailable
  }
  private readonly pending = new Map<string, PendingResult>()
  /**
   * Correlations explicitly rejected by connection loss may still terminalize
   * in native code. Keep only those exact, peer-scoped correlations so one
   * expected late terminal is quiet while unknown or duplicate terminals stay
   * diagnostic.
   */
  private readonly expectedLateTerminals = new Map<string, ExpectedLateTerminal>()
  private readonly connections = new Map<string, NativeConnection>()
  private readonly databases = new Map<string, NativeProtocolRecord>()
  private readonly subscriptionsByAddress = new Map<string, NativeSubscription>()
  private readonly scanListeners = new Set<(advertisement: CoreBluetoothAdvertisement) => void>()
  private readonly scanFailureListeners = new Set<(safeMessage: string) => void>()
  private readonly disconnectListeners = new Set<(nativePeerId: string, safeMessage: string | null) => void>()
  private readonly databaseChangedListeners = new Set<(nativePeerId: string) => void>()
  private readonly adapterListeners = new Set<(state: CoreBluetoothAdapterSnapshot) => void>()
  private readonly nativeReleaseRetryLedger = new Map<string, NativeBinaryReference>()
  private latestAdapterState: CoreBluetoothAdapterSnapshot | null = null
  private attachmentRecord: NativeProtocolRecord | null = null
  private maximumInputPayloadBytes = 0
  private nextEpoch = 1
  private nextConnection = 1
  private nextDatabase = 1
  private nextSubscription = 1
  private consumerListenerFailureCount = 0
  private opened = false
  private closing = false
  private nativeAttachmentOpened = false
  private nativeDestroyCompleted = false
  private destroyRequested = false
  private destroyResult: Promise<void> | null = null
  private fatalTeardownObservation: Promise<void> | null = null
  private preJavaScriptEventBufferOverflowed = false

  constructor(
    private readonly control: NativeProtocolControl,
    private readonly ownerId: string
  ) {}

  /** Binds this one native radio boundary to the backend attachment before its control handshake. */
  bindAttachment(attachment: NativeAttachmentIdentity): void {
    if (this.opened || this.attachmentRecord !== null) {
      throw contractError('lifecycle.invalid-state', 'boundary', 'rn-android-boundary.bind-attachment')
    }
    this.attachmentRecord = protocolRecord('attachment', [
      field(1, attachment.attachmentId),
      field(2, attachment.backendInstanceId),
      field(3, attachment.backendGeneration),
      field(4, attachment.adapterId),
      field(5, attachment.adapterGeneration)
    ])
  }

  adapterSnapshot(): CoreBluetoothAdapterSnapshot {
    return (
      this.latestAdapterState ??
      Object.freeze({
        availability: 'unknown',
        // 'unknown', like its availability and power siblings: this snapshot is
        // the ABSENCE of a measurement, not a denial. Reporting 'unavailable'
        // made every readiness gate treat a radio that is simply still starting
        // up as one the caller is not permitted to use.
        authorization: 'unknown',
        power: 'unknown',
        safeReason: 'The Android radio has not emitted its authoritative adapter state yet.'
      })
    )
  }

  async open(): Promise<void> {
    if (this.opened || this.nativeAttachmentOpened || this.destroyRequested) {
      throw contractError('lifecycle.invalid-state', 'boundary', 'rn-android-boundary.open')
    }
    const attachment = this.requireAttachmentRecord('open')
    this.preJavaScriptEventBufferOverflowed = false
    try {
      const handshake = await this.control.handshake({
        nativeProtocol: { minimum: protocolVersion, maximum: protocolVersion },
        abi: { minimum: NATIVE_PROTOCOL_ABI_VERSION, maximum: NATIVE_PROTOCOL_ABI_VERSION },
        controlSurface: { minimum: controlSurfaceVersion, maximum: controlSurfaceVersion },
        backendContract: { minimum: contractVersion, maximum: contractVersion },
        capabilitySchema: { minimum: contractVersion, maximum: contractVersion },
        eventSchema: { minimum: contractVersion, maximum: contractVersion },
        traceFormat: { minimum: contractVersion, maximum: contractVersion },
        ...attachmentIdentityFromRecord(attachment),
        ownerId: this.ownerId
      })
      this.nativeAttachmentOpened = true
      this.nativeDestroyCompleted = false
      assertHandshakeSelection(handshake)
      this.maximumInputPayloadBytes = Math.min(maximumNativePayloadBytes, handshake.maximumBinaryPayloadBytes)
      this.phyExtensionAvailable = handshake.phyAvailable === true
      this.securityExtensionAvailable = handshake.securityAvailable === true
      this.securityCancellationExtensionAvailable =
        this.securityExtensionAvailable && handshake.securityCancelPairingAvailable === true
      await this.control.installExecutionRuntime()
      setNativeProtocolFatalSink(reason => this.failAttachment(reason))
      setNativeProtocolEventSink(bytes => this.receiveRecord(bytes))
      if (this.preJavaScriptEventBufferOverflowed) {
        throw contractError('stream.overflow', 'boundary', 'rn-android-boundary.open.pre-js-event-buffer')
      }
      this.opened = true
    } catch (error) {
      this.maximumInputPayloadBytes = 0
      this.phyExtensionAvailable = false
      let closeFailure: Error | null = null
      if (this.nativeAttachmentOpened) {
        try {
          await this.closeNativeAttachment(attachment)
        } catch (closeError) {
          console.error('[ReactNativeAndroidProtocolBoundary.open] Handshake-open cleanup failed:', closeError)
          closeFailure =
            closeError instanceof Error
              ? closeError
              : new Error('Native attachment close failed with a non-Error value during boundary open rollback')
        }
      }
      this.destroyRequested = true
      if (closeFailure !== null) {
        const primaryFailure =
          error instanceof Error ? error : new Error('Native protocol boundary open failed with a non-Error value')
        throw new AggregateError(
          [primaryFailure, closeFailure],
          'Native protocol boundary open failed and its attachment cleanup remains retryable'
        )
      }
      throw error
    }
  }

  async startScan(
    onAdvertisement: (advertisement: CoreBluetoothAdvertisement) => void,
    serviceUuids: readonly string[],
    deviceAddresses: readonly string[] = [],
    platform?: CoreBluetoothScanPlatformOptions
  ): Promise<void> {
    this.requireOpen('start-scan')
    if (platform !== undefined && platform.kind !== 'android') {
      throw contractError('capability.unsupported', 'scan', 'rn-android-boundary.scan.platform-options')
    }
    if (platform?.phy !== undefined || platform?.reportDelayMs !== undefined) {
      throw contractError('capability.unsupported', 'scan', 'rn-android-boundary.scan.platform-options')
    }
    if (platform?.callbackType === 'match-lost') {
      // Android CALLBACK_TYPE_MATCH_LOST would be requested, but the native
      // event protocol has no loss representation: the radio forwards every
      // callback through the ordinary advertisement path, so a loss would be
      // emitted as a fresh observation. Fail closed instead of encoding it.
      throw contractError('capability.unsupported', 'scan', 'rn-android-boundary.scan.callback-type-match-lost')
    }
    this.scanListeners.add(onAdvertisement)
    try {
      await this.dispatch('scanStart', [
        field(
          12,
          protocolRecord('scanOptions', [
            field(1, [...serviceUuids]),
            field(2, true),
            field(3, androidScanMode(platform)),
            field(4, androidScanCallbackType(platform)),
            field(5, androidScanLegacy(platform)),
            ...(deviceAddresses.length === 0 ? [] : [field(6, [...deviceAddresses])])
          ])
        )
      ])
    } catch (error) {
      this.scanListeners.delete(onAdvertisement)
      throw error
    }
  }

  async stopScan(): Promise<void> {
    this.requireOpen('stop-scan')
    await this.dispatch('scanStop', [])
    this.scanListeners.clear()
  }

  /** Reads the current Android system bond table without creating GATT ownership. */
  async enumerateBondedPeers(): Promise<readonly AndroidBondedPeerSnapshot[]> {
    this.requireOpen('enumerate-bonded-peers')
    const result = await this.dispatch('enumerateBondedPeers', [])
    if (requiredString(result, 2, 'rn-android-boundary.enumerate-bonded-peers.kind') !== 'bondedPeers') {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.enumerate-bonded-peers.kind')
    }
    const bondedPeers = result.fields.find(candidate => candidate.id === 23)?.value
    if (!Array.isArray(bondedPeers)) {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.enumerate-bonded-peers.records')
    }
    return Object.freeze(
      bondedPeers.map((candidate, index) => {
        if (!isNativeProtocolRecord(candidate) || candidate.kind !== 'bondedPeerSnapshot') {
          throw contractError(
            'protocol.malformed',
            'boundary',
            `rn-android-boundary.enumerate-bonded-peers.record-${index}`
          )
        }
        const nativePeerId = requiredString(
          candidate,
          1,
          `rn-android-boundary.enumerate-bonded-peers.record-${index}.native-peer-id`
        )
        if (nativePeerId.trim().length === 0) {
          throw contractError(
            'protocol.malformed',
            'boundary',
            `rn-android-boundary.enumerate-bonded-peers.record-${index}.native-peer-id`
          )
        }
        return Object.freeze({
          nativePeerId,
          displayName: optionalString(candidate, 2)
        })
      })
    )
  }

  async connect(nativePeerId: string, intent: ConnectionIntent = 'direct'): Promise<void> {
    this.requireOpen('connect')
    const existing = this.connections.get(nativePeerId)
    if (existing !== undefined && existing.state !== 'disconnected') {
      throw contractError('connection.already-owned', 'connection', 'rn-android-boundary.connect')
    }
    const connection = this.createConnection(nativePeerId)
    this.connections.set(nativePeerId, connection)
    try {
      await this.dispatch('connect', [field(10, connection.record), field(20, nativeConnectionIntent(intent))])
      connection.state = 'connected'
    } catch (error) {
      this.connections.delete(nativePeerId)
      throw error
    }
  }

  async disconnect(nativePeerId: string): Promise<void> {
    this.requireOpen('disconnect')
    const connection = this.requireConnection(nativePeerId, 'disconnect')
    await this.dispatch('disconnect', [field(10, connection.record)])
    connection.state = 'disconnected'
    this.databases.delete(nativePeerId)
  }

  connectionState(nativePeerId: string): 'connecting' | 'connected' | 'disconnected' {
    return this.connections.get(nativePeerId)?.state ?? 'disconnected'
  }

  async readRssi(nativePeerId: string): Promise<number> {
    this.requireOpen('read-rssi')
    const connection = this.requireConnection(nativePeerId, 'read-rssi')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.read-rssi')
    }
    const result = await this.dispatch('readRssi', [field(10, connection.record)])
    return requiredSigned(result, 13, 'rn-android-boundary.read-rssi.rssi')
  }

  async requestMtu(nativePeerId: string, requestedMtu: number): Promise<number> {
    this.requireOpen('request-mtu')
    const connection = this.requireConnection(nativePeerId, 'request-mtu')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.request-mtu')
    }
    const result = await this.dispatch('requestMtu', [field(10, connection.record), field(14, requestedMtu)])
    return requiredUnsigned(result, 14, 'rn-android-boundary.request-mtu.negotiated')
  }

  async effectiveMtu(nativePeerId: string): Promise<number | null> {
    this.requireOpen('effective-mtu')
    const connection = this.requireConnection(nativePeerId, 'effective-mtu')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.effective-mtu')
    }
    const result = await this.dispatch('readMtu', [field(10, connection.record)])
    if (requiredString(result, 2, 'rn-android-boundary.effective-mtu.kind') !== 'mtu') {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.effective-mtu.kind')
    }
    const value = optionalUnsigned(result, 22, 'rn-android-boundary.effective-mtu.value')
    if (value === null) return null
    if (value < MINIMUM_ATT_MTU || value > MAXIMUM_REQUESTED_ATT_MTU) {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.effective-mtu.range')
    }
    return value
  }

  async requestPriority(nativePeerId: string, priority: ConnectionPriority): Promise<boolean> {
    this.requireOpen('request-priority')
    const connection = this.requireConnection(nativePeerId, 'request-priority')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.request-priority')
    }
    const result = await this.dispatch('requestPriority', [
      field(10, connection.record),
      field(16, nativePriorityFromPublic(priority))
    ])
    if (requiredString(result, 2, 'rn-android-boundary.request-priority.kind') !== 'priority') {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.request-priority.kind')
    }
    return requiredBoolean(result, 18, 'rn-android-boundary.request-priority.accepted')
  }

  async readPhy(nativePeerId: string): Promise<CoreBluetoothPhyObservation> {
    this.requirePhyExtension('read-phy')
    this.requireOpen('read-phy')
    const connection = this.requireConnection(nativePeerId, 'read-phy')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.read-phy')
    }
    const result = await this.dispatch('readPhy', [field(10, connection.record)])
    if (requiredString(result, 2, 'rn-android-boundary.read-phy.kind') !== 'phy') {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.read-phy.kind')
    }
    return Object.freeze({
      txPhy: publicPhyFromNative(requiredString(result, 19, 'rn-android-boundary.read-phy.tx')),
      rxPhy: publicPhyFromNative(requiredString(result, 20, 'rn-android-boundary.read-phy.rx'))
    })
  }

  async requestPhy(nativePeerId: string, preference: PhyPreference): Promise<CoreBluetoothPhyRequestResult> {
    this.requirePhyExtension('request-phy')
    this.requireOpen('request-phy')
    const connection = this.requireConnection(nativePeerId, 'request-phy')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.request-phy')
    }
    if (preference.tx === undefined && preference.rx === undefined) {
      throw contractError('argument.invalid', 'connection', 'rn-android-boundary.request-phy.preference')
    }
    const fields: NativeProtocolField[] = [field(10, connection.record)]
    if (preference.tx !== undefined) fields.push(field(17, nativePhyFromPublic(preference.tx)))
    if (preference.rx !== undefined) fields.push(field(18, nativePhyFromPublic(preference.rx)))
    const result = await this.dispatch('requestPhy', fields)
    if (requiredString(result, 2, 'rn-android-boundary.request-phy.kind') !== 'phy') {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.request-phy.kind')
    }
    const accepted = requiredBoolean(result, 21, 'rn-android-boundary.request-phy.accepted')
    const tx = optionalString(result, 19)
    const rx = optionalString(result, 20)
    if (accepted !== (tx !== null && rx !== null)) {
      throw contractError('protocol.malformed', 'boundary', 'rn-android-boundary.request-phy.observation')
    }
    return Object.freeze({
      accepted,
      observation:
        tx === null || rx === null
          ? null
          : Object.freeze({ txPhy: publicPhyFromNative(tx), rxPhy: publicPhyFromNative(rx) })
    })
  }

  async securityState(nativePeerId: string): Promise<AndroidSecurityState> {
    this.requireSecurityExtension('security-state')
    this.requireOpen('security-state')
    const result = await this.dispatch('securityState', [field(15, nativePeerId)])
    return securityStateFromRecord(result, nativePeerId, 'rn-android-boundary.security-state')
  }

  async pair(
    nativePeerId: string,
    transport: SecurityPairOptions['transport'],
    signal: AbortSignal | null = null
  ): Promise<{ readonly outcome: 'paired' | 'already-paired' | 'rejected'; readonly state: AndroidSecurityState }> {
    this.requireSecurityExtension('pair')
    this.requireOpen('pair')
    if (isAbortSignalAborted(signal)) {
      throw contractError('operation.aborted', 'core', 'rn-android-boundary.pair')
    }
    const current = await this.securityState(nativePeerId)
    if (isAbortSignalAborted(signal)) {
      throw contractError('operation.aborted', 'core', 'rn-android-boundary.pair')
    }
    // Android's public bond state is not transport-specific. A generic bond is
    // sufficient for the platform default, but an explicit LE request must
    // reach the native device-type-aware check instead of silently accepting a
    // possible BR/EDR-only bond.
    if (transport === 'auto' && current.bond === 'bonded') {
      return { outcome: 'already-paired', state: current }
    }
    const nativeTransport = transport === 'auto' ? 'platformDefault' : transport
    const result = await this.dispatch('securityPair', [field(15, nativePeerId), field(19, nativeTransport)])
    const state = securityStateFromRecord(result, nativePeerId, 'rn-android-boundary.pair')
    return { outcome: state.bond === 'bonded' ? 'paired' : 'rejected', state }
  }

  async cancelPairing(nativePeerId: string): Promise<void> {
    this.requireSecurityCancellationExtension('cancel-pairing')
    this.requireOpen('cancel-pairing')
    await this.dispatch('securityCancelPairing', [field(15, nativePeerId)])
  }

  async cleanupPairing(nativePeerId: string): Promise<void> {
    this.requireSecurityExtension('pairing-cleanup')
    this.requireOpen('pairing-cleanup')
    await this.dispatch('securityCancelPairing', [field(15, nativePeerId)])
  }

  async unpair(_nativePeerId: string): Promise<'unsupported'> {
    this.requireSecurityExtension('unpair')
    return 'unsupported'
  }

  onSecurityState(listener: (record: AndroidSecurityStateChangedRecord) => void): () => void {
    this.securityListeners.add(listener)
    return () => this.securityListeners.delete(listener)
  }

  async discover(nativePeerId: string): Promise<CoreBluetoothGattSnapshot> {
    this.requireOpen('discover')
    const connection = this.requireConnection(nativePeerId, 'discover')
    if (connection.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'rn-android-boundary.discover')
    }
    const database = this.createDatabase(connection.record)
    const result = await this.dispatch('discover', [field(10, connection.record), field(11, database)])
    const snapshot = requiredRecord(result, 12, 'rn-android-boundary.discover.snapshot')
    this.databases.set(nativePeerId, database)
    return snapshotFromRecord(snapshot)
  }

  async read(address: CoreBluetoothCharacteristicAddress): Promise<Uint8Array> {
    this.requireOpen('read')
    const result = await this.dispatch('read', [field(4, this.characteristicPath(address))])
    const reference = binaryReferenceFromRecord(requiredRecord(result, 6, 'rn-android-boundary.read.binary'))
    return this.takeOutputBytes(reference, 'read')
  }

  async write(address: CoreBluetoothCharacteristicAddress, bytes: Uint8Array, withResponse: boolean): Promise<void> {
    this.requireOpen('write')
    if (bytes.byteLength > this.maximumInputPayloadBytes) {
      throw contractError('bytes.too-large', 'boundary', 'rn-android-boundary.write')
    }
    const correlation = this.nextCorrelation()
    const reference = retainNativeProtocolBytes(correlation.nonce, bytes)
    const command = commandRecord(protocolVersion, 'write', correlation.record, [
      field(4, this.characteristicPath(address)),
      field(6, binaryReferenceRecord(reference)),
      field(13, withResponse ? 'withResponse' : 'withoutResponse')
    ])
    await this.submit(command, correlation.nonce, 'write', () => {
      this.releaseOrRetainForTeardown(reference, 'write-input-dispatch-failure')
    })
  }

  async readDescriptor(address: CoreBluetoothDescriptorAddress): Promise<Uint8Array> {
    this.requireOpen('read-descriptor')
    const descriptorPath = this.descriptorPath(address)
    const result = await this.dispatch('readDescriptor', [field(5, descriptorPath)])
    this.assertDescriptorResultPath(
      descriptorPath,
      requiredRecord(result, 15, 'rn-android-boundary.read-descriptor.path'),
      'read-descriptor'
    )
    const reference = binaryReferenceFromRecord(requiredRecord(result, 6, 'rn-android-boundary.read-descriptor.binary'))
    return this.takeOutputBytes(reference, 'read-descriptor')
  }

  async writeDescriptor(address: CoreBluetoothDescriptorAddress, bytes: Uint8Array): Promise<void> {
    this.requireOpen('write-descriptor')
    if (bytes.byteLength > this.maximumInputPayloadBytes) {
      throw contractError('bytes.too-large', 'boundary', 'rn-android-boundary.write-descriptor')
    }
    const correlation = this.nextCorrelation()
    const reference = retainNativeProtocolBytes(correlation.nonce, bytes)
    const command = commandRecord(protocolVersion, 'writeDescriptor', correlation.record, [
      field(5, this.descriptorPath(address)),
      field(6, binaryReferenceRecord(reference))
    ])
    const result = await this.submit(command, correlation.nonce, 'writeDescriptor', () => {
      this.releaseOrRetainForTeardown(reference, 'write-descriptor-input-dispatch-failure')
    })
    this.assertDescriptorResultPath(
      requiredRecord(command, 5, 'rn-android-boundary.write-descriptor.path'),
      requiredRecord(result, 15, 'rn-android-boundary.write-descriptor.result-path'),
      'write-descriptor'
    )
  }

  async startNotify(address: CoreBluetoothCharacteristicAddress, onValue: (bytes: Uint8Array) => void): Promise<void> {
    return this.startNotifyWithMode(address, 'notification', onValue)
  }

  async startNotifyWithMode(
    address: CoreBluetoothCharacteristicAddress,
    mode: CoreBluetoothNotificationDeliveryMode,
    onValue: (bytes: Uint8Array) => void
  ): Promise<void> {
    this.requireOpen('start-notify')
    const key = addressKey(address)
    if (this.subscriptionsByAddress.has(key)) {
      throw contractError('lifecycle.invalid-state', 'gatt', 'rn-android-boundary.start-notify')
    }
    const subscriptionId = `rn-android-subscription-${this.nextSubscription}`
    this.nextSubscription += 1
    const subscription: NativeSubscription = { subscriptionId, address, onValue }
    // Native Android can emit a value immediately after CCCD enablement, before its terminal result arrives.
    this.subscriptionsByAddress.set(key, subscription)
    try {
      await this.dispatch('subscribe', [
        field(4, this.characteristicPath(address)),
        field(7, subscriptionId),
        field(21, mode)
      ])
    } catch (error) {
      this.subscriptionsByAddress.delete(key)
      throw error
    }
  }

  async stopNotify(address: CoreBluetoothCharacteristicAddress): Promise<void> {
    this.requireOpen('stop-notify')
    const key = addressKey(address)
    const subscription = this.subscriptionsByAddress.get(key)
    if (subscription === undefined) {
      return
    }
    await this.dispatch('unsubscribe', [
      field(4, this.characteristicPath(address)),
      field(7, subscription.subscriptionId)
    ])
    this.subscriptionsByAddress.delete(key)
  }

  onDisconnect(listener: (nativePeerId: string, safeMessage: string | null) => void): () => void {
    this.disconnectListeners.add(listener)
    return () => this.disconnectListeners.delete(listener)
  }

  onDatabaseChanged(listener: (nativePeerId: string) => void): () => void {
    this.databaseChangedListeners.add(listener)
    return () => this.databaseChangedListeners.delete(listener)
  }

  onScanFailure(listener: (safeMessage: string) => void): () => void {
    this.scanFailureListeners.add(listener)
    return () => this.scanFailureListeners.delete(listener)
  }

  onAdapterState(listener: (state: CoreBluetoothAdapterSnapshot) => void): () => void {
    this.adapterListeners.add(listener)
    return () => this.adapterListeners.delete(listener)
  }

  async destroy(): Promise<void> {
    if (this.destroyResult !== null) {
      return this.destroyResult
    }
    if (!this.opened && !this.nativeAttachmentOpened) {
      return
    }
    const destruction = this.destroyInternal()
    this.destroyResult = destruction.catch(error => {
      this.destroyResult = null
      throw error
    })
    return this.destroyResult
  }

  private async destroyInternal(): Promise<void> {
    this.closing = true
    const attachment = this.requireAttachmentRecord('destroy')
    this.destroyRequested = true
    try {
      if (this.opened && !this.nativeDestroyCompleted) {
        await this.dispatch('destroy', [])
        this.nativeDestroyCompleted = true
      }
    } catch (error) {
      console.error('[ReactNativeAndroidProtocolBoundary.destroy] Native protocol destroy failed:', error)
      throw error instanceof Error ? error : new Error('Native protocol destroy failed')
    }
    this.retryNativeReleaseLedger('destroy-before-close')
    try {
      await this.closeNativeAttachment(attachment)
    } catch (closeError) {
      console.error('[ReactNativeAndroidProtocolBoundary.destroy] Native attachment close failed:', closeError)
      throw closeError instanceof Error ? closeError : new Error('Native attachment close failed')
    }
    this.opened = false
    this.closing = false
    this.phyExtensionAvailable = false
    this.scanListeners.clear()
    this.scanFailureListeners.clear()
    this.connections.clear()
    this.databases.clear()
    this.subscriptionsByAddress.clear()
    this.disconnectListeners.clear()
    this.databaseChangedListeners.clear()
    this.adapterListeners.clear()
    this.securityListeners.clear()
    this.expectedLateTerminals.clear()
    this.rejectPending('Native protocol attachment was destroyed')
  }

  private async closeNativeAttachment(attachment: NativeProtocolRecord): Promise<void> {
    if (!this.nativeAttachmentOpened) {
      return
    }
    await this.control.closeAttachment(attachmentIdentityFromRecord(attachment))
    this.nativeAttachmentOpened = false
  }

  private async dispatch(kind: string, fields: readonly NativeProtocolField[]): Promise<NativeProtocolRecord> {
    const correlation = this.nextCorrelation()
    return this.submit(commandRecord(protocolVersion, kind, correlation.record, fields), correlation.nonce, kind)
  }

  private submit(
    command: NativeProtocolRecord,
    nonce: string,
    kind: string,
    onSubmissionFailure: (() => void) | null = null
  ): Promise<NativeProtocolRecord> {
    const key = operationKey(commandEpoch(command), nonce)
    const nativePeerId = nativePeerIdForCommand(command)
    return new Promise<NativeProtocolRecord>((resolve, reject) => {
      this.pending.set(key, { kind, nativePeerId, resolve, reject })
      try {
        submitNativeProtocolCommand(encodeNativeProtocolRecord(command))
      } catch (error) {
        this.pending.delete(key)
        onSubmissionFailure?.()
        reject(error instanceof Error ? error : new Error('Native protocol command submission failed'))
      }
    })
  }

  private receiveRecord(bytes: Uint8Array): void {
    let record: NativeProtocolRecord
    try {
      record = decodeNativeProtocolRecord(bytes)
    } catch (error) {
      this.rejectMalformedNativeRecord(error)
    }
    if (record.kind === 'result') {
      try {
        this.receiveResult(record)
      } catch (error) {
        this.rejectMalformedNativeRecord(error)
      }
      return
    }
    if (record.kind === 'event') {
      try {
        this.receiveEvent(record)
      } catch (error) {
        // A decoded event that cannot be materialized is quarantined after its
        // binary references have been released; it cannot settle a pending command.
        console.error('[ReactNativeAndroidProtocolBoundary.receiveRecord] Native record was rejected:', error)
      }
      return
    }
    this.rejectMalformedNativeRecord(
      contractError('protocol.malformed', 'boundary', 'rn-android-boundary.receive-record')
    )
  }

  private rejectMalformedNativeRecord(error: unknown): never {
    console.error('[ReactNativeAndroidProtocolBoundary.receiveRecord] Native record was rejected:', error)
    this.failAttachment('A malformed native protocol record invalidated pending operations')
    throw error instanceof Error ? error : new Error('Native protocol record decoding failed')
  }

  private receiveResult(result: NativeProtocolRecord): void {
    const terminal = requiredRecord(result, 3, 'rn-android-boundary.result.terminal')
    const correlation = requiredRecord(terminal, 1, 'rn-android-boundary.result.correlation')
    this.assertCurrentAttachment(
      requiredRecord(correlation, 1, 'rn-android-boundary.result.correlation-attachment'),
      'result-correlation'
    )
    const key = operationKey(
      requiredUnsigned(correlation, 2, 'rn-android-boundary.result.epoch'),
      requiredString(correlation, 3, 'rn-android-boundary.result.nonce')
    )
    const pending = this.pending.get(key)
    if (pending === undefined) {
      const expectedLateTerminal = this.expectedLateTerminals.get(key)
      if (
        expectedLateTerminal !== undefined &&
        sameAttachmentIdentity(
          expectedLateTerminal.attachment,
          attachmentIdentityFromRecord(this.requireAttachmentRecord('result-late-terminal'))
        )
      ) {
        // This terminal belongs to the exact operation that this boundary
        // already rejected because its peer was lost. Consume the tombstone;
        // a duplicate terminal must remain visible as a protocol diagnostic.
        this.expectedLateTerminals.delete(key)
        return
      }
      console.error('[ReactNativeAndroidProtocolBoundary.receiveResult] Late terminal result was quarantined:', { key })
      return
    }
    this.pending.delete(key)
    if (requiredString(terminal, 2, 'rn-android-boundary.result.outcome') === 'succeeded') {
      pending.resolve(result)
      return
    }
    pending.reject(nativeOperationFailure(optionalRecord(result, 10), pending.kind))
  }

  private receiveEvent(event: NativeProtocolRecord): void {
    const kind = requiredString(event, 3, 'rn-android-boundary.event.kind')
    if (kind === 'securityStateChanged') {
      this.assertCurrentAttachment(
        requiredRecord(event, 4, 'rn-android-boundary.event.security-attachment'),
        'security-event'
      )
      const nativePeerId = requiredString(event, 16, 'rn-android-boundary.event.security-peer')
      const bondState = requiredString(event, 17, 'rn-android-boundary.event.security-bond-state')
      const state = securityStateFromBondState(bondState, 'rn-android-boundary.event.security-state')
      for (const listener of this.securityListeners) {
        this.invokeConsumerListener('securityStateChanged', () => listener({ nativePeerId, state }))
      }
      return
    }
    if (kind === 'advertisement') {
      const advertisement = requiredRecord(event, 12, 'rn-android-boundary.event.advertisement')
      const parsedAdvertisement = parseAdvertisementRecord(advertisement)
      const advertisementBytes = this.takeAdvertisementBytes(parsedAdvertisement)
      const value = advertisementFromRecord(parsedAdvertisement, advertisementBytes)
      for (const listener of this.scanListeners) {
        this.invokeConsumerListener('advertisement', () => listener(value))
      }
      return
    }
    if (kind === 'notification') {
      const subscriptionId = requiredString(event, 11, 'rn-android-boundary.event.subscription')
      const reference = binaryReferenceFromRecord(requiredRecord(event, 13, 'rn-android-boundary.event.binary'))
      // A notification's payload must belong to the subscription that produced
      // it. The native codec enforces exactly this (requireBinaryCorrelation,
      // NativeProtocolV2Codec.cpp), and enforcing it here too is what stops the
      // deterministic layer from accepting a record the radio path rejects: a
      // test double that mints its own correlation models a notification that
      // can never actually be delivered, so the suite stays green while every
      // real notification is dropped. That is issue #168.
      const operation = requiredRecord(event, 10, 'rn-android-boundary.event.notification-operation')
      const operationNonce = requiredString(operation, 3, 'rn-android-boundary.event.notification-operation-nonce')
      if (reference.operationCorrelation !== operationNonce) {
        throw contractError(
          'protocol.violation',
          'boundary',
          'rn-android-boundary.event.notification-binary-correlation'
        )
      }
      const bytes = this.takeOutputBytes(reference, 'notification')
      const subscription = [...this.subscriptionsByAddress.values()].find(
        candidate => candidate.subscriptionId === subscriptionId
      )
      if (subscription === undefined) {
        console.error(
          '[ReactNativeAndroidProtocolBoundary.receiveEvent] Notification for an inactive subscription was quarantined:',
          { subscriptionId }
        )
        return
      }
      this.invokeConsumerListener('notification', () => subscription.onValue(bytes))
      return
    }
    if (kind === 'databaseChanged') {
      this.assertCurrentAttachment(
        requiredRecord(event, 4, 'rn-android-boundary.event.database-changed-attachment'),
        'database-changed-event'
      )
      const database = requiredRecord(event, 8, 'rn-android-boundary.event.database')
      const connectionPath = requiredRecord(database, 1, 'rn-android-boundary.event.database-connection')
      const nativePeerId = requiredString(connectionPath, 2, 'rn-android-boundary.event.database-peer')
      const connection = this.connections.get(nativePeerId)
      const currentDatabase = this.databases.get(nativePeerId)
      if (
        connection === undefined ||
        connection.state !== 'connected' ||
        currentDatabase === undefined ||
        !sameDatabasePath(currentDatabase, database) ||
        !sameConnectionPath(connection.record, connectionPath)
      ) {
        console.error(
          '[ReactNativeAndroidProtocolBoundary.receiveEvent] Stale databaseChanged event was quarantined:',
          { nativePeerId, databaseGeneration: optionalString(database, 3) }
        )
        return
      }
      if (this.databases.get(nativePeerId) === currentDatabase) {
        this.databases.delete(nativePeerId)
      }
      for (const listener of this.databaseChangedListeners) {
        this.invokeConsumerListener('databaseChanged', () => listener(nativePeerId))
      }
      return
    }
    if (kind === 'connectionLost') {
      const connection = requiredRecord(event, 7, 'rn-android-boundary.event.connection')
      const peerId = requiredString(connection, 2, 'rn-android-boundary.event.peer')
      const error = optionalRecord(event, 14)
      const safeMessage = error === null ? null : optionalString(error, 7)
      this.invalidateConnection(peerId)
      this.rejectPendingForPeer(peerId, safeMessage ?? 'Android GATT connection was lost')
      for (const listener of this.disconnectListeners) {
        this.invokeConsumerListener('connectionLost', () => listener(peerId, safeMessage))
      }
      return
    }
    if (kind === 'adapterState') {
      const state = adapterStateFromRecord(requiredRecord(event, 15, 'rn-android-boundary.event.adapter-state'))
      this.latestAdapterState = state
      for (const listener of this.adapterListeners) {
        this.invokeConsumerListener('adapterState', () => listener(state))
      }
      return
    }
    if (kind === 'diagnostic') {
      const error = optionalRecord(event, 14)
      const code = error === null ? null : optionalString(error, 1)
      const operation = error === null ? null : optionalString(error, 3)
      const safeMessage = error === null ? null : optionalString(error, 7)
      if (code === 'stream.overflow') {
        if (operation === 'pre-js-event-buffer') {
          this.preJavaScriptEventBufferOverflowed = true
        } else {
          this.failAttachment('Native Android event ingress overflowed')
        }
        console.error('[ReactNativeAndroidProtocolBoundary.receiveEvent] Native event buffer overflowed:', {
          operation,
          safeMessage
        })
        return
      }
      if (code === 'scanFailed') {
        const message = safeMessage ?? 'Android scan failed'
        this.scanListeners.clear()
        for (const listener of this.scanFailureListeners) {
          this.invokeConsumerListener('scanFailed', () => listener(message))
        }
        return
      }
      console.error('[ReactNativeAndroidProtocolBoundary.receiveEvent] Native diagnostic event received:', {
        code,
        safeMessage
      })
      return
    }
    console.error('[ReactNativeAndroidProtocolBoundary.receiveEvent] Unsupported native event was quarantined:', {
      kind
    })
  }

  private takeAdvertisementBytes(advertisement: ParsedNativeAdvertisement): Map<NativeBinaryReference, Uint8Array> {
    const bytesByReference = new Map<NativeBinaryReference, Uint8Array>()
    let firstFailure: Error | null = null
    for (const reference of advertisementBinaryReferences(advertisement)) {
      try {
        bytesByReference.set(reference, this.takeOutputBytes(reference, 'advertisement'))
      } catch (error) {
        const failure = error instanceof Error ? error : new Error('Native advertisement output copy failed')
        console.error(
          '[ReactNativeAndroidProtocolBoundary.takeAdvertisementBytes] Native advertisement output copy failed:',
          {
            ownerToken: reference.ownerToken,
            operationCorrelation: reference.operationCorrelation,
            error: failure
          }
        )
        if (firstFailure === null) {
          firstFailure = failure
        }
      }
    }
    if (firstFailure !== null) {
      throw firstFailure
    }
    return bytesByReference
  }

  private takeOutputBytes(reference: NativeBinaryReference, operation: string): Uint8Array {
    let output: Uint8Array | null = null
    let copyFailure: Error | null = null
    let releaseFailure: Error | null = null
    try {
      if (reference.byteLength > maximumNativePayloadBytes) {
        throw contractError('bytes.too-large', 'boundary', `rn-android-boundary.${operation}`)
      }
      output = new Uint8Array(copyNativeProtocolBytes(reference))
    } catch (error) {
      console.error('[ReactNativeAndroidProtocolBoundary.takeOutputBytes] Native output copy failed:', {
        operation,
        error
      })
      copyFailure = error instanceof Error ? error : new Error('Native output copy failed')
    } finally {
      try {
        releaseFailure = this.releaseOrRetainForTeardown(reference, `${operation}-output`)
      } catch (error) {
        releaseFailure = error instanceof Error ? error : new Error('Native output release failed')
      }
    }
    if (releaseFailure !== null) {
      console.error('[ReactNativeAndroidProtocolBoundary.takeOutputBytes] Native output release failed:', {
        operation,
        ownerToken: reference.ownerToken,
        operationCorrelation: reference.operationCorrelation,
        error: releaseFailure
      })
      throw releaseFailure
    }
    if (copyFailure !== null) {
      throw copyFailure
    }
    if (output === null) {
      throw contractError('lifecycle.invariant-violation', 'boundary', `rn-android-boundary.${operation}.copy`)
    }
    return output
  }

  private invokeConsumerListener(eventKind: string, invoke: () => void): void {
    try {
      invoke()
    } catch (error) {
      this.consumerListenerFailureCount += 1
      console.error('[ReactNativeAndroidProtocolBoundary.invokeConsumerListener] Consumer listener failed:', {
        metric: 'nativeProtocolConsumerListenerFailure',
        eventKind,
        failureCount: this.consumerListenerFailureCount,
        error: error instanceof Error ? error : new Error('Consumer listener threw a non-Error value')
      })
    }
  }

  private releaseOrRetainForTeardown(reference: NativeBinaryReference, operation: string): Error | null {
    const key = `${reference.ownerToken}:${reference.operationCorrelation}`
    try {
      const released = releaseNativeProtocolBytes(reference)
      if (released) {
        this.nativeReleaseRetryLedger.delete(key)
        return null
      }
      // A false return is the native proof that this exact owner was already
      // released. Retrying it would turn a completed release into a false leak.
      this.nativeReleaseRetryLedger.delete(key)
      console.error(
        '[ReactNativeAndroidProtocolBoundary.releaseOrRetainForTeardown] Native release was already terminal:',
        {
          metric: 'nativeProtocolBinaryReleaseAlreadyTerminal',
          operation,
          ownerToken: reference.ownerToken,
          operationCorrelation: reference.operationCorrelation
        }
      )
      return null
    } catch (error) {
      const failure = error instanceof Error ? error : new Error('Native binary release failed with a non-Error value')
      this.nativeReleaseRetryLedger.set(key, reference)
      console.error(
        '[ReactNativeAndroidProtocolBoundary.releaseOrRetainForTeardown] Native release retained for retry:',
        {
          metric: 'nativeProtocolBinaryReleaseRetryable',
          operation,
          ownerToken: reference.ownerToken,
          operationCorrelation: reference.operationCorrelation,
          retryLedgerSize: this.nativeReleaseRetryLedger.size,
          error: failure
        }
      )
      return failure
    }
  }

  private retryNativeReleaseLedger(operation: string): void {
    let firstFailure: Error | null = null
    for (const reference of [...this.nativeReleaseRetryLedger.values()]) {
      const failure = this.releaseOrRetainForTeardown(reference, operation)
      if (firstFailure === null && failure !== null) {
        firstFailure = failure
      }
    }
    if (firstFailure !== null) {
      throw new AggregateError([firstFailure], `Native binary cleanup remains retryable during ${operation}`)
    }
  }

  private failAttachment(reason: string): void {
    if (this.destroyResult !== null || this.fatalTeardownObservation !== null || !this.nativeAttachmentOpened) {
      return
    }
    this.opened = false
    this.closing = true
    this.destroyRequested = true
    this.nativeDestroyCompleted = true
    this.scanListeners.clear()
    this.scanFailureListeners.clear()
    this.connections.clear()
    this.databases.clear()
    this.subscriptionsByAddress.clear()
    this.disconnectListeners.clear()
    this.databaseChangedListeners.clear()
    this.adapterListeners.clear()
    this.securityListeners.clear()
    this.expectedLateTerminals.clear()
    this.rejectPending(reason)
    const attachment = this.requireAttachmentRecord('fatal-attachment')
    const teardown = (async () => {
      this.retryNativeReleaseLedger('fatal-attachment-before-close')
      await this.closeNativeAttachment(attachment)
      this.closing = false
    })()
    this.destroyResult = teardown.catch(error => {
      this.destroyResult = null
      throw error
    })
    this.fatalTeardownObservation = this.destroyResult.catch(error => {
      console.error(
        '[ReactNativeAndroidProtocolBoundary.failAttachment] Fatal attachment teardown remained unobserved:',
        error
      )
    })
  }

  private nextCorrelation(): { readonly nonce: string; readonly record: NativeProtocolRecord } {
    const dispatchEpoch = this.nextEpoch
    this.nextEpoch += 1
    const nonce = `rn-android-operation-${dispatchEpoch}`
    return {
      nonce,
      record: protocolRecord('operationCorrelation', [
        field(1, this.requireAttachmentRecord('next-correlation')),
        field(2, dispatchEpoch),
        field(3, nonce)
      ])
    }
  }

  private createConnection(nativePeerId: string): NativeConnection {
    // A new connection starts a new peer lifecycle. A terminal from the prior
    // lifecycle must not be silently accepted after this point.
    this.clearExpectedLateTerminalsForPeer(nativePeerId)
    const ordinal = this.nextConnection
    this.nextConnection += 1
    return {
      record: protocolRecord('connectionPath', [
        field(1, this.requireAttachmentRecord('create-connection')),
        field(2, nativePeerId),
        field(3, `rn-android-connection-${ordinal}`),
        field(4, `rn-android-lease-${ordinal}`),
        field(5, `rn-android-connection-generation-${ordinal}`)
      ]),
      state: 'connecting'
    }
  }

  private createDatabase(connection: NativeProtocolRecord): NativeProtocolRecord {
    const ordinal = this.nextDatabase
    this.nextDatabase += 1
    return protocolRecord('databasePath', [
      field(1, connection),
      field(2, `rn-android-database-${ordinal}`),
      field(3, `rn-android-database-generation-${ordinal}`)
    ])
  }

  private characteristicPath(address: CoreBluetoothCharacteristicAddress): NativeProtocolRecord {
    const database = this.databases.get(address.nativePeerId)
    if (database === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', 'rn-android-boundary.characteristic-path')
    }
    const service = protocolRecord('servicePath', [
      field(1, database),
      field(2, address.serviceUuid),
      field(3, String(address.serviceOccurrence))
    ])
    return protocolRecord('characteristicPath', [
      field(1, service),
      field(2, address.characteristicUuid),
      field(3, String(address.characteristicOccurrence))
    ])
  }

  private descriptorPath(address: CoreBluetoothDescriptorAddress): NativeProtocolRecord {
    return protocolRecord('descriptorPath', [
      field(1, this.characteristicPath(address)),
      field(2, address.descriptorUuid),
      field(3, String(address.descriptorOccurrence))
    ])
  }

  private assertDescriptorResultPath(
    expected: NativeProtocolRecord,
    actual: NativeProtocolRecord,
    operation: string
  ): void {
    const expectedBytes = encodeNativeProtocolRecord(expected)
    const actualBytes = encodeNativeProtocolRecord(actual)
    if (
      expectedBytes.byteLength !== actualBytes.byteLength ||
      expectedBytes.some((byte, index) => byte !== actualBytes[index])
    ) {
      throw contractError('protocol.violation', 'boundary', `rn-android-boundary.${operation}.descriptor-path`)
    }
  }

  private requireConnection(nativePeerId: string, operation: string): NativeConnection {
    const connection = this.connections.get(nativePeerId)
    if (connection === undefined) {
      throw contractError('connection.not-found', 'connection', `rn-android-boundary.${operation}`)
    }
    return connection
  }

  private requireOpen(operation: string): void {
    if (!this.opened || this.closing || this.destroyRequested) {
      throw contractError('lifecycle.destroyed', 'boundary', `rn-android-boundary.${operation}`)
    }
  }

  private requireSecurityExtension(operation: string): void {
    if (!this.securityExtensionAvailable) {
      throw contractError('capability.unsupported', 'capability', `rn-android-boundary.${operation}`)
    }
  }

  private requirePhyExtension(operation: string): void {
    if (!this.phyExtensionAvailable) {
      throw contractError('capability.unsupported', 'capability', `rn-android-boundary.${operation}`)
    }
  }

  private requireSecurityCancellationExtension(operation: string): void {
    if (!this.securityCancellationExtensionAvailable) {
      throw contractError('capability.unsupported', 'capability', `rn-android-boundary.${operation}`)
    }
  }

  private assertCurrentAttachment(record: NativeProtocolRecord, operation: string): void {
    const expected = attachmentIdentityFromRecord(this.requireAttachmentRecord(operation))
    const actual = attachmentIdentityFromRecord(record)
    if (
      actual.attachmentId !== expected.attachmentId ||
      actual.backendInstanceId !== expected.backendInstanceId ||
      actual.backendGeneration !== expected.backendGeneration ||
      actual.adapterId !== expected.adapterId ||
      actual.adapterGeneration !== expected.adapterGeneration
    ) {
      throw contractError('protocol.violation', 'boundary', `rn-android-boundary.${operation}.attachment-mismatch`)
    }
  }

  private requireAttachmentRecord(operation: string): NativeProtocolRecord {
    if (this.attachmentRecord === null) {
      throw contractError('lifecycle.invalid-state', 'boundary', `rn-android-boundary.${operation}.attachment`)
    }
    return this.attachmentRecord
  }

  private rejectPending(message: string): void {
    for (const pending of this.pending.values()) {
      pending.reject(new Error(message))
    }
    this.pending.clear()
  }

  private invalidateConnection(nativePeerId: string): void {
    this.connections.delete(nativePeerId)
    this.databases.delete(nativePeerId)
    for (const [key, subscription] of this.subscriptionsByAddress) {
      if (subscription.address.nativePeerId === nativePeerId) {
        this.subscriptionsByAddress.delete(key)
      }
    }
  }

  private rejectPendingForPeer(nativePeerId: string, message: string): void {
    const attachment = attachmentIdentityFromRecord(this.requireAttachmentRecord('reject-pending-for-peer'))
    for (const [key, pending] of this.pending) {
      // Android owns disconnect teardown until it emits that operation's
      // terminal result. A connectionLost event can arrive while that cleanup
      // is in flight; rejecting disconnect here would turn its later, valid
      // terminal into a protocol error and lose the authoritative cleanup
      // outcome. This mirrors the native dispatcher's connection-loss policy.
      if (pending.nativePeerId === nativePeerId && pending.kind !== 'disconnect') {
        this.pending.delete(key)
        this.recordExpectedLateTerminal(key, nativePeerId, attachment)
        pending.reject(
          contractError('connection.lost', 'connection', `rn-android-boundary.${pending.kind}`, {
            domain: 'android',
            code: 'connectionLost',
            safeMessage: message,
            metadata: Object.freeze({})
          })
        )
      }
    }
  }

  private recordExpectedLateTerminal(key: string, nativePeerId: string, attachment: NativeAttachmentIdentity): void {
    if (this.expectedLateTerminals.size >= maximumExpectedLateTerminals) {
      const oldest = this.expectedLateTerminals.keys().next().value
      if (typeof oldest === 'string') {
        this.expectedLateTerminals.delete(oldest)
      }
    }
    this.expectedLateTerminals.set(key, { nativePeerId, attachment })
  }

  private clearExpectedLateTerminalsForPeer(nativePeerId: string): void {
    for (const [key, tombstone] of this.expectedLateTerminals) {
      if (tombstone.nativePeerId === nativePeerId) {
        this.expectedLateTerminals.delete(key)
      }
    }
  }
}

function sameDatabasePath(left: NativeProtocolRecord, right: NativeProtocolRecord): boolean {
  if (left.kind !== 'databasePath' || right.kind !== 'databasePath') {
    return false
  }
  return (
    sameConnectionPath(
      requiredRecord(left, 1, 'rn-android-boundary.database-path.connection'),
      requiredRecord(right, 1, 'rn-android-boundary.database-path.connection')
    ) &&
    requiredString(left, 2, 'rn-android-boundary.database-path.id') ===
      requiredString(right, 2, 'rn-android-boundary.database-path.id') &&
    requiredString(left, 3, 'rn-android-boundary.database-path.generation') ===
      requiredString(right, 3, 'rn-android-boundary.database-path.generation')
  )
}

function sameConnectionPath(left: NativeProtocolRecord, right: NativeProtocolRecord): boolean {
  if (left.kind !== 'connectionPath' || right.kind !== 'connectionPath') {
    return false
  }
  return (
    sameAttachmentPath(
      requiredRecord(left, 1, 'rn-android-boundary.connection-path.attachment'),
      requiredRecord(right, 1, 'rn-android-boundary.connection-path.attachment')
    ) &&
    requiredString(left, 2, 'rn-android-boundary.connection-path.peer') ===
      requiredString(right, 2, 'rn-android-boundary.connection-path.peer') &&
    requiredString(left, 3, 'rn-android-boundary.connection-path.id') ===
      requiredString(right, 3, 'rn-android-boundary.connection-path.id') &&
    requiredString(left, 4, 'rn-android-boundary.connection-path.lease') ===
      requiredString(right, 4, 'rn-android-boundary.connection-path.lease') &&
    requiredString(left, 5, 'rn-android-boundary.connection-path.generation') ===
      requiredString(right, 5, 'rn-android-boundary.connection-path.generation')
  )
}

function sameAttachmentPath(left: NativeProtocolRecord, right: NativeProtocolRecord): boolean {
  if (left.kind !== 'attachment' || right.kind !== 'attachment') {
    return false
  }
  return (
    requiredString(left, 1, 'rn-android-boundary.attachment.id') ===
      requiredString(right, 1, 'rn-android-boundary.attachment.id') &&
    requiredString(left, 2, 'rn-android-boundary.attachment.backend-instance') ===
      requiredString(right, 2, 'rn-android-boundary.attachment.backend-instance') &&
    requiredString(left, 3, 'rn-android-boundary.attachment.backend-generation') ===
      requiredString(right, 3, 'rn-android-boundary.attachment.backend-generation') &&
    requiredString(left, 4, 'rn-android-boundary.attachment.adapter') ===
      requiredString(right, 4, 'rn-android-boundary.attachment.adapter') &&
    requiredString(left, 5, 'rn-android-boundary.attachment.adapter-generation') ===
      requiredString(right, 5, 'rn-android-boundary.attachment.adapter-generation')
  )
}

function sameAttachmentIdentity(left: NativeAttachmentIdentity, right: NativeAttachmentIdentity): boolean {
  return (
    left.attachmentId === right.attachmentId &&
    left.backendInstanceId === right.backendInstanceId &&
    left.backendGeneration === right.backendGeneration &&
    left.adapterId === right.adapterId &&
    left.adapterGeneration === right.adapterGeneration
  )
}

function isNativeProtocolRecord(value: unknown): value is NativeProtocolRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && 'kind' in value && 'fields' in value
}

function isAbortSignalAborted(signal: AbortSignal | null): boolean {
  return signal?.aborted === true
}

function nativePriorityFromPublic(priority: ConnectionPriority): 'lowPower' | 'balanced' | 'highThroughput' {
  if (priority === 'low-power') return 'lowPower'
  if (priority === 'balanced') return 'balanced'
  if (priority === 'high-throughput') return 'highThroughput'
  throw contractError('argument.invalid', 'connection', 'rn-android-boundary.request-priority.priority')
}

/** Preserves native platform details instead of flattening CoreBluetooth failures to plain Error. */
function nativeOperationFailure(error: NativeProtocolRecord | null, operation: string): Error {
  const safeMessage = error === null ? null : optionalString(error, 7)
  if (error === null) {
    return new Error(`Native ${operation} operation failed`)
  }
  const nativeDomain = optionalString(error, 9) ?? optionalString(error, 2) ?? 'native-protocol'
  const nativeCode = nativeErrorCode(error)
  if (nativeCode === 'cancelled') {
    return contractError('operation.aborted', 'core', `rn-android-boundary.${operation}`)
  }
  if (nativeCode === 'permissionDenied') {
    return contractError('permission.denied', 'adapter', `rn-android-boundary.${operation}`)
  }
  if (nativeCode === 'adapterUnavailable') {
    return contractError('adapter.unavailable', 'adapter', `rn-android-boundary.${operation}`)
  }
  if (nativeCode === 'adapterPoweredOff') {
    return contractError('adapter.powered-off', 'adapter', `rn-android-boundary.${operation}`)
  }
  if (nativeCode === 'adapterResetting') {
    return contractError('adapter.resetting', 'adapter', `rn-android-boundary.${operation}`)
  }
  if (nativeCode === 'connectionLost') {
    return contractError('connection.lost', 'connection', `rn-android-boundary.${operation}`, {
      domain: nativeDomain,
      code: nativeCode,
      safeMessage: safeMessage ?? `Native ${operation} operation ended because the connection was lost`,
      metadata: Object.freeze({})
    })
  }
  return contractError('platform.failure', 'platform', `rn-android-boundary.${operation}`, {
    domain: nativeDomain,
    code: nativeCode,
    safeMessage: safeMessage ?? `Native ${operation} operation failed`,
    metadata: Object.freeze({})
  })
}

function nativeErrorCode(error: NativeProtocolRecord): string {
  const coreBluetoothCode = error.fields.find(candidate => candidate.id === 10)?.value
  if (typeof coreBluetoothCode === 'number' && Number.isSafeInteger(coreBluetoothCode)) {
    return String(coreBluetoothCode)
  }
  return optionalString(error, 1) ?? 'native-error'
}

function assertHandshakeSelection(handshake: NativeProtocolHandshakeResult): void {
  if (
    handshake.nativeProtocol !== protocolVersion ||
    handshake.abi !== NATIVE_PROTOCOL_ABI_VERSION ||
    handshake.controlSurface !== controlSurfaceVersion ||
    handshake.backendContract !== contractVersion ||
    handshake.capabilitySchema !== contractVersion ||
    handshake.eventSchema !== contractVersion ||
    handshake.traceFormat !== contractVersion ||
    !Number.isSafeInteger(handshake.maximumControlRecordBytes) ||
    handshake.maximumControlRecordBytes <= 0 ||
    !Number.isSafeInteger(handshake.maximumBinaryPayloadBytes) ||
    handshake.maximumBinaryPayloadBytes <= 0 ||
    (handshake.securityAvailable !== undefined && typeof handshake.securityAvailable !== 'boolean') ||
    (handshake.securityCancelPairingAvailable !== undefined &&
      typeof handshake.securityCancelPairingAvailable !== 'boolean') ||
    (handshake.phyAvailable !== undefined && typeof handshake.phyAvailable !== 'boolean')
  ) {
    throw contractError('protocol.incompatible', 'boundary', 'rn-android-boundary.open.handshake')
  }
}

function securityStateFromRecord(
  record: NativeProtocolRecord,
  expectedPeerId: string,
  operation: string
): AndroidSecurityState {
  const peerId = requiredString(record, 16, `${operation}.peer`)
  if (peerId !== expectedPeerId) throw contractError('protocol.violation', 'boundary', `${operation}.peer-mismatch`)
  return securityStateFromBondState(requiredString(record, 17, `${operation}.bond-state`), operation)
}

function securityStateFromBondState(value: string, operation: string): AndroidSecurityState {
  if (
    value !== 'bonded' &&
    value !== 'bonding' &&
    value !== 'notBonded' &&
    value !== 'unknown' &&
    value !== 'unsupported'
  ) {
    throw contractError('protocol.malformed', 'boundary', `${operation}.bond-state`)
  }
  const bond = value === 'notBonded' ? 'not-bonded' : value
  return Object.freeze({
    bond,
    encryption: 'unsupported',
    authentication: 'unsupported',
    secureConnections: 'unsupported',
    pairingPossible: value === 'bonded' || value === 'bonding' || value === 'notBonded' ? true : null
  })
}

function androidScanMode(platform: CoreBluetoothScanPlatformOptions | undefined): number {
  if (platform?.mode === 'low-power') return 0
  if (platform?.mode === 'balanced') return 1
  if (platform?.mode === 'opportunistic') return -1
  return 2
}

function androidScanCallbackType(platform: CoreBluetoothScanPlatformOptions | undefined): number {
  return platform?.callbackType === 'first-match' ? 2 : 1
}

function androidScanLegacy(platform: CoreBluetoothScanPlatformOptions | undefined): boolean {
  return platform?.legacy !== false
}
