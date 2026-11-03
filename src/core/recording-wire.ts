// src/core/recording-wire.ts
// Shared, host-neutral native recording wire validation.
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
} from '../backend-contract/errors'
import { type AdapterAuthorization, type AdapterAvailability, type AdapterPower } from '../backend-contract/identity'
import { type StreamTerminalNotice } from '../backend-contract/streams'
import { isNotificationReceipt, type NotificationReceipt } from '../backend-contract/gatt'
import {
  MAX_RECORDING_PREPARE_TEXT_BYTES,
  type RecordingControlOperation
} from '../backend-contract/continuation-recording-bounds'
export const MAX_OPERATION_BYTES = 524288
export const MAX_BASE64_LENGTH = 4 * Math.ceil(MAX_OPERATION_BYTES / 3)
export const MAX_WIRE_TEXT_BYTES = 1048576
export const OPERATION_PREFIX = 'react-native-rust-core.wire'
export type WireResult<Value> =
  | { readonly ok: true; readonly value: Value }
  | { readonly ok: false; readonly error: BackendContractError }
export const DELIVERY_KINDS = Object.freeze(['notification', 'indication', 'unknown'] as const)
export type WireDelivery = (typeof DELIVERY_KINDS)[number]
export const COMMIT_STATES = Object.freeze(['not-dispatched', 'uncertain'] as const)
export type WireCommit = (typeof COMMIT_STATES)[number]
export const ADAPTER_AVAILABILITY = Object.freeze([
  'available',
  'unavailable',
  'unsupported',
  'unknown'
] as const satisfies readonly AdapterAvailability[])
export const ADAPTER_AUTHORIZATION = Object.freeze([
  'granted',
  'denied',
  'restricted',
  'not-determined',
  'unavailable',
  'unknown'
] as const satisfies readonly AdapterAuthorization[])
export const ADAPTER_POWER = Object.freeze([
  'on',
  'off',
  'resetting',
  'unsupported',
  'unknown'
] as const satisfies readonly AdapterPower[])
export const SECURITY_BOND = Object.freeze(['bonded', 'bonding', 'not-bonded', 'unknown', 'unsupported'] as const)
export const SECURITY_ENCRYPTION = Object.freeze(['encrypted', 'not-encrypted', 'unknown', 'unsupported'] as const)
export const SECURITY_AUTHENTICATION = Object.freeze([
  'authenticated',
  'unauthenticated',
  'unknown',
  'unsupported'
] as const)
export const SECURITY_SECURE_CONNECTIONS = Object.freeze(['yes', 'no', 'unknown', 'unsupported'] as const)
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
export const INGRESS_CLASSES = Object.freeze(['advertisement', 'notification', 'control'] as const)

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

export interface WireSecurityState {
  readonly bond: (typeof SECURITY_BOND)[number]
  readonly encryption: (typeof SECURITY_ENCRYPTION)[number]
  readonly authentication: (typeof SECURITY_AUTHENTICATION)[number]
  readonly secureConnections: (typeof SECURITY_SECURE_CONNECTIONS)[number]
  readonly pairingPossible: boolean | null
}

export interface WireRestoredPeer {
  readonly peerId: string
  readonly name: string | null
  readonly connected: boolean
}

export interface WireAdvertisementRecord {
  readonly t: 'adv'
  readonly ordinal: number
  readonly operationId: string
  readonly startOperationId: string
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
  readonly sourceTimestampMs: number | null
}

export function notificationReceiptOrThrow(value: unknown, path: string): NotificationReceipt | undefined {
  if (value === undefined) return undefined
  if (!isNotificationReceipt(value)) throw malformed(path)
  return Object.freeze({ ...value })
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
      readonly receipt?: NotificationReceipt
    }
  | {
      readonly t: 'recording-changed'
      readonly ordinal: number
      readonly consumer: string
      readonly recordingId: string
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
  | {
      readonly t: 'security-failed'
      readonly ordinal: number
      readonly peerId: string | null
      readonly error: WireRemoteFailure
    }
  | {
      readonly t: 'readiness'
      readonly ordinal: number
      readonly peerId: string
      readonly connectionGeneration: string | null
      readonly ready: boolean
    }
  | { readonly t: 'restored'; readonly ordinal: number; readonly peers: readonly WireRestoredPeer[] }

export interface WireDrainBatch {
  readonly more: boolean
  readonly records: readonly WireDrainRecord[]
  /**
   * Cumulative control records the owner refused past its queue cap, ever
   * (X-R5). Monotonic per session: an increase means control facts were
   * lost behind queued data, and the client re-reads them with
   * `session.reconcile` instead of waiting for the in-band `ingress-drop`.
   */
  readonly controlLost: number
}

export function operationName(path: string): string {
  return `${OPERATION_PREFIX}.${path}`
}

export function malformed(path: string): BackendContractError {
  return contractError('protocol.malformed', 'core', operationName(path))
}

export function capture<Value>(parse: () => Value): WireResult<Value> {
  try {
    return { ok: true, value: parse() }
  } catch (error) {
    if (error instanceof BackendContractError) return { ok: false, error }
    throw error
  }
}

export const BASE64_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'

export const PAD = 0x3d

export const BASE64_VALUES = (() => {
  const table = new Int8Array(128).fill(-1)
  for (let index = 0; index < BASE64_ALPHABET.length; index += 1) {
    table[BASE64_ALPHABET.charCodeAt(index)] = index
  }
  return table
})()

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

export function base64Value(text: string, index: number, path: string): number {
  const code = text.charCodeAt(index)
  const value = code < 128 ? (BASE64_VALUES[code] ?? -1) : -1
  if (value < 0) throw malformed(path)
  return value
}

export function decodeBase64OrThrow(text: unknown, path: string): Uint8Array {
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

export function utf8LengthExceeds(text: string, limit: number): boolean {
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

export function assertTextWithinBound(
  text: string,
  path: string,
  domain: BleErrorDomain,
  maximumBytes = MAX_WIRE_TEXT_BYTES
): void {
  // A UTF-16 code unit is never less than one UTF-8 byte, so the length
  // check is a sound early reject before the exact byte count.
  if (text.length > maximumBytes || utf8LengthExceeds(text, maximumBytes)) {
    throw contractError('bytes.too-large', domain, operationName(path))
  }
}

export function parseWireTextOrThrow(text: unknown, path: string, maximumBytes = MAX_WIRE_TEXT_BYTES): unknown {
  if (typeof text !== 'string') throw malformed(path)
  assertTextWithinBound(text, path, 'core', maximumBytes)
  try {
    const parsed: unknown = JSON.parse(text)
    return parsed
  } catch (error) {
    if (error instanceof SyntaxError) throw malformed(path)
    throw error
  }
}

export function isPlainObject(value: object): boolean {
  const prototype: unknown = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

export function exactObject(value: unknown, keys: readonly string[], path: string): ReadonlyMap<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || !isPlainObject(value)) {
    throw malformed(path)
  }
  const entries = new Map<string, unknown>(Object.entries(value))
  if (entries.size !== keys.length || keys.some(key => !entries.has(key))) throw malformed(path)
  return entries
}

export function arrayOf<Item>(
  value: unknown,
  path: string,
  parse: (entry: unknown, entryPath: string) => Item
): readonly Item[] {
  if (!Array.isArray(value)) throw malformed(path)
  return Object.freeze(value.map((entry: unknown, index: number) => parse(entry, `${path}[${index}]`)))
}

export function nullable<Item>(
  value: unknown,
  path: string,
  parse: (entry: unknown, entryPath: string) => Item
): Item | null {
  return value === null ? null : parse(value, path)
}

export interface IntegerRange {
  readonly min: number
  readonly max: number
}

export const NON_NEGATIVE: IntegerRange = { min: 0, max: Number.MAX_SAFE_INTEGER }

export const POSITIVE: IntegerRange = { min: 1, max: Number.MAX_SAFE_INTEGER }

export const COMPANY_ID: IntegerRange = { min: 0, max: 0xffff }

export const SIGNED_BYTE: IntegerRange = { min: -128, max: 127 }

export const SIGNED_SAFE: IntegerRange = { min: -Number.MAX_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER }

export function integerOrThrow(value: unknown, range: IntegerRange, path: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < range.min || value > range.max) {
    throw malformed(path)
  }
  return value === 0 ? 0 : value
}

export function enumOrThrow<Member extends string>(value: unknown, members: readonly Member[], path: string): Member {
  const member = members.find(candidate => candidate === value)
  if (member === undefined) throw malformed(path)
  return member
}

export function stringOrThrow(value: unknown, path: string): string {
  if (typeof value !== 'string' || value.length === 0) throw malformed(path)
  return value
}

export function optionalTextOrThrow(value: unknown, path: string): string | null {
  if (value === null) return null
  if (typeof value !== 'string') throw malformed(path)
  return value
}

export function booleanOrThrow(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') throw malformed(path)
  return value
}

export const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

export function uuidOrThrow(value: unknown, path: string): string {
  if (typeof value !== 'string' || !CANONICAL_UUID.test(value)) throw malformed(path)
  return value
}

export function nonEmptyArrayOrNull<Item>(
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

export function platformDetailOrThrow(value: unknown, path: string): WirePlatformDetail {
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

export function remoteFailureOrThrow(value: unknown, path: string): WireRemoteFailure {
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

export function parseNativeControlEnvelope(text: unknown, operation: string): WireResult<WireInvokeEnvelope> {
  return capture(() => invokeEnvelopeOrThrow(text, `${operation}.envelope`, false))
}

export function parseRecordingControlEnvelope(
  text: unknown,
  operation: RecordingControlOperation
): WireResult<WireInvokeEnvelope> {
  return capture(() => {
    enumOrThrow(operation, ['status', 'prepare', 'acknowledge', 'stop', 'clear'], 'continuation.recording.operation')
    return invokeEnvelopeOrThrow(
      text,
      `continuation.recording.${operation}.envelope`,
      false,
      operation === 'prepare' ? MAX_RECORDING_PREPARE_TEXT_BYTES : MAX_WIRE_TEXT_BYTES
    )
  })
}

export function invokeEnvelopeOrThrow(
  text: unknown,
  path: string,
  effectful: boolean,
  maximumBytes = MAX_WIRE_TEXT_BYTES
): WireInvokeEnvelope {
  const value = parseWireTextOrThrow(text, path, maximumBytes)
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
  if (effectful !== (commit !== null)) throw malformed(`${path}.commit`)
  const retryability = enumOrThrow(fields.get('retryability'), BLE_RETRYABILITIES, `${path}.retryability`)
  if (commit === 'uncertain' && retryability !== 'never') throw malformed(`${path}.retryability`)
  return Object.freeze({ kind: 'failure', failure, commit, retryability })
}

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

export function remoteFailureError(failure: WireRemoteFailure): BackendContractError {
  return contractError(failure.code, failure.domain, failure.operation, remotePlatformDetail(failure))
}

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

export function adapterStateOrThrow(value: unknown, path: string): WireAdapterState {
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

export function securityStateOrThrow(value: unknown, path: string): WireSecurityState {
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

export function restoredPeerOrThrow(value: unknown, path: string): WireRestoredPeer {
  const fields = exactObject(value, ['peerId', 'name', 'connected'], path)
  return Object.freeze({
    peerId: stringOrThrow(fields.get('peerId'), `${path}.peerId`),
    name: optionalTextOrThrow(fields.get('name'), `${path}.name`),
    connected: booleanOrThrow(fields.get('connected'), `${path}.connected`)
  })
}

export const DRAIN_RECORD_TYPES = Object.freeze([
  'adv',
  'scan-end',
  'value',
  'stream-end',
  'recording-changed',
  'adapter',
  'link',
  'db-changed',
  'ingress-drop',
  'security',
  'security-failed',
  'readiness',
  'restored'
] as const)

export function advertisementOrThrow(fields: ReadonlyMap<string, unknown>, ordinal: number, path: string) {
  const record: WireAdvertisementRecord = {
    t: 'adv',
    ordinal,
    operationId: stringOrThrow(fields.get('operationId'), `${path}.operationId`),
    startOperationId: stringOrThrow(fields.get('startOperationId'), `${path}.startOperationId`),
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
    observedAtMs: integerOrThrow(fields.get('observedAtMs'), NON_NEGATIVE, `${path}.observedAtMs`),
    sourceTimestampMs: nullable(fields.get('sourceTimestampMs'), `${path}.sourceTimestampMs`, (entry, entryPath) =>
      integerOrThrow(entry, NON_NEGATIVE, entryPath)
    )
  }
  return Object.freeze(record)
}

export function drainRecordOrThrow(value: unknown, path: string): WireDrainRecord {
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
        'operationId',
        'startOperationId',
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
        'observedAtMs',
        'sourceTimestampMs'
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
      const fields = exactObject(
        value,
        [
          't',
          'ordinal',
          'consumer',
          'valueB64',
          'delivery',
          ...(new Map<string, unknown>(Object.entries(value)).has('receipt') ? ['receipt'] : [])
        ],
        recordPath
      )
      const ordinal = integerOrThrow(fields.get('ordinal'), NON_NEGATIVE, `${recordPath}.ordinal`)
      return Object.freeze({
        t: type,
        ordinal,
        consumer: stringOrThrow(fields.get('consumer'), `${recordPath}.consumer`),
        value: decodeBase64OrThrow(fields.get('valueB64'), `${recordPath}.valueB64`),
        delivery: enumOrThrow(fields.get('delivery'), DELIVERY_KINDS, `${recordPath}.delivery`),
        ...(fields.get('receipt') === undefined
          ? {}
          : { receipt: notificationReceiptOrThrow(fields.get('receipt'), `${recordPath}.receipt`) })
      })
    }
    case 'recording-changed': {
      const { fields, ordinal } = read(['consumer', 'recordingId'])
      return Object.freeze({
        t: type,
        ordinal,
        consumer: stringOrThrow(fields.get('consumer'), `${recordPath}.consumer`),
        recordingId: stringOrThrow(fields.get('recordingId'), `${recordPath}.recordingId`)
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
    case 'security-failed': {
      const { fields, ordinal } = read(['peerId', 'error'])
      return Object.freeze({
        t: type,
        ordinal,
        peerId: nullable(fields.get('peerId'), `${recordPath}.peerId`, stringOrThrow),
        error: remoteFailureOrThrow(fields.get('error'), `${recordPath}.error`)
      })
    }
    case 'readiness': {
      const { fields, ordinal } = read(['peerId', 'connectionGeneration', 'ready'])
      return Object.freeze({
        t: type,
        ordinal,
        peerId: stringOrThrow(fields.get('peerId'), `${recordPath}.peerId`),
        connectionGeneration: nullable(
          fields.get('connectionGeneration'),
          `${recordPath}.connectionGeneration`,
          stringOrThrow
        ),
        ready: booleanOrThrow(fields.get('ready'), `${recordPath}.ready`)
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

export function drainBatchOrThrow(value: unknown, lastOrdinal: number | null, path: string): WireDrainBatch {
  const fields = exactObject(value, ['more', 'records', 'controlLost'], path)
  const more = booleanOrThrow(fields.get('more'), `${path}.more`)
  const records = arrayOf(fields.get('records'), `${path}.records`, drainRecordOrThrow)
  const controlLost = integerOrThrow(fields.get('controlLost'), NON_NEGATIVE, `${path}.controlLost`)
  let previous = lastOrdinal
  records.forEach((record, index) => {
    if (previous !== null && record.ordinal <= previous) throw malformed(`${path}.records[${index}].ordinal`)
    previous = record.ordinal
  })
  return Object.freeze({ more, records, controlLost })
}

export function parseDrainText(text: unknown, lastOrdinal: number | null): WireResult<WireDrainBatch> {
  return capture(() => drainBatchOrThrow(parseWireTextOrThrow(text, 'drain'), lastOrdinal, 'drain'))
}
