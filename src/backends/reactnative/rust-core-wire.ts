// src/backends/reactnative/rust-core-wire.ts
//
// TS half of wire revision `ubm-mobile-wire/1` (the Rust half is
// `crates/ubm-mobile/src/wire.rs`). Pure functions over JSON text and
// values: no host globals, no Node APIs, no mutation of inputs. Every
// exported function returns a `WireResult`; a malformed input yields a
// structured `BackendContractError` and never a partial value.

import {
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  BLE_RETRYABILITIES,
  BackendContractError,
  contractError,
  type BleErrorCode,
  type BleErrorDomain,
  type BleRetryability,
  type PlatformErrorDetail
} from '../../backend-contract/errors'
import type { AdapterAuthorization, AdapterAvailability, AdapterPower } from '../../backend-contract/identity'
import type { BlePeerState, PeerSource, ResourceCounters } from '../../backend-contract/backend'
import {
  READ_PROVENANCES,
  type CancellationAcknowledgement,
  type ReadProvenance,
  type WriteMode
} from '../../backend-contract/operations'
import type { StreamTerminalNotice } from '../../backend-contract/streams'

export const WIRE_REVISION = 'ubm-mobile-wire/1'
/** Frozen C-UBM per-operation byte ceiling (`contracts/src/bounds.ts`, `ubm_core::contracts`). */
export const MAX_OPERATION_BYTES = 524288
/** Longest padded base64 text that can encode `MAX_OPERATION_BYTES`. */
export const MAX_BASE64_LENGTH = 4 * Math.ceil(MAX_OPERATION_BYTES / 3)
/** Bound on any JSON text crossing the native boundary, in UTF-8 bytes. */
export const MAX_WIRE_TEXT_BYTES = 1048576
/** Union of `ubm_core::central::GATT_PROP_*` bits the core can report. */
export const GATT_PROPERTY_MASK = 0x1f

const OPERATION_PREFIX = 'react-native-rust-core.wire'

export type WireResult<Value> =
  | { readonly ok: true; readonly value: Value }
  | { readonly ok: false; readonly error: BackendContractError }

// -- closed vocabularies -------------------------------------------------

export const WIRE_OPS = Object.freeze([
  'adapter.state',
  'counters.describe',
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
  'connection.read-phy',
  'connection.request-phy',
  'connection.maximum-write-length',
  'security.state',
  'security.pair',
  'security.cancel-pairing',
  'background.acquire',
  'background.release',
  'background.update-notification',
  'companion.associate',
  'presence.observe',
  'presence.unobserve',
  'gatt.discover',
  'gatt.read',
  'gatt.read-descriptor',
  'gatt.write',
  'gatt.write-descriptor',
  'gatt.subscribe',
  'gatt.unsubscribe',
  'op.cancel',
  'session.reconcile',
  'session.dispose'
] as const)
export type WireOp = (typeof WIRE_OPS)[number]

const WRITE_OPS: readonly WireOp[] = Object.freeze(['gatt.write', 'gatt.write-descriptor'])

export const DELIVERY_KINDS = Object.freeze(['notification', 'indication', 'unknown'] as const)
export type WireDelivery = (typeof DELIVERY_KINDS)[number]

export const COMMIT_STATES = Object.freeze(['not-dispatched', 'uncertain'] as const)
export type WireCommit = (typeof COMMIT_STATES)[number]

export const WRITE_COMMIT_STATES = Object.freeze(['confirmed', 'unknown'] as const)
export type WireWriteCommitState = (typeof WRITE_COMMIT_STATES)[number]

const ADAPTER_AVAILABILITY = Object.freeze([
  'available',
  'unavailable',
  'unsupported',
  'unknown'
] as const satisfies readonly AdapterAvailability[])
const ADAPTER_AUTHORIZATION = Object.freeze([
  'granted',
  'denied',
  'restricted',
  'not-determined',
  'unavailable',
  'unknown'
] as const satisfies readonly AdapterAuthorization[])
const ADAPTER_POWER = Object.freeze([
  'on',
  'off',
  'resetting',
  'unsupported',
  'unknown'
] as const satisfies readonly AdapterPower[])
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
const SECURITY_BOND = Object.freeze(['bonded', 'bonding', 'not-bonded', 'unknown', 'unsupported'] as const)
const SECURITY_ENCRYPTION = Object.freeze(['encrypted', 'not-encrypted', 'unknown', 'unsupported'] as const)
const SECURITY_AUTHENTICATION = Object.freeze(['authenticated', 'unauthenticated', 'unknown', 'unsupported'] as const)
const SECURITY_SECURE_CONNECTIONS = Object.freeze(['yes', 'no', 'unknown', 'unsupported'] as const)
const PAIR_OUTCOMES = Object.freeze(['paired', 'already-paired', 'rejected'] as const)
/** ATT MTU bounds the radio can report (23 minimum, 517 Android maximum request). */
const ATT_MTU: IntegerRange = { min: 23, max: 517 }
/** One byte up to the ATT maximum attribute value (Core Spec Vol 3 Part F §3.2.9). */
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
export const SCAN_END_REASONS = Object.freeze([
  'closed',
  'overflow',
  'source-failed',
  'owner-released',
  'connection-lost',
  'service-changed',
  'operation-aborted',
  'operation-timed-out'
] as const satisfies readonly StreamTerminalNotice['reason'][])
export const STREAM_END_REASONS = Object.freeze(['overflow', 'invalidated', 'closed'] as const)
export const LINK_REASONS = Object.freeze(['local', 'peer', 'adapter'] as const)
export const DATABASE_STATES = Object.freeze(['undiscovered', 'discovering', 'current', 'changed', 'invalid'] as const)
export const INGRESS_CLASSES = Object.freeze(['advertisement', 'notification', 'control'] as const)

// -- result types --------------------------------------------------------

/**
 * The platform's own error identity a radio failure carried (finding 113):
 * Android `{domain:"android", code:<native code>, metadata:{androidGattStatus}}`,
 * Apple the `NSError` domain and code, as legacy React Native reported them.
 */
export interface WirePlatformDetail {
  readonly domain: string
  readonly code: string
  readonly message: string | null
  readonly metadata: Readonly<Record<string, number | string | boolean>>
}

export interface WireRemoteFailure {
  readonly code: BleErrorCode
  readonly domain: BleErrorDomain
  readonly operation: string
  readonly detail: string | null
  readonly platform: WirePlatformDetail | null
}

export type WireInvokeEnvelope =
  | { readonly kind: 'value'; readonly value: unknown }
  | {
      readonly kind: 'failure'
      readonly failure: WireRemoteFailure
      readonly commit: WireCommit | null
      /** The owner's own answer; never re-derived from the code. */
      readonly retryability: BleRetryability
    }

export interface WireAdapterState {
  readonly availability: AdapterAvailability
  readonly authorization: AdapterAuthorization
  readonly power: AdapterPower
  readonly safeReason: string | null
  readonly updatedAt: number
  readonly backendGeneration: string
  readonly adapterGeneration: string
}

export type WireResourceCounters = { readonly [Key in keyof ResourceCounters]: number }

/**
 * `counters.describe`: `counters`/`native` describe the resources the asking
 * session holds (its lease namespace), so every manager reports its own and
 * returns to baseline independently; `process` names the whole process
 * owner that every session shares.
 */
export interface WireCounters {
  readonly counters: WireResourceCounters
  readonly native: {
    readonly pendingRadioRequests: number
    readonly liveOps: number
  }
  readonly process: {
    readonly counters: WireResourceCounters
    readonly native: {
      readonly pendingRadioRequests: number
      readonly lateRadioCompletions: number
      readonly ingressDrops: { readonly advertisement: number; readonly notification: number; readonly control: number }
      readonly liveOps: number
    }
  }
}

export interface WirePeerRecord {
  readonly peerId: string
  readonly name: string | null
  readonly rssi: number | null
  readonly source: PeerSource
  readonly reachability: BlePeerState['reachability']
  readonly connection: BlePeerState['connection']
  readonly bond: BlePeerState['bond']
  readonly lastSeenAtMonotonicMs: number | null
}

export interface WireSecurityState {
  readonly bond: (typeof SECURITY_BOND)[number]
  readonly encryption: (typeof SECURITY_ENCRYPTION)[number]
  readonly authentication: (typeof SECURITY_AUTHENTICATION)[number]
  readonly secureConnections: (typeof SECURITY_SECURE_CONNECTIONS)[number]
  readonly pairingPossible: boolean | null
}

export interface WirePhyObservation {
  readonly tx: WirePhy
  readonly rx: WirePhy
}

export interface WireRestoredPeer {
  readonly peerId: string
  readonly name: string | null
  readonly connected: boolean
}

/**
 * One link in `session.reconcile`: a link the owner holds connected, or the
 * latest link end it reported for the peer (what its `link` record said).
 */
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

/** One of this session's consumers in `session.reconcile`. */
export type WireReconcileSubscription =
  | { readonly consumer: string; readonly state: 'live' }
  | {
      readonly consumer: string
      readonly state: 'ended'
      readonly reason: (typeof STREAM_END_REASONS)[number]
      readonly droppedItems: number
      readonly droppedBytes: number
    }

/**
 * `session.reconcile`: every fact a control record carries, re-read from the
 * owner after records were lost at its full control queue (104/105).
 */
export interface WireReconcile {
  readonly adapter: WireAdapterState
  readonly links: readonly WireReconcileLink[]
  readonly subscriptions: readonly WireReconcileSubscription[]
  readonly security: readonly { readonly peerId: string; readonly state: WireSecurityState }[]
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
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: readonly WireCharacteristic[]
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

export interface WireOpResults {
  readonly 'adapter.state': WireAdapterState
  readonly 'counters.describe': WireCounters
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
  readonly 'connection.read-phy': WirePhyObservation
  readonly 'connection.request-phy': { readonly accepted: boolean; readonly observation: WirePhyObservation | null }
  /** The platform's largest single write in the requested mode, bounded by the ATT maximum attribute value. */
  readonly 'connection.maximum-write-length': { readonly maximumWriteLength: number }
  readonly 'security.state': WireSecurityState
  readonly 'security.pair': { readonly outcome: (typeof PAIR_OUTCOMES)[number]; readonly state: WireSecurityState }
  readonly 'security.cancel-pairing': { readonly state: 'requested' }
  readonly 'background.acquire': { readonly leaseId: string }
  readonly 'background.release': WireCleanupRecord
  readonly 'background.update-notification': { readonly state: 'updated' }
  readonly 'presence.observe': { readonly state: 'observing' }
  readonly 'presence.unobserve': { readonly state: 'idle' }
  readonly 'companion.associate': {
    readonly source: 'associated'
    readonly associationId: number
    readonly peerId: string | null
    readonly displayName: string | null
  }
  readonly 'gatt.discover': WireDiscovery
  readonly 'gatt.read': { readonly value: Uint8Array; readonly provenance: ReadProvenance }
  readonly 'gatt.read-descriptor': { readonly value: Uint8Array }
  readonly 'gatt.write': { readonly commitState: WireWriteCommitState }
  readonly 'gatt.write-descriptor': { readonly commitState: WireWriteCommitState }
  readonly 'gatt.subscribe': { readonly consumer: string; readonly delivery: WireDelivery }
  readonly 'gatt.unsubscribe': { readonly state: 'released'; readonly physicalDisabled: boolean }
  readonly 'op.cancel': { readonly state: CancellationAcknowledgement<string>['state'] }
  readonly 'session.reconcile': WireReconcile
  readonly 'session.dispose': WireCleanupRecord
}

export interface WireAdvertisementRecord {
  readonly t: 'adv'
  readonly ordinal: number
  readonly peerId: string
  readonly localName: string | null
  readonly rssi: number | null
  readonly txPower: number | null
  readonly serviceUuids: readonly string[] | null
  readonly manufacturerData: readonly { readonly companyId: number; readonly payload: Uint8Array }[] | null
  readonly serviceData: readonly { readonly uuid: string; readonly payload: Uint8Array }[] | null
  readonly connectable: boolean | null
  readonly solicitedServiceUuids: readonly string[] | null
  readonly overflowServiceUuids: readonly string[] | null
  /** GAP Appearance (0..65535); null when not carried or not reported (CoreBluetooth never reports it). */
  readonly appearance: number | null
  /** The raw advertising record bytes (Android `ScanRecord`); null when not reported. */
  readonly rawRecord: Uint8Array | null
  readonly observedAtMs: number
}

export type WireDrainRecord =
  | WireAdvertisementRecord
  | {
      readonly t: 'scan-end'
      readonly ordinal: number
      readonly operationId: string
      readonly reason: (typeof SCAN_END_REASONS)[number]
    }
  | {
      readonly t: 'value'
      readonly ordinal: number
      readonly consumer: string
      readonly value: Uint8Array
      readonly delivery: WireDelivery
    }
  | {
      readonly t: 'stream-end'
      readonly ordinal: number
      readonly consumer: string
      readonly reason: (typeof STREAM_END_REASONS)[number]
      readonly droppedItems: number
      readonly droppedBytes: number
    }
  | { readonly t: 'adapter'; readonly ordinal: number; readonly state: WireAdapterState }
  | {
      readonly t: 'link'
      readonly ordinal: number
      readonly peerId: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string | null
      readonly reason: (typeof LINK_REASONS)[number]
    }
  | {
      readonly t: 'db-changed'
      readonly ordinal: number
      readonly peerId: string
      readonly connectionGeneration: string
      readonly databaseGeneration: string
    }
  | {
      readonly t: 'ingress-drop'
      readonly ordinal: number
      readonly class: (typeof INGRESS_CLASSES)[number]
      readonly count: number
    }
  | { readonly t: 'security'; readonly ordinal: number; readonly peerId: string; readonly state: WireSecurityState }
  | { readonly t: 'restored'; readonly ordinal: number; readonly peers: readonly WireRestoredPeer[] }

export interface WireDrainBatch {
  readonly more: boolean
  readonly records: readonly WireDrainRecord[]
}

export type WireJson = string | number | boolean | null | readonly WireJson[] | { readonly [key: string]: WireJson }
export type WireJsonObject = { readonly [key: string]: WireJson }

// -- fault plumbing ------------------------------------------------------

function operationName(path: string): string {
  return `${OPERATION_PREFIX}.${path}`
}

function malformed(path: string): BackendContractError {
  return contractError('protocol.malformed', 'core', operationName(path))
}

/**
 * Runs a parser that signals faults by throwing `BackendContractError` and
 * converts them to a `WireResult`. Anything else is a defect in this module
 * and propagates rather than being disguised as a wire fault.
 */
function capture<Value>(parse: () => Value): WireResult<Value> {
  try {
    return { ok: true, value: parse() }
  } catch (error) {
    if (error instanceof BackendContractError) return { ok: false, error }
    throw error
  }
}

// -- base64 (RFC 4648 §4, padded, strict) --------------------------------

const BASE64_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
const PAD = 0x3d
const BASE64_VALUES = (() => {
  const table = new Int8Array(128).fill(-1)
  for (let index = 0; index < BASE64_ALPHABET.length; index += 1) {
    table[BASE64_ALPHABET.charCodeAt(index)] = index
  }
  return table
})()

/** Encodes bytes (views included) as padded base64; rejects oversize input. */
export function encodeBase64(bytes: Readonly<Uint8Array>): WireResult<string> {
  return capture(() => {
    if (!(bytes instanceof Uint8Array)) {
      throw contractError('argument.invalid', 'gatt', operationName('bytes'))
    }
    if (bytes.byteLength > MAX_OPERATION_BYTES) {
      throw contractError('bytes.too-large', 'gatt', operationName('bytes'))
    }
    const sextet = (chunk: number, shift: number): string => BASE64_ALPHABET.charAt((chunk >>> shift) & 0x3f)
    let text = ''
    let chunk = 0
    let filled = 0
    for (const byte of bytes) {
      chunk = (chunk << 8) | byte
      filled += 1
      if (filled === 3) {
        text += sextet(chunk, 18) + sextet(chunk, 12) + sextet(chunk, 6) + sextet(chunk, 0)
        chunk = 0
        filled = 0
      }
    }
    if (filled === 1) {
      text += `${sextet(chunk << 16, 18)}${sextet(chunk << 16, 12)}==`
    } else if (filled === 2) {
      text += `${sextet(chunk << 8, 18)}${sextet(chunk << 8, 12)}${sextet(chunk << 8, 6)}=`
    }
    return text
  })
}

/** Decodes strict padded base64 from the core into an owned `Uint8Array`. */
export function decodeBase64(text: unknown, path: string): WireResult<Uint8Array> {
  return capture(() => decodeBase64OrThrow(text, path))
}

function base64Value(text: string, index: number, path: string): number {
  const code = text.charCodeAt(index)
  const value = code < 128 ? (BASE64_VALUES[code] ?? -1) : -1
  if (value < 0) throw malformed(path)
  return value
}

function decodeBase64OrThrow(text: unknown, path: string): Uint8Array {
  if (typeof text !== 'string') throw malformed(path)
  // Size first, from the length alone: nothing is scanned or allocated for
  // an input that cannot fit the operation byte ceiling.
  if (text.length > MAX_BASE64_LENGTH) {
    throw contractError('bytes.too-large', 'core', operationName(path))
  }
  if (text.length % 4 !== 0) throw malformed(path)
  if (text.length === 0) return new Uint8Array(0)
  const padding = text.charCodeAt(text.length - 1) !== PAD ? 0 : text.charCodeAt(text.length - 2) !== PAD ? 1 : 2
  const decodedLength = (text.length / 4) * 3 - padding
  if (decodedLength > MAX_OPERATION_BYTES) {
    throw contractError('bytes.too-large', 'core', operationName(path))
  }
  const bytes = new Uint8Array(decodedLength)
  const fullQuads = padding === 0 ? text.length : text.length - 4
  let offset = 0
  for (let index = 0; index < fullQuads; index += 4) {
    const chunk =
      (base64Value(text, index, path) << 18) |
      (base64Value(text, index + 1, path) << 12) |
      (base64Value(text, index + 2, path) << 6) |
      base64Value(text, index + 3, path)
    bytes[offset] = chunk >>> 16
    bytes[offset + 1] = (chunk >>> 8) & 0xff
    bytes[offset + 2] = chunk & 0xff
    offset += 3
  }
  if (padding > 0) {
    const last = text.length - 4
    const first = base64Value(text, last, path)
    const second = base64Value(text, last + 1, path)
    if (padding === 2) {
      if ((second & 0x0f) !== 0) throw malformed(path)
      bytes[offset] = (first << 2) | (second >>> 4)
    } else {
      const third = base64Value(text, last + 2, path)
      if ((third & 0x03) !== 0) throw malformed(path)
      bytes[offset] = (first << 2) | (second >>> 4)
      bytes[offset + 1] = ((second & 0x0f) << 4) | (third >>> 2)
    }
  }
  return bytes
}

// -- bounded JSON text ---------------------------------------------------

/** UTF-8 length of `text`, stopping as soon as it exceeds `limit`. */
function utf8LengthExceeds(text: string, limit: number): boolean {
  let bytes = 0
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index)
    if (code < 0x80) {
      bytes += 1
    } else if (code < 0x800) {
      bytes += 2
    } else if (code >= 0xd800 && code <= 0xdbff && index + 1 < text.length) {
      const next = text.charCodeAt(index + 1)
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4
        index += 1
      } else {
        bytes += 3
      }
    } else {
      bytes += 3
    }
    if (bytes > limit) return true
  }
  return false
}

function assertTextWithinBound(text: string, path: string, domain: BleErrorDomain): void {
  // A UTF-16 code unit is never less than one UTF-8 byte, so the length
  // check is a sound early reject before the exact byte count.
  if (text.length > MAX_WIRE_TEXT_BYTES || utf8LengthExceeds(text, MAX_WIRE_TEXT_BYTES)) {
    throw contractError('bytes.too-large', domain, operationName(path))
  }
}

function parseWireTextOrThrow(text: unknown, path: string): unknown {
  if (typeof text !== 'string') throw malformed(path)
  assertTextWithinBound(text, path, 'core')
  try {
    const parsed: unknown = JSON.parse(text)
    return parsed
  } catch (error) {
    if (error instanceof SyntaxError) throw malformed(path)
    throw error
  }
}

/** Parses JSON text from the native side after checking the 1 MiB bound. */
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

/**
 * Serializes `invoke` args. Rejects anything `JSON.stringify` would drop or
 * reshape (typed arrays, `undefined`, non-finite numbers, class instances):
 * bytes cross this boundary only as `…B64` strings from `encodeBase64`.
 */
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

// -- structural readers --------------------------------------------------

function isPlainObject(value: object): boolean {
  const prototype: unknown = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

/** A JSON object whose key set is exactly `keys`. */
function exactObject(value: unknown, keys: readonly string[], path: string): ReadonlyMap<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || !isPlainObject(value)) {
    throw malformed(path)
  }
  const entries = new Map<string, unknown>(Object.entries(value))
  if (entries.size !== keys.length || keys.some(key => !entries.has(key))) throw malformed(path)
  return entries
}

function arrayOf<Item>(
  value: unknown,
  path: string,
  parse: (entry: unknown, entryPath: string) => Item
): readonly Item[] {
  if (!Array.isArray(value)) throw malformed(path)
  return Object.freeze(value.map((entry: unknown, index: number) => parse(entry, `${path}[${index}]`)))
}

function nullable<Item>(value: unknown, path: string, parse: (entry: unknown, entryPath: string) => Item): Item | null {
  return value === null ? null : parse(value, path)
}

interface IntegerRange {
  readonly min: number
  readonly max: number
}

const NON_NEGATIVE: IntegerRange = { min: 0, max: Number.MAX_SAFE_INTEGER }
const POSITIVE: IntegerRange = { min: 1, max: Number.MAX_SAFE_INTEGER }
const COMPANY_ID: IntegerRange = { min: 0, max: 0xffff }
const SIGNED_BYTE: IntegerRange = { min: -128, max: 127 }
const SIGNED_SAFE: IntegerRange = { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER }

function integerOrThrow(value: unknown, range: IntegerRange, path: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < range.min || value > range.max) {
    throw malformed(path)
  }
  return value === 0 ? 0 : value
}

function enumOrThrow<Member extends string>(value: unknown, members: readonly Member[], path: string): Member {
  const member = members.find(candidate => candidate === value)
  if (member === undefined) throw malformed(path)
  return member
}

function stringOrThrow(value: unknown, path: string): string {
  if (typeof value !== 'string' || value.length === 0) throw malformed(path)
  return value
}

function optionalTextOrThrow(value: unknown, path: string): string | null {
  if (value === null) return null
  if (typeof value !== 'string') throw malformed(path)
  return value
}

function booleanOrThrow(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') throw malformed(path)
  return value
}

const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

/** The core emits `canonical_uuid` output only: lowercase, dashed, 128-bit. */
function uuidOrThrow(value: unknown, path: string): string {
  if (typeof value !== 'string' || !CANONICAL_UUID.test(value)) throw malformed(path)
  return value
}

function propertyMaskOrThrow(value: unknown, path: string): number {
  const mask = integerOrThrow(value, { min: 0, max: 0xff }, path)
  if ((mask & ~GATT_PROPERTY_MASK) !== 0) throw malformed(path)
  return mask
}

function nonEmptyArrayOrNull<Item>(
  value: unknown,
  path: string,
  parse: (entry: unknown, entryPath: string) => Item
): readonly Item[] | null {
  // Revision 1 sends an empty collection as `null`; one spelling per fact.
  if (value === null) return null
  const items = arrayOf(value, path, parse)
  if (items.length === 0) throw malformed(path)
  return items
}

function assertUniqueInstances(items: readonly { readonly uuid: string; readonly occurrence: number }[], path: string) {
  const seen = new Set<string>()
  for (const item of items) {
    const key = `${item.uuid}#${item.occurrence}`
    if (seen.has(key)) throw malformed(path)
    seen.add(key)
  }
}

// -- exported validators -------------------------------------------------

/** Safe non-negative integer (≤ 2^53−1); fractions, NaN and ±Infinity are rejected, never rounded. */
export function parseOccurrence(value: unknown, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, NON_NEGATIVE, path))
}

/** Safe integer within `[min, max]`. */
export function parseSafeInteger(value: unknown, range: IntegerRange, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, range, path))
}

export function parseCompanyId(value: unknown, path: string): WireResult<number> {
  return capture(() => integerOrThrow(value, COMPANY_ID, path))
}

/** Characteristic property bits limited to `GATT_PROPERTY_MASK`. */
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

/** Core-issued generation identity: an opaque non-empty string. */
export function parseGeneration(value: unknown, path: string): WireResult<string> {
  return capture(() => stringOrThrow(value, path))
}

// -- envelope ------------------------------------------------------------

function platformDetailOrThrow(value: unknown, path: string): WirePlatformDetail {
  const fields = exactObject(value, ['domain', 'code', 'message', 'metadata'], path)
  const metadataValue = fields.get('metadata')
  if (typeof metadataValue !== 'object' || metadataValue === null || Array.isArray(metadataValue)) {
    throw malformed(`${path}.metadata`)
  }
  const metadata: Record<string, number | string | boolean> = {}
  for (const [key, entry] of Object.entries(metadataValue)) {
    if (typeof entry === 'number') metadata[key] = integerOrThrow(entry, SIGNED_SAFE, `${path}.metadata.${key}`)
    else if (typeof entry === 'string' || typeof entry === 'boolean') metadata[key] = entry
    else throw malformed(`${path}.metadata.${key}`)
  }
  return Object.freeze({
    domain: stringOrThrow(fields.get('domain'), `${path}.domain`),
    code: stringOrThrow(fields.get('code'), `${path}.code`),
    message: optionalTextOrThrow(fields.get('message'), `${path}.message`),
    metadata: Object.freeze(metadata)
  })
}

/**
 * The owner always sends `platform` (null when the failure did not come
 * from the radio); a native facade's own rejection has no such key.
 */
function remoteFailureOrThrow(value: unknown, path: string): WireRemoteFailure {
  const keys = ['code', 'domain', 'operation', 'detail']
  const withPlatform = typeof value === 'object' && value !== null && Object.hasOwn(value, 'platform')
  const fields = exactObject(value, withPlatform ? [...keys, 'platform'] : keys, path)
  return Object.freeze({
    code: enumOrThrow(fields.get('code'), BLE_ERROR_CODES, `${path}.code`),
    domain: enumOrThrow(fields.get('domain'), BLE_ERROR_DOMAINS, `${path}.domain`),
    operation: stringOrThrow(fields.get('operation'), `${path}.operation`),
    detail: optionalTextOrThrow(fields.get('detail'), `${path}.detail`),
    platform: withPlatform ? nullable(fields.get('platform'), `${path}.platform`, platformDetailOrThrow) : null
  })
}

function wireOpOrThrow(op: unknown, path: string): WireOp {
  const member = WIRE_OPS.find(candidate => candidate === op)
  if (member === undefined) throw contractError('argument.invalid', 'core', operationName(path))
  return member
}

/**
 * Parses the string `invoke` resolves with. `commit` is required non-null
 * on write failures and required null everywhere else.
 */
export function parseInvokeEnvelope(text: unknown, op: WireOp): WireResult<WireInvokeEnvelope> {
  return capture(() => {
    const knownOp = wireOpOrThrow(op, 'envelope.op')
    const path = `${knownOp}.envelope`
    const value = parseWireTextOrThrow(text, path)
    if (typeof value !== 'object' || value === null || Array.isArray(value)) throw malformed(path)
    const ok = new Map<string, unknown>(Object.entries(value)).get('ok')
    if (ok === true) {
      const fields = exactObject(value, ['ok', 'value'], path)
      return Object.freeze({ kind: 'value', value: fields.get('value') })
    }
    if (ok !== false) throw malformed(`${path}.ok`)
    const fields = exactObject(value, ['ok', 'error', 'commit', 'retryability'], path)
    const failure = remoteFailureOrThrow(fields.get('error'), `${path}.error`)
    const commit = nullable(fields.get('commit'), `${path}.commit`, (entry, entryPath) =>
      enumOrThrow(entry, COMMIT_STATES, entryPath)
    )
    if (WRITE_OPS.includes(knownOp) !== (commit !== null)) throw malformed(`${path}.commit`)
    const retryability = enumOrThrow(fields.get('retryability'), BLE_RETRYABILITIES, `${path}.retryability`)
    if (commit === 'uncertain' && retryability !== 'never') throw malformed(`${path}.retryability`)
    return Object.freeze({ kind: 'failure', failure, commit, retryability })
  })
}

/**
 * The platform detail a remote failure reports. A radio failure carries the
 * platform's own identity, as legacy React Native reported it (finding 113);
 * an owner failure carries its detail text under the `ubm-mobile` domain.
 */
export function remotePlatformDetail(
  failure: Pick<WireRemoteFailure, 'code' | 'operation' | 'detail' | 'platform'>
): PlatformErrorDetail | null {
  if (failure.platform !== null) {
    return Object.freeze({
      domain: failure.platform.domain,
      code: failure.platform.code,
      safeMessage: failure.platform.message ?? failure.detail ?? `Native ${failure.operation} operation failed`,
      metadata: failure.platform.metadata
    })
  }
  return failure.detail === null
    ? null
    : Object.freeze({ domain: 'ubm-mobile', code: failure.code, safeMessage: failure.detail, metadata: {} })
}

/** The contract error a remote failure reports. */
export function remoteFailureError(failure: WireRemoteFailure): BackendContractError {
  return contractError(failure.code, failure.domain, failure.operation, remotePlatformDetail(failure))
}

/**
 * The contract error a failure envelope reports: the owner's retryability,
 * and its commit state on writes (a write that may have committed is never
 * retryable — the parser refuses an envelope that says otherwise).
 */
export function failureEnvelopeError(
  envelope: Extract<WireInvokeEnvelope, { readonly kind: 'failure' }>
): BackendContractError {
  const error = remoteFailureError(envelope.failure)
  return new BackendContractError({
    ...error.normalized,
    retryability: envelope.retryability,
    ...(envelope.commit === null ? {} : { commit: envelope.commit })
  })
}

/**
 * Parses the text a native facade rejects with: the structured wire failure
 * `{code, domain, operation, detail}` Rust minted. Any other text is
 * `protocol.malformed` at `path`, never a guessed identity.
 */
export function parseRemoteFailureText(text: unknown, path: string): WireResult<WireRemoteFailure> {
  return capture(() => remoteFailureOrThrow(parseWireTextOrThrow(text, path), path))
}

// -- admission and host services -------------------------------------------

/** Rust's `openSession` admission record (docs/MOBILE_RUST_WIRE.md). */
export interface WireAdmission {
  /** The session id as the decimal string every later native call takes. */
  readonly sessionId: string
  readonly contractRevision: string
  readonly wireRevision: string
  /** The binding's `ubm-native-build-identity/1` record, still unparsed. */
  readonly buildIdentity: unknown
}

/** Parses the admission JSON `openSession` resolves with. */
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

/** The one bound on host-supplied random bytes per call (legacy `getRandomBytes`). */
export const MAX_RANDOM_BYTES = 1024

/** Validates a random-byte request length before anything crosses the bridge. */
export function checkRandomByteLength(length: unknown): WireResult<number> {
  return capture(() => {
    if (typeof length !== 'number' || !Number.isSafeInteger(length) || length < 1 || length > MAX_RANDOM_BYTES) {
      throw contractError('argument.invalid', 'core', operationName('random-bytes.length'))
    }
    return length
  })
}

/** Decodes `randomBytes` base64; the byte count must equal the request. */
export function parseRandomBytesText(text: unknown, length: number): WireResult<Uint8Array> {
  return capture(() => {
    const bytes = decodeBase64OrThrow(text, 'random-bytes')
    if (bytes.byteLength !== length) throw malformed('random-bytes.length')
    return bytes
  })
}

/** The app-declared restoration identity `restorationIdentity` answers. */
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

/** Parses the `restorationIdentity` JSON with its exact key set. */
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

/**
 * Parses the answer to `restorationIdentity('{}')`: the identity the app
 * configured natively (Info.plist), or `null` when it configured none.
 */
export function parseConfiguredRestorationIdentityText(text: unknown): WireResult<WireRestorationIdentity | null> {
  if (text === 'null') return Object.freeze({ ok: true, value: null })
  return parseRestorationIdentityText(text)
}

// -- op result parsers ---------------------------------------------------

function adapterStateOrThrow(value: unknown, path: string): WireAdapterState {
  const fields = exactObject(
    value,
    ['availability', 'authorization', 'power', 'safeReason', 'updatedAt', 'backendGeneration', 'adapterGeneration'],
    path
  )
  return Object.freeze({
    availability: enumOrThrow(fields.get('availability'), ADAPTER_AVAILABILITY, `${path}.availability`),
    authorization: enumOrThrow(fields.get('authorization'), ADAPTER_AUTHORIZATION, `${path}.authorization`),
    power: enumOrThrow(fields.get('power'), ADAPTER_POWER, `${path}.power`),
    safeReason: optionalTextOrThrow(fields.get('safeReason'), `${path}.safeReason`),
    updatedAt: integerOrThrow(fields.get('updatedAt'), NON_NEGATIVE, `${path}.updatedAt`),
    backendGeneration: stringOrThrow(fields.get('backendGeneration'), `${path}.backendGeneration`),
    adapterGeneration: stringOrThrow(fields.get('adapterGeneration'), `${path}.adapterGeneration`)
  })
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
    ['pendingRadioRequests', 'lateRadioCompletions', 'ingressDrops', 'liveOps'],
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

/** `released` carries no failures and `release-failed` at least one: a record never contradicts itself. */
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
  const fields = exactObject(value, ['uuid', 'occurrence', 'characteristics'], path)
  const characteristics = arrayOf(fields.get('characteristics'), `${path}.characteristics`, characteristicOrThrow)
  assertUniqueInstances(characteristics, `${path}.characteristics`)
  return Object.freeze({
    uuid: uuidOrThrow(fields.get('uuid'), `${path}.uuid`),
    occurrence: integerOrThrow(fields.get('occurrence'), NON_NEGATIVE, `${path}.occurrence`),
    characteristics
  })
}

function discoveryOrThrow(value: unknown, path: string): WireDiscovery {
  const fields = exactObject(value, ['connectionGeneration', 'databaseGeneration', 'services'], path)
  const services = arrayOf(fields.get('services'), `${path}.services`, serviceOrThrow)
  assertUniqueInstances(services, `${path}.services`)
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

/** A characteristic read carries the radio's own provenance; a reply without one is malformed. */
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

function securityStateOrThrow(value: unknown, path: string): WireSecurityState {
  const fields = exactObject(
    value,
    ['bond', 'encryption', 'authentication', 'secureConnections', 'pairingPossible'],
    path
  )
  return Object.freeze({
    bond: enumOrThrow(fields.get('bond'), SECURITY_BOND, `${path}.bond`),
    encryption: enumOrThrow(fields.get('encryption'), SECURITY_ENCRYPTION, `${path}.encryption`),
    authentication: enumOrThrow(fields.get('authentication'), SECURITY_AUTHENTICATION, `${path}.authentication`),
    secureConnections: enumOrThrow(
      fields.get('secureConnections'),
      SECURITY_SECURE_CONNECTIONS,
      `${path}.secureConnections`
    ),
    pairingPossible: nullable(fields.get('pairingPossible'), `${path}.pairingPossible`, booleanOrThrow)
  })
}

function phyObservationOrThrow(value: unknown, path: string): WirePhyObservation {
  const fields = exactObject(value, ['tx', 'rx'], path)
  return Object.freeze({
    tx: enumOrThrow(fields.get('tx'), PHYS, `${path}.tx`),
    rx: enumOrThrow(fields.get('rx'), PHYS, `${path}.rx`)
  })
}

function restoredPeerOrThrow(value: unknown, path: string): WireRestoredPeer {
  const fields = exactObject(value, ['peerId', 'name', 'connected'], path)
  return Object.freeze({
    peerId: stringOrThrow(fields.get('peerId'), `${path}.peerId`),
    name: optionalTextOrThrow(fields.get('name'), `${path}.name`),
    connected: booleanOrThrow(fields.get('connected'), `${path}.connected`)
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
  const fields = exactObject(value, ['consumer', 'state', 'reason', 'droppedItems', 'droppedBytes'], path)
  return Object.freeze({
    consumer: stringOrThrow(fields.get('consumer'), `${path}.consumer`),
    state: enumOrThrow(fields.get('state'), ['ended'] as const, `${path}.state`),
    reason: enumOrThrow(fields.get('reason'), STREAM_END_REASONS, `${path}.reason`),
    droppedItems: integerOrThrow(fields.get('droppedItems'), NON_NEGATIVE, `${path}.droppedItems`),
    droppedBytes: integerOrThrow(fields.get('droppedBytes'), NON_NEGATIVE, `${path}.droppedBytes`)
  })
}

function reconcileOrThrow(value: unknown, path: string): WireReconcile {
  const fields = exactObject(value, ['adapter', 'links', 'subscriptions', 'security', 'restored', 'scan'], path)
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
    restored: arrayOf(fields.get('restored'), `${path}.restored`, restoredPeerOrThrow),
    scan: nullable(fields.get('scan'), `${path}.scan`, stringOrThrow)
  })
}

type OpParsers = { readonly [Op in WireOp]: (value: unknown, path: string) => WireOpResults[Op] }

const OP_PARSERS: OpParsers = Object.freeze({
  'adapter.state': adapterStateOrThrow,
  'counters.describe': countersOrThrow,
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
      source: enumOrThrow(fields.get('source'), ['associated'] as const, `${path}.source`),
      associationId: integerOrThrow(
        fields.get('associationId'),
        { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER },
        `${path}.associationId`
      ),
      peerId: optionalTextOrThrow(fields.get('peerId'), `${path}.peerId`),
      displayName: optionalTextOrThrow(fields.get('displayName'), `${path}.displayName`)
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
  'gatt.read': characteristicReadOrThrow,
  'gatt.read-descriptor': readValueOrThrow,
  'gatt.write': writeReceiptOrThrow,
  'gatt.write-descriptor': writeReceiptOrThrow,
  'gatt.subscribe': (value: unknown, path: string) => {
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

/** Parses the `value` of a successful envelope for `op` (spec §2 table). */
export function parseOpValue<Op extends WireOp>(op: Op, value: unknown): WireResult<WireOpResults[Op]> {
  return capture(() => {
    wireOpOrThrow(op, 'op')
    return OP_PARSERS[op](value, op)
  })
}

/**
 * A without-response write cannot be confirmed by the radio, so a
 * `confirmed` receipt for one is a stronger fact than the platform can
 * report and is rejected. A weaker `unknown` for with-response is honest.
 */
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

// -- drain ---------------------------------------------------------------

const DRAIN_RECORD_TYPES = Object.freeze([
  'adv',
  'scan-end',
  'value',
  'stream-end',
  'adapter',
  'link',
  'db-changed',
  'ingress-drop',
  'security',
  'restored'
] as const)

function advertisementOrThrow(fields: ReadonlyMap<string, unknown>, ordinal: number, path: string) {
  const record: WireAdvertisementRecord = {
    t: 'adv',
    ordinal,
    peerId: stringOrThrow(fields.get('peerId'), `${path}.peerId`),
    localName: optionalTextOrThrow(fields.get('localName'), `${path}.localName`),
    rssi: nullable(fields.get('rssi'), `${path}.rssi`, (entry, entryPath) =>
      integerOrThrow(entry, SIGNED_BYTE, entryPath)
    ),
    txPower: nullable(fields.get('txPower'), `${path}.txPower`, (entry, entryPath) =>
      integerOrThrow(entry, SIGNED_BYTE, entryPath)
    ),
    serviceUuids: nonEmptyArrayOrNull(fields.get('serviceUuids'), `${path}.serviceUuids`, uuidOrThrow),
    manufacturerData: nonEmptyArrayOrNull(
      fields.get('manufacturerData'),
      `${path}.manufacturerData`,
      (entry, entryPath) => {
        const item = exactObject(entry, ['companyId', 'payloadB64'], entryPath)
        return Object.freeze({
          companyId: integerOrThrow(item.get('companyId'), COMPANY_ID, `${entryPath}.companyId`),
          payload: decodeBase64OrThrow(item.get('payloadB64'), `${entryPath}.payloadB64`)
        })
      }
    ),
    serviceData: nonEmptyArrayOrNull(fields.get('serviceData'), `${path}.serviceData`, (entry, entryPath) => {
      const item = exactObject(entry, ['uuid', 'payloadB64'], entryPath)
      return Object.freeze({
        uuid: uuidOrThrow(item.get('uuid'), `${entryPath}.uuid`),
        payload: decodeBase64OrThrow(item.get('payloadB64'), `${entryPath}.payloadB64`)
      })
    }),
    connectable: nullable(fields.get('connectable'), `${path}.connectable`, booleanOrThrow),
    solicitedServiceUuids: nonEmptyArrayOrNull(
      fields.get('solicitedServiceUuids'),
      `${path}.solicitedServiceUuids`,
      uuidOrThrow
    ),
    overflowServiceUuids: nonEmptyArrayOrNull(
      fields.get('overflowServiceUuids'),
      `${path}.overflowServiceUuids`,
      uuidOrThrow
    ),
    appearance: nullable(fields.get('appearance'), `${path}.appearance`, (entry, entryPath) =>
      integerOrThrow(entry, COMPANY_ID, entryPath)
    ),
    rawRecord: nullable(fields.get('rawRecordB64'), `${path}.rawRecordB64`, decodeBase64OrThrow),
    observedAtMs: integerOrThrow(fields.get('observedAtMs'), NON_NEGATIVE, `${path}.observedAtMs`)
  }
  return Object.freeze(record)
}

function drainRecordOrThrow(value: unknown, path: string): WireDrainRecord {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw malformed(path)
  const type = enumOrThrow(new Map<string, unknown>(Object.entries(value)).get('t'), DRAIN_RECORD_TYPES, `${path}.t`)
  const recordPath = `${path}<${type}>`
  const read = (keys: readonly string[]) => {
    const fields = exactObject(value, ['t', 'ordinal', ...keys], recordPath)
    return { fields, ordinal: integerOrThrow(fields.get('ordinal'), NON_NEGATIVE, `${recordPath}.ordinal`) }
  }
  switch (type) {
    case 'adv': {
      const { fields, ordinal } = read([
        'peerId',
        'localName',
        'rssi',
        'txPower',
        'serviceUuids',
        'manufacturerData',
        'serviceData',
        'connectable',
        'solicitedServiceUuids',
        'overflowServiceUuids',
        'appearance',
        'rawRecordB64',
        'observedAtMs'
      ])
      return advertisementOrThrow(fields, ordinal, recordPath)
    }
    case 'scan-end': {
      const { fields, ordinal } = read(['operationId', 'reason'])
      return Object.freeze({
        t: type,
        ordinal,
        operationId: stringOrThrow(fields.get('operationId'), `${recordPath}.operationId`),
        reason: enumOrThrow(fields.get('reason'), SCAN_END_REASONS, `${recordPath}.reason`)
      })
    }
    case 'value': {
      const { fields, ordinal } = read(['consumer', 'valueB64', 'delivery'])
      return Object.freeze({
        t: type,
        ordinal,
        consumer: stringOrThrow(fields.get('consumer'), `${recordPath}.consumer`),
        value: decodeBase64OrThrow(fields.get('valueB64'), `${recordPath}.valueB64`),
        delivery: enumOrThrow(fields.get('delivery'), DELIVERY_KINDS, `${recordPath}.delivery`)
      })
    }
    case 'stream-end': {
      const { fields, ordinal } = read(['consumer', 'reason', 'droppedItems', 'droppedBytes'])
      return Object.freeze({
        t: type,
        ordinal,
        consumer: stringOrThrow(fields.get('consumer'), `${recordPath}.consumer`),
        reason: enumOrThrow(fields.get('reason'), STREAM_END_REASONS, `${recordPath}.reason`),
        droppedItems: integerOrThrow(fields.get('droppedItems'), NON_NEGATIVE, `${recordPath}.droppedItems`),
        droppedBytes: integerOrThrow(fields.get('droppedBytes'), NON_NEGATIVE, `${recordPath}.droppedBytes`)
      })
    }
    case 'adapter': {
      const { fields, ordinal } = read(['state'])
      return Object.freeze({ t: type, ordinal, state: adapterStateOrThrow(fields.get('state'), `${recordPath}.state`) })
    }
    case 'link': {
      const { fields, ordinal } = read(['peerId', 'connectionGeneration', 'databaseGeneration', 'reason'])
      return Object.freeze({
        t: type,
        ordinal,
        peerId: stringOrThrow(fields.get('peerId'), `${recordPath}.peerId`),
        connectionGeneration: stringOrThrow(fields.get('connectionGeneration'), `${recordPath}.connectionGeneration`),
        databaseGeneration: nullable(
          fields.get('databaseGeneration'),
          `${recordPath}.databaseGeneration`,
          stringOrThrow
        ),
        reason: enumOrThrow(fields.get('reason'), LINK_REASONS, `${recordPath}.reason`)
      })
    }
    case 'db-changed': {
      const { fields, ordinal } = read(['peerId', 'connectionGeneration', 'databaseGeneration'])
      return Object.freeze({
        t: type,
        ordinal,
        peerId: stringOrThrow(fields.get('peerId'), `${recordPath}.peerId`),
        connectionGeneration: stringOrThrow(fields.get('connectionGeneration'), `${recordPath}.connectionGeneration`),
        databaseGeneration: stringOrThrow(fields.get('databaseGeneration'), `${recordPath}.databaseGeneration`)
      })
    }
    case 'ingress-drop': {
      const { fields, ordinal } = read(['class', 'count'])
      return Object.freeze({
        t: type,
        ordinal,
        class: enumOrThrow(fields.get('class'), INGRESS_CLASSES, `${recordPath}.class`),
        count: integerOrThrow(fields.get('count'), POSITIVE, `${recordPath}.count`)
      })
    }
    case 'security': {
      const { fields, ordinal } = read(['peerId', 'state'])
      return Object.freeze({
        t: type,
        ordinal,
        peerId: stringOrThrow(fields.get('peerId'), `${recordPath}.peerId`),
        state: securityStateOrThrow(fields.get('state'), `${recordPath}.state`)
      })
    }
    case 'restored': {
      const { fields, ordinal } = read(['peers'])
      return Object.freeze({
        t: type,
        ordinal,
        peers: arrayOf(fields.get('peers'), `${recordPath}.peers`, restoredPeerOrThrow)
      })
    }
  }
}

function drainBatchOrThrow(value: unknown, lastOrdinal: number | null, path: string): WireDrainBatch {
  const fields = exactObject(value, ['more', 'records'], path)
  const more = booleanOrThrow(fields.get('more'), `${path}.more`)
  const records = arrayOf(fields.get('records'), `${path}.records`, drainRecordOrThrow)
  let previous = lastOrdinal
  records.forEach((record, index) => {
    if (previous !== null && record.ordinal <= previous) throw malformed(`${path}.records[${index}].ordinal`)
    previous = record.ordinal
  })
  return Object.freeze({ more, records })
}

/**
 * Parses a parsed `drain` result. Ordinals are session-monotonic: each must
 * exceed `lastOrdinal` (the last one already delivered, or `null` for none)
 * and its predecessor. One malformed record fails the whole batch.
 */
export function parseDrainValue(value: unknown, lastOrdinal: number | null): WireResult<WireDrainBatch> {
  return capture(() => drainBatchOrThrow(value, lastOrdinal, 'drain'))
}

/** Bounded text entry point for the string `drain` returns. */
export function parseDrainText(text: unknown, lastOrdinal: number | null): WireResult<WireDrainBatch> {
  return capture(() => drainBatchOrThrow(parseWireTextOrThrow(text, 'drain'), lastOrdinal, 'drain'))
}
