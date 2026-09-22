// src/backends/desktop/desktop-rust-core-binding.ts
//
// The JS face of the shared desktop Rust core: the `UbmCentral` surface of
// the N-API addon (`bindings/napi/src/dispatch.rs`), the binding the
// provider opens centrals through, and the verbatim mapping of the addon's
// error wire form onto contract errors. Types only plus pure mapping: the
// addon itself is loaded by `src/desktop-core-addon.ts`.

import { BackendContractError, BLE_ERROR_CODES, BLE_ERROR_DOMAINS, contractError } from '../../backend-contract/errors'
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

export interface DesktopRustCoreAdvertisement {
  readonly peerId: string
  readonly address?: string | null
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
  readonly source?: 'advertisement' | 'device-state'
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
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid?: string | null
  readonly characteristicOccurrence?: number | null
  readonly descriptorUuid?: string | null
  readonly descriptorOccurrence?: number | null
  readonly properties: number
  readonly access?: DesktopRustCoreCharacteristicAccess | null
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
  readonly kind: 'terminal' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly operationId?: string | null
  readonly aborted?: boolean | null
  readonly detail?: string | null
  readonly missed?: number | null
}

/** One row of the core's capability registration for a desktop OS. */
export interface DesktopRustCoreCapabilityState {
  readonly id: string
  readonly state: 'limited' | 'unsupported'
  readonly limitation?: string | null
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

/** Typed notification poll outcome (F17). */
export interface DesktopRustCoreNotificationPoll {
  readonly kind: 'value' | 'empty' | 'terminal' | 'invalidated' | 'closed'
  readonly value?: Uint8Array | null
  readonly cause?: string | null
  readonly droppedItems?: number | null
  readonly droppedBytes?: number | null
  readonly replacedItems?: number | null
}

/** One connection-lifecycle event from the core, or a gap marker. */
export interface DesktopRustCoreLifecycleEvent {
  /** `adapter-lost`: the core released this link because the adapter was lost (LEGACY-AUDIT-1 #57). */
  readonly kind: 'link-lost' | 'released' | 'services-changed' | 'adapter-lost' | 'lagged' | 'closed'
  readonly sequence?: number | null
  readonly peerId?: string | null
  readonly peerKey?: string | null
  readonly connectionGeneration?: string | null
  readonly requested?: boolean | null
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
export interface DesktopRustCoreCentral {
  createTicket(): string
  cancelTicket(ticket: string): Promise<DesktopRustCoreTicketCancel>
  releaseTicket(ticket: string): boolean
  adapterName(): Promise<string>
  adapterState(options?: DesktopRustCoreControl): Promise<DesktopRustCoreAdapterPower>
  takeAdapterEvent(): Promise<DesktopRustCoreAdapterEvent | null>
  takeLifecycleEvent(): Promise<DesktopRustCoreLifecycleEvent | null>
  dispatchCounters(): DesktopRustCoreDispatchCounters
  startScan(
    options: {
      readonly owner: string
      readonly serviceUuids: string[]
      /** The OS duplicate filter policy (`all` when absent). */
      readonly duplicatePolicy?: 'all' | 'first' | 'merged'
      /**
       * The local-name prefix the OS filter narrows by (BlueZ
       * `SetDiscoveryFilter` `Pattern`; CoreBluetooth and WinRT ignore it).
       * It only narrows: the caller's software match stays the final filter.
       */
      readonly localNamePrefix?: string
    } & DesktopRustCoreControl
  ): Promise<{ operationId: string }>
  stopScan(operationId: string, options?: DesktopRustCoreControl): Promise<'stopped' | 'not-active'>
  /** The live scan's next observation with its scan id and age (`null` when none waits). */
  takeScanObservation(): Promise<DesktopRustCoreScanObservation | null>
  connect(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<{
    peerKey: string
    connectionGeneration: string | null
  }>
  disconnect(
    options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl
  ): Promise<'released' | 'already-released'>
  readRssi(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<number>
  /**
   * Effective ATT MTU of the live link, as the OS reports it (finding 217
   * follow-up): macOS `maximumWriteValueLength(.withResponse) + 3`, Windows
   * `GattSession.MaxPduSize`, Linux the BlueZ characteristic MTU.
   */
  readEffectiveMtu(
    options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl
  ): Promise<number>
  /**
   * Registers the whole snapshot or rejects with a typed error: a malformed
   * platform UUID is `protocol.malformed` (`discovery.snapshot.uuid`), a
   * database past the ATT handle space `capability.limited`
   * (`discovery.database-bound`). Nothing is skipped.
   */
  discover(options: { readonly peerId: string; readonly lease: string } & DesktopRustCoreControl): Promise<{
    pathsRegistered: number
  }>
  discoveredPaths(peerId: string): Promise<DesktopRustCorePath[]>
  /** The value and the radio's provenance word (`read-response` | `read-or-notification`). */
  read(
    options: { readonly peerId: string; readonly selector: DesktopRustCoreSelector } & DesktopRustCoreControl
  ): Promise<{ readonly value: Uint8Array; readonly provenance: string }>
  write(
    options: {
      readonly peerId: string
      readonly selector: DesktopRustCoreSelector
      readonly value: Uint8Array
      readonly mode?: string
    } & DesktopRustCoreControl
  ): Promise<void>
  readDescriptor(
    options: { readonly peerId: string; readonly selector: DesktopRustCoreSelector } & DesktopRustCoreControl
  ): Promise<Uint8Array>
  writeDescriptor(
    options: {
      readonly peerId: string
      readonly selector: DesktopRustCoreSelector
      readonly value: Uint8Array
    } & DesktopRustCoreControl
  ): Promise<void>
  subscribe(
    options: {
      readonly peerId: string
      readonly selector: DesktopRustCoreSelector
      readonly consumer: string
      readonly deliveryMode?: 'notification' | 'indication'
      /** The consumer's overflow policy in the core (`error` when absent). */
      readonly overflowPolicy?: 'error' | 'drop-oldest' | 'drop-newest' | 'latest'
    } & DesktopRustCoreControl
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
  /** The core operation id of the owned scan, `null` when none (the re-read after a scan-terminal lag). */
  activeScanId(): string | null
  takeAdapterResetEvent(): Promise<DesktopRustCoreAdapterResetEvent | null>
  adapterStatus(): DesktopRustCoreAdapterStatus
  /** Resolves once the adapter is usable; `capability.unavailable` / `adapter.initialize` after `withinMs`. */
  awaitUsableAdapter(withinMs: number): Promise<DesktopRustCoreAdapterPower>
  installPairingGenerationController(
    read: (adapterId: string) => Promise<string>,
    set: (adapterId: string, generation: string) => Promise<void>
  ): void
}

/** Native Rust core entry: opens one dispatch central on the chosen radio. */
/** The BlueZ D-Bus bus a central runs on (the legacy `busKind`). */
export type DesktopRustCoreBluezBus = 'system' | 'session'

/** Host-supplied privileged pairing-generation operations (BlueZ; see `BluezPairingGenerationController`). */
export interface DesktopRustCoreGenerationController {
  read(adapterId: string): Promise<'legacy-only' | 'enabled' | 'required'>
  set(adapterId: string, generation: 'legacy-only' | 'enabled' | 'required'): Promise<void>
}

export interface DesktopRustCoreBinding {
  openProduction(options: {
    readonly owner: string
    readonly platform: DesktopRustCorePlatform
    readonly adapterId: string | null
    readonly bluezBus?: DesktopRustCoreBluezBus
    readonly pairingGeneration?: boolean
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
  const entries: Record<string, string | number | boolean> = {}
  for (const key of Object.keys(metadata)) {
    const entry: unknown = Reflect.get(metadata, key)
    if (typeof entry !== 'string' && typeof entry !== 'number' && typeof entry !== 'boolean') return undefined
    entries[key] = entry
  }
  return Object.freeze({ domain, code, message, metadata: Object.freeze(entries) })
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
            metadata: wire.platform.metadata
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
