// src/tauri/transport.ts

import { BLE_ERROR_CODES, BLE_ERROR_DOMAINS, contractError } from '../backend-contract/errors'
import type {
  BleErrorCode,
  BleErrorDomain,
  CleanupRecord,
  NormalizedBleError,
  PlatformErrorDetail
} from '../backend-contract/errors'
import type { IpcClientLeaseIdentity } from '../backend-contract/ipc'
import type { IpcClientBootstrap } from '../ipc/protocol'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'
import {
  assertSafeSerializablePrototype,
  createOwnedSerializableRecord,
  setOwnedSerializableEntry,
  utf8ByteLength
} from '../backend-contract/serializable'
import type {
  IpcBleEvent,
  IpcBleRequest,
  IpcBleResponse,
  IpcClientTransport,
  IpcEventAcknowledgeResponse,
  IpcFailureResponse
} from '../ipc/protocol'
import { encodeTauriScanQuery } from './scan-plan'
import type { NormalizedScanQuery } from '../backend-contract/scan-query'

/** Tauri v2 plugin command registered by the Rust crate. */
export const TAURI_BLE_PLUGIN_COMMAND = 'plugin:unified-ble-manager|invoke'
const TAURI_BYTES_WIRE_TAG = '$__unifiedBleBytesV2'

/** Maximum nested array/record depth for one encode or decode walk. */
export const TAURI_WIRE_MAX_DEPTH = 32
/** Maximum objects, arrays, and primitives visited during one walk. */
export const TAURI_WIRE_MAX_NODES = 16_384
/** Maximum entries in a general array (tagged byte payloads use the binary budget). */
export const TAURI_WIRE_MAX_ARRAY_LENGTH = 65_536
/** Maximum UTF-8 bytes in a single object key. */
export const TAURI_WIRE_MAX_KEY_BYTES = 1024
/** Maximum cumulative UTF-8 bytes of strings and keys in one walk. */
export const TAURI_WIRE_MAX_TEXT_BYTES = 2 * 1024 * 1024
/** Maximum cumulative tagged/owned byte payload in one walk; matches IPC pending-stream retained bytes. */
export const TAURI_WIRE_MAX_BINARY_BYTES = 2 * 1024 * 1024

interface TauriWireWalkState {
  depth: number
  nodes: number
  textBytes: number
  binaryBytes: number
  readonly ancestors: WeakSet<object>
}

function createTauriWireWalkState(): TauriWireWalkState {
  return { depth: 0, nodes: 0, textBytes: 0, binaryBytes: 0, ancestors: new WeakSet<object>() }
}

function tauriWireMalformed(detail: string): never {
  throw contractError('protocol.malformed', 'ipc', `tauri.transport.${detail}`)
}

function chargeTauriWireNode(state: TauriWireWalkState): void {
  state.nodes += 1
  if (state.nodes > TAURI_WIRE_MAX_NODES) tauriWireMalformed('nodes')
}

function chargeTauriWireText(state: TauriWireWalkState, value: string): void {
  const byteLength = utf8ByteLength(value)
  if (state.textBytes + byteLength > TAURI_WIRE_MAX_TEXT_BYTES) tauriWireMalformed('text-bytes')
  state.textBytes += byteLength
}

function chargeTauriWireKey(state: TauriWireWalkState, key: string): void {
  const byteLength = utf8ByteLength(key)
  if (byteLength > TAURI_WIRE_MAX_KEY_BYTES) tauriWireMalformed('key-bytes')
  chargeTauriWireText(state, key)
}

function chargeTauriWireBinary(state: TauriWireWalkState, byteLength: number): void {
  if (byteLength > TAURI_WIRE_MAX_BINARY_BYTES - state.binaryBytes) tauriWireMalformed('binary-bytes')
  state.binaryBytes += byteLength
}

function enterTauriWireContainer(state: TauriWireWalkState, value: object): void {
  if (state.ancestors.has(value)) tauriWireMalformed('cycle')
  if (state.depth >= TAURI_WIRE_MAX_DEPTH) tauriWireMalformed('depth')
  state.ancestors.add(value)
  state.depth += 1
}

function leaveTauriWireContainer(state: TauriWireWalkState, value: object): void {
  state.ancestors.delete(value)
  state.depth -= 1
}

/**
 * The only request that carries the event Channel. Attaching binds the sink
 * for the attachment's lifetime; see `eventChannelArgument`.
 */
export const TAURI_ATTACH_REQUEST_KIND = 'bootstrap'

function isAttachRequest(request: { readonly kind?: unknown }): boolean {
  return request.kind === TAURI_ATTACH_REQUEST_KIND
}

/** Structural subset of `@tauri-apps/api/core.invoke` used by this package. */
export type TauriInvoke = <Response>(command: string, args?: Record<string, unknown>) => Promise<Response>

/** Structural subset of a Tauri v2 Channel. */
export interface TauriChannel<Message> {
  onmessage: ((message: Message) => void) | null
}

/** Constructor shape of `Channel` from `@tauri-apps/api/core`. */
export interface TauriChannelConstructor {
  new <Message>(): TauriChannel<Message>
}

export interface TauriBleIpcTransportOptions {
  readonly invoke: TauriInvoke
  readonly Channel: TauriChannelConstructor
  readonly command?: string
}

/**
 * Tauri v2 transport for the shared desktop IPC client. Callers pass the
 * official `invoke` and `Channel` exports, keeping the core package free of a
 * mandatory Tauri runtime dependency and straightforward to test.
 */
export class TauriBleIpcTransport<Attachment extends string, Client extends string>
  implements IpcClientTransport<Attachment, Client>
{
  private readonly invokeCore: TauriInvoke
  private readonly command: string
  private readonly eventChannel: TauriChannel<unknown>
  private readonly listeners = new Set<(event: IpcBleEvent) => void>()
  private trustedScanQuery: SerializableRecord | null = null

  constructor(options: TauriBleIpcTransportOptions) {
    if (typeof options.invoke !== 'function' || typeof options.Channel !== 'function') {
      throw new TypeError('TauriBleIpcTransport requires the official Tauri v2 invoke and Channel APIs')
    }
    this.invokeCore = options.invoke
    this.command = options.command ?? TAURI_BLE_PLUGIN_COMMAND
    if (this.command.length === 0) {
      throw new TypeError('TauriBleIpcTransport command must not be empty')
    }
    this.eventChannel = new options.Channel<unknown>()
    this.eventChannel.onmessage = wireEvent => {
      const event = decodeIpcBleEvent(wireEvent)
      for (const listener of [...this.listeners]) {
        listener(event)
      }
    }
  }

  async invoke<Operation extends string>(
    request: IpcBleRequest<Attachment, Client, Operation>
  ): Promise<IpcBleResponse<Attachment, Client>> {
    if (request.kind === TAURI_ATTACH_REQUEST_KIND && !isIpcBootstrapRequest(request)) {
      throw contractError('protocol.malformed', 'ipc', 'tauri.transport.bootstrap-request')
    }
    const trustedRequest = this.bindTrustedScanQuery(request)
    const wireRequest =
      trustedRequest.kind === 'route'
        ? { kind: trustedRequest.kind, envelope: trustedRequest.envelope }
        : trustedRequest
    const response = await this.invokeCore<unknown>(this.command, {
      request: encodeTauriWireValue(wireRequest),
      ...this.eventChannelArgument(request)
    })
    return decodeIpcBleResponse<Attachment, Client>(response)
  }

  async withTrustedScanQuery<Response>(query: NormalizedScanQuery, action: () => Promise<Response>): Promise<Response> {
    if (this.trustedScanQuery !== null) {
      throw contractError('ownership.denied', 'scan', 'tauri.transport.trusted-query')
    }
    this.trustedScanQuery = encodeTauriScanQuery(query)
    try {
      return await action()
    } finally {
      this.trustedScanQuery = null
    }
  }

  private bindTrustedScanQuery<Operation extends string>(
    request: IpcBleRequest<Attachment, Client, Operation>
  ): IpcBleRequest<Attachment, Client, Operation> {
    if (request.kind !== 'route' || request.envelope.command !== 'scan.start') return request
    if (request.envelope.payload !== undefined && 'query' in request.envelope.payload) return request
    if (this.trustedScanQuery === null) return request
    return Object.freeze({
      ...request,
      envelope: Object.freeze({
        ...request.envelope,
        payload: Object.freeze({ ...request.envelope.payload, query: this.trustedScanQuery })
      })
    })
  }

  /**
   * Supplies the event Channel on the attach request only.
   *
   * Tauri deserializes every `Channel` command argument into a *new* Rust
   * `Channel` bound to this one JavaScript callback id, and dropping any of
   * them evals `{ end: true, index }` for that shared id. The Tauri JS runtime
   * answers an end message whose index matches its next expected index by
   * calling `unregisterCallback`, which tears the callback down permanently.
   *
   * Passing the Channel on a second request therefore destroys the event
   * stream established by the first: the request/response path would silently
   * kill the event path. The event sink is bound once, at attach, and lives
   * for the attachment, which is the lifetime every other host already gives
   * it via `subscribe`.
   */
  private eventChannelArgument(request: IpcBleRequest<Attachment, Client, string>): {
    eventChannel?: TauriChannel<unknown>
  } {
    return isAttachRequest(request) ? { eventChannel: this.eventChannel } : {}
  }

  subscribe(listener: (event: IpcBleEvent) => void): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  async acknowledge(
    rendererLease: IpcClientLeaseIdentity,
    eventId: string
  ): Promise<IpcEventAcknowledgeResponse | IpcFailureResponse> {
    const response = await this.invokeCore<unknown>(this.command, {
      request: encodeTauriWireValue({ kind: 'event.ack', rendererLease, eventId })
    })
    const decoded = decodeIpcBleResponse<Attachment, Client>(response)
    if (decoded.kind === 'event.ack' || decoded.kind === 'failure') return decoded
    throw contractError('protocol.malformed', 'ipc', 'tauri.transport.acknowledge-response')
  }
}

/** Encodes bytes explicitly before Tauri serializes nested command arguments as JSON. */
export function encodeTauriWireValue(value: unknown): unknown {
  return encodeTauriWireValueWithState(value, createTauriWireWalkState())
}

function encodeTauriWireValueWithState(value: unknown, state: TauriWireWalkState): unknown {
  chargeTauriWireNode(state)
  assertEncodableTauriValue(value)
  if (typeof value === 'string') {
    chargeTauriWireText(state, value)
    return value
  }
  if (value instanceof Uint8Array) {
    chargeTauriWireBinary(state, value.byteLength)
    return { [TAURI_BYTES_WIRE_TAG]: Array.from(value) }
  }
  if (Array.isArray(value)) {
    if (value.length > TAURI_WIRE_MAX_ARRAY_LENGTH) tauriWireMalformed('array-length')
    enterTauriWireContainer(state, value)
    try {
      return value.map(item => encodeTauriWireValueWithState(item, state))
    } finally {
      leaveTauriWireContainer(state, value)
    }
  }
  if (isWireRecord(value)) {
    assertSafeSerializablePrototype(value, 'ipc', 'tauri.transport.prototype')
    enterTauriWireContainer(state, value)
    try {
      const encoded = createOwnedSerializableRecord<unknown>()
      for (const [key, item] of Object.entries(value)) {
        chargeTauriWireKey(state, key)
        setOwnedSerializableEntry(
          encoded,
          key,
          encodeTauriWireValueWithState(item, state),
          'ipc',
          'tauri.transport.forbidden-key'
        )
      }
      return encoded
    } finally {
      leaveTauriWireContainer(state, value)
    }
  }
  if (value !== null && typeof value === 'object') {
    throw contractError('protocol.malformed', 'ipc', 'tauri.transport.encode-object')
  }
  return value
}

/** Reconstructs independently owned bytes from Rust responses and Channel messages. */
export function decodeTauriWireValue(value: unknown): unknown {
  return decodeTauriWireValueWithState(value, createTauriWireWalkState())
}

function decodeTauriWireValueWithState(value: unknown, state: TauriWireWalkState): unknown {
  chargeTauriWireNode(state)
  if (typeof value === 'string') {
    chargeTauriWireText(state, value)
    return value
  }
  if (value instanceof Uint8Array) {
    chargeTauriWireBinary(state, value.byteLength)
    return new Uint8Array(value)
  }
  if (Array.isArray(value)) {
    if (value.length > TAURI_WIRE_MAX_ARRAY_LENGTH) tauriWireMalformed('array-length')
    enterTauriWireContainer(state, value)
    try {
      return value.map(item => decodeTauriWireValueWithState(item, state))
    } finally {
      leaveTauriWireContainer(state, value)
    }
  }
  if (!isWireRecord(value)) {
    if (value !== null && typeof value === 'object') {
      throw contractError('protocol.malformed', 'ipc', 'tauri.transport.decode-object')
    }
    return value
  }
  enterTauriWireContainer(state, value)
  try {
    if (Object.prototype.hasOwnProperty.call(value, TAURI_BYTES_WIRE_TAG)) {
      const keys = Object.keys(value)
      const bytes = value[TAURI_BYTES_WIRE_TAG]
      if (keys.length !== 1 || !Array.isArray(bytes)) {
        tauriWireMalformed('bytes')
      }
      chargeTauriWireKey(state, TAURI_BYTES_WIRE_TAG)
      chargeTauriWireBinary(state, bytes.length)
      if (!bytes.every(isByte)) tauriWireMalformed('bytes')
      return new Uint8Array(bytes)
    }
    assertSafeSerializablePrototype(value, 'ipc', 'tauri.transport.prototype')
    const decoded = createOwnedSerializableRecord<unknown>()
    for (const [key, item] of Object.entries(value)) {
      chargeTauriWireKey(state, key)
      setOwnedSerializableEntry(
        decoded,
        key,
        decodeTauriWireValueWithState(item, state),
        'ipc',
        'tauri.transport.forbidden-key'
      )
    }
    return decoded
  } finally {
    leaveTauriWireContainer(state, value)
  }
}

function isWireRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array) return false
  try {
    const prototype = Object.getPrototypeOf(value)
    return prototype === Object.prototype || prototype === null
  } catch {
    return false
  }
}

function assertEncodableTauriValue(value: unknown): void {
  if (
    value === undefined ||
    typeof value === 'bigint' ||
    typeof value === 'function' ||
    typeof value === 'symbol' ||
    value instanceof Date ||
    value instanceof Map ||
    value instanceof Set ||
    value instanceof ArrayBuffer ||
    (ArrayBuffer.isView(value) && !(value instanceof Uint8Array))
  ) {
    throw contractError('protocol.malformed', 'ipc', 'tauri.transport.encode-value')
  }
}

function isByte(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= 255
}

function decodeIpcBleEvent(value: unknown): IpcBleEvent {
  const decoded = decodeTauriWireValue(value)
  if (!isIpcBleEvent(decoded)) throw contractError('protocol.malformed', 'ipc', 'tauri.transport.event')
  return decoded
}

function decodeIpcBleResponse<Attachment extends string, Client extends string>(
  value: unknown
): IpcBleResponse<Attachment, Client> {
  const decoded = decodeTauriWireValue(value)
  if (!isIpcBleResponse<Attachment, Client>(decoded)) {
    throw contractError('protocol.malformed', 'ipc', adapterStateMismatchOperation(decoded))
  }
  return decoded
}

const adapterStateKeys = Object.freeze([
  'availability',
  'authorization',
  'power',
  'heard',
  'backendGeneration',
  'updatedAt',
  'safeReason'
])

function adapterStateMismatchOperation(value: unknown): string {
  const bootstrap = wireRecord(wireRecord(value)?.bootstrap)
  const attachment = wireRecord(bootstrap?.attachment)
  const adapter = wireRecord(attachment?.adapter)
  const state = wireRecord(adapter?.state)
  if (state === null) return 'tauri.transport.response'
  const actual = Object.keys(state)
  const extra = actual.filter(key => !adapterStateKeys.includes(key)).sort()
  const missing = adapterStateKeys.filter(key => !actual.includes(key)).sort()
  if (extra.length === 0 && missing.length === 0) return 'tauri.transport.response'
  const parts = []
  if (extra.length > 0) parts.push(`extra=${extra.join(',')}`)
  if (missing.length > 0) parts.push(`missing=${missing.join(',')}`)
  return `tauri.transport.response ${parts.join(' ')}`
}

function isIpcBleEvent(value: unknown): value is IpcBleEvent {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['rendererLease', 'eventId', 'streamId', 'item']) &&
    isLease(record.rendererLease) &&
    nonEmptyString(record.eventId) &&
    nonEmptyString(record.streamId) &&
    serializableRecord(record.item)
  )
}

function isIpcBootstrapRequest(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['kind', 'offer']) &&
    record.kind === TAURI_ATTACH_REQUEST_KIND &&
    isIpcCompatibilityOffer(record.offer)
  )
}

function isIpcCompatibilityOffer(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['backendContract', 'capabilitySchema', 'eventSchema', 'traceFormat', 'ipcProtocol']) &&
    isVersionRange(record.backendContract, 'backend-contract') &&
    isVersionRange(record.capabilitySchema, 'capability-schema') &&
    isVersionRange(record.eventSchema, 'event-schema') &&
    isVersionRange(record.traceFormat, 'trace-format') &&
    isVersionRange(record.ipcProtocol, 'ipc-protocol')
  )
}

function isVersionRange(value: unknown, axis: string): boolean {
  const record = wireRecord(value)
  if (record === null || !exactKeys(record, ['axis', 'minimum', 'maximum']) || record.axis !== axis) return false
  const minimum = wireRecord(record.minimum)
  const maximum = wireRecord(record.maximum)
  return (
    minimum !== null &&
    maximum !== null &&
    exactKeys(minimum, ['axis', 'value']) &&
    exactKeys(maximum, ['axis', 'value']) &&
    minimum.axis === axis &&
    maximum.axis === axis &&
    safeInteger(minimum.value) &&
    safeInteger(maximum.value) &&
    minimum.value >= 0 &&
    maximum.value >= minimum.value
  )
}

function isIpcBleResponse<Attachment extends string, Client extends string>(
  value: unknown
): value is IpcBleResponse<Attachment, Client> {
  const record = wireRecord(value)
  if (record === null || typeof record.kind !== 'string') return false
  switch (record.kind) {
    case 'bootstrap':
      return exactKeys(record, ['kind', 'bootstrap']) && isBootstrap(record.bootstrap)
    case 'route':
      return exactKeys(record, ['kind', 'payload']) && serializableRecord(record.payload)
    case 'release':
      return exactKeys(record, ['kind', 'cleanup']) && isCleanupRecord(record.cleanup)
    case 'event.ack':
      return exactKeys(record, ['kind'])
    case 'failure':
      return exactKeys(record, ['kind', 'error']) && isNormalizedBleError(record.error)
    default:
      return false
  }
}

function isBootstrap<Attachment extends string, Client extends string>(
  value: unknown
): value is IpcClientBootstrap<Attachment, Client> {
  const record = wireRecord(value)
  if (
    record === null ||
    !hasBootstrapKeys(record) ||
    !nonEmptyString(record.attachmentId) ||
    !isAttachment(record.attachment) ||
    !isIpcVersionAxes(record.versions) ||
    !isCapabilitySnapshot(record.capabilities, wireRecord(record.attachment)?.backendGeneration) ||
    (record.discovery !== undefined && !isDiscoveryDescriptor(record.discovery)) ||
    !isRenderer(record.renderer) ||
    !isLease(record.rendererLease)
  ) {
    return false
  }
  const attachment = wireRecord(record.attachment)
  if (attachment === null || attachment.attachmentId !== record.attachmentId) return false
  return true
}

function hasBootstrapKeys(record: Record<string, unknown>): boolean {
  const keys = Object.keys(record).sort()
  const required = ['attachment', 'attachmentId', 'capabilities', 'renderer', 'rendererLease', 'versions']
  const withDiscovery = [...required, 'discovery'].sort()
  return (
    (keys.length === required.length && keys.every((key, index) => key === required.sort()[index])) ||
    (keys.length === withDiscovery.length && keys.every((key, index) => key === withDiscovery[index]))
  )
}

function isDiscoveryDescriptor(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    Object.keys(record).length === 1 &&
    (record.kind === 'continuous-scan' || record.kind === 'system-chooser' || record.kind === 'hybrid')
  )
}

function isCapabilitySnapshot(value: unknown, expectedBackendGeneration: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['schemaVersion', 'backendGeneration', 'descriptors']) &&
    record.schemaVersion === 2 &&
    nonEmptyString(record.backendGeneration) &&
    record.backendGeneration === expectedBackendGeneration &&
    Array.isArray(record.descriptors) &&
    record.descriptors.every(isCapabilityDescriptor)
  )
}

function isCapabilityState(value: unknown): boolean {
  return value === 'supported' || value === 'limited' || value === 'unsupported' || value === 'unavailable'
}

function isCapabilityDescriptor(value: unknown): boolean {
  const record = wireRecord(value)
  if (
    record === null ||
    !exactKeys(record, [
      'id',
      'state',
      'selectedSchemaRange',
      'implementationOrigin',
      'tck',
      'evidence',
      'limitations',
      'limits'
    ]) ||
    !nonEmptyString(record.id) ||
    !isCapabilityState(record.state) ||
    (record.implementationOrigin !== 'backend-native' && record.implementationOrigin !== 'core-emulated') ||
    !isCapabilitySchemaRange(record.selectedSchemaRange) ||
    !isCapabilityTck(record.tck) ||
    !isCapabilityEvidence(record.evidence) ||
    !isLimitations(record.limitations) ||
    !isCapabilityLimits(record.limits)
  ) {
    return false
  }
  const evidence = wireRecord(record.evidence)
  const tck = wireRecord(record.tck)
  const limitations = record.limitations
  const evidenceLimitations = evidence?.limitations
  const requiredScenarios = tck?.requiredScenarioIds
  const evidenceScenarios = evidence?.scenarioIds
  if (
    !Array.isArray(limitations) ||
    !Array.isArray(evidenceLimitations) ||
    !Array.isArray(requiredScenarios) ||
    !Array.isArray(evidenceScenarios)
  ) {
    return false
  }
  return (
    limitationsEqual(limitations, evidenceLimitations) &&
    requiredScenarios.every(scenario => typeof scenario === 'string' && evidenceScenarios.includes(scenario)) &&
    ((record.state === 'supported' && evidence?.evidenceLevel !== 'blocked') || record.state !== 'supported')
  )
}

function isCapabilitySchemaRange(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['axis', 'minimum', 'maximum']) &&
    record.axis === 'capability-schema' &&
    isVersionNumber(record.minimum, 'capability-schema') &&
    isVersionNumber(record.maximum, 'capability-schema') &&
    versionNumber(record.minimum) <= versionNumber(record.maximum)
  )
}

function isCapabilityTck(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['suiteId', 'requiredScenarioIds', 'contractRange']) &&
    nonEmptyString(record.suiteId) &&
    stringArray(record.requiredScenarioIds) &&
    record.requiredScenarioIds.length > 0 &&
    isCapabilitySchemaRange(record.contractRange)
  )
}

function isCapabilityEvidence(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, [
      'receiptId',
      'evidenceLevel',
      'implementationVersion',
      'sourceDigest',
      'scenarioIds',
      'limitations'
    ]) &&
    nonEmptyString(record.receiptId) &&
    (record.evidenceLevel === 'blocked' ||
      record.evidenceLevel === 'deterministic' ||
      record.evidenceLevel === 'live-preview' ||
      record.evidenceLevel === 'supported' ||
      record.evidenceLevel === 'reliability-qualified') &&
    nonEmptyString(record.implementationVersion) &&
    nonEmptyString(record.sourceDigest) &&
    stringArray(record.scenarioIds) &&
    isLimitations(record.limitations)
  )
}

function isLimitations(value: unknown): value is readonly Record<string, unknown>[] {
  return (
    Array.isArray(value) &&
    value.every(item => {
      const record = wireRecord(item)
      return (
        record !== null &&
        exactKeys(record, ['code', 'explanation', 'affectedGuarantee']) &&
        nonEmptyString(record.code) &&
        nonEmptyString(record.explanation) &&
        nonEmptyString(record.affectedGuarantee)
      )
    })
  )
}

function limitationsEqual(
  left: readonly Record<string, unknown>[],
  right: readonly Record<string, unknown>[]
): boolean {
  return (
    left.length === right.length &&
    left.every((limitation, index) => {
      const other = right[index]
      return (
        other !== undefined &&
        limitation.code === other.code &&
        limitation.explanation === other.explanation &&
        limitation.affectedGuarantee === other.affectedGuarantee
      )
    })
  )
}

function isCapabilityLimits(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    Object.keys(record).length > 0 &&
    Object.values(record).every(item => {
      const limit = wireRecord(item)
      if (limit === null || !exactKeys(limit, ['maximum', 'minimum', 'unit'])) return false
      return (
        finiteNumber(limit.maximum) &&
        limit.maximum >= 0 &&
        (limit.minimum === null || (finiteNumber(limit.minimum) && limit.minimum >= 0)) &&
        typeof limit.unit === 'string' &&
        limit.unit.length > 0
      )
    })
  )
}

function isVersionNumber(value: unknown, axis: string): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['axis', 'value']) &&
    record.axis === axis &&
    safeInteger(record.value) &&
    record.value >= 0
  )
}

function versionNumber(value: unknown): number {
  const record = wireRecord(value)
  if (record === null || typeof record.value !== 'number') {
    throw new TypeError('Malformed capability schema version')
  }
  return record.value
}

function isAttachment(value: unknown): boolean {
  const record = wireRecord(value)
  if (record === null || !exactKeys(record, ['attachmentId', 'backendInstanceId', 'backendGeneration', 'adapter'])) {
    return false
  }
  const adapter = wireRecord(record.adapter)
  if (
    adapter === null ||
    !exactKeys(adapter, ['adapterId', 'displayName', 'state', 'adapterGeneration', 'limitations']) ||
    !nonEmptyString(adapter.adapterId) ||
    !(adapter.displayName === null || typeof adapter.displayName === 'string') ||
    !isAdapterState(adapter.state) ||
    !nonEmptyString(adapter.adapterGeneration) ||
    !stringArray(adapter.limitations)
  ) {
    return false
  }
  const state = wireRecord(adapter.state)
  if (state === null || state.backendGeneration !== record.backendGeneration) return false
  return (
    nonEmptyString(record.attachmentId) &&
    nonEmptyString(record.backendInstanceId) &&
    nonEmptyString(record.backendGeneration)
  )
}

function isAdapterState(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, adapterStateKeys) &&
    isAdapterAvailability(record.availability) &&
    isAdapterAuthorization(record.authorization) &&
    isAdapterPower(record.power) &&
    isHeardCount(record.heard) &&
    nonEmptyString(record.backendGeneration) &&
    finiteNumber(record.updatedAt) &&
    (record.safeReason === null || typeof record.safeReason === 'string')
  )
}

function isHeardCount(value: unknown): boolean {
  return value === null || (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0)
}

function isAdapterAvailability(value: unknown): boolean {
  return value === 'available' || value === 'unavailable' || value === 'unsupported' || value === 'unknown'
}

function isAdapterAuthorization(value: unknown): boolean {
  return (
    value === 'granted' ||
    value === 'denied' ||
    value === 'restricted' ||
    value === 'not-determined' ||
    value === 'unavailable' ||
    value === 'unknown'
  )
}

function isAdapterPower(value: unknown): boolean {
  return value === 'on' || value === 'off' || value === 'resetting' || value === 'unsupported' || value === 'unknown'
}

function isRenderer(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['clientId', 'windowScope', 'sessionScope']) &&
    nonEmptyString(record.clientId) &&
    nonEmptyString(record.windowScope) &&
    nonEmptyString(record.sessionScope)
  )
}

function isLease(value: unknown): value is IpcClientLeaseIdentity {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['leaseId', 'generation']) &&
    nonEmptyString(record.leaseId) &&
    nonEmptyString(record.generation)
  )
}

function isIpcVersionAxes(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['backendContract', 'capabilitySchema', 'eventSchema', 'traceFormat', 'ipcProtocol']) &&
    negotiatedVersion(record.backendContract, 'backend-contract') &&
    negotiatedVersion(record.capabilitySchema, 'capability-schema') &&
    negotiatedVersion(record.eventSchema, 'event-schema') &&
    negotiatedVersion(record.traceFormat, 'trace-format') &&
    negotiatedVersion(record.ipcProtocol, 'ipc-protocol')
  )
}

function negotiatedVersion(value: unknown, axis: string): boolean {
  const record = wireRecord(value)
  const selected = versionNumberValue(record?.selected, axis)
  const localRange = versionRangeValue(record?.localRange, axis)
  const remoteRange = versionRangeValue(record?.remoteRange, axis)
  const localMinimum = localRange?.minimum ?? null
  const localMaximum = localRange?.maximum ?? null
  const remoteMinimum = remoteRange?.minimum ?? null
  const remoteMaximum = remoteRange?.maximum ?? null
  return (
    record !== null &&
    exactKeys(record, ['axis', 'selected', 'localRange', 'remoteRange']) &&
    record.axis === axis &&
    selected !== null &&
    localMinimum !== null &&
    localMaximum !== null &&
    remoteMinimum !== null &&
    remoteMaximum !== null &&
    localMinimum <= selected &&
    selected <= localMaximum &&
    remoteMinimum <= selected &&
    selected <= remoteMaximum
  )
}

function versionNumberValue(value: unknown, axis: string): number | null {
  const record = wireRecord(value)
  if (record === null || !exactKeys(record, ['axis', 'value']) || record.axis !== axis || !safeInteger(record.value)) {
    return null
  }
  return record.value
}

function versionRangeValue(
  value: unknown,
  axis: string
): { readonly minimum: number; readonly maximum: number } | null {
  const record = wireRecord(value)
  if (record === null || !exactKeys(record, ['axis', 'minimum', 'maximum']) || record.axis !== axis) return null
  const minimum = versionNumberValue(record.minimum, axis)
  const maximum = versionNumberValue(record.maximum, axis)
  if (minimum === null || maximum === null || minimum > maximum) return null
  return { minimum, maximum }
}

function isCleanupRecord(value: unknown): value is CleanupRecord {
  const record = wireRecord(value)
  if (
    record === null ||
    !exactKeys(record, ['state', 'failures']) ||
    (record.state !== 'released' && record.state !== 'release-failed')
  ) {
    return false
  }
  const failures = Array.isArray(record.failures) ? record.failures : null
  if (failures === null || !failures.every(isCleanupFailure)) return false
  return (record.state === 'released') === (failures.length === 0)
}

function isCleanupFailure(value: unknown): boolean {
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['resourceKind', 'error']) &&
    nonEmptyString(record.resourceKind) &&
    isNormalizedBleError(record.error)
  )
}

function isNormalizedBleError(value: unknown): value is NormalizedBleError {
  const record = wireRecord(value)
  const retryability =
    record?.code === 'operation.aborted' || record?.code === 'operation.timed-out' ? 'caller-decides' : 'never'
  return (
    record !== null &&
    exactKeys(record, ['code', 'domain', 'operation', 'platform', 'retryability']) &&
    isBleErrorCode(record.code) &&
    isBleErrorDomain(record.domain) &&
    nonEmptyString(record.domain) &&
    nonEmptyString(record.operation) &&
    record.retryability === retryability &&
    isPlatformErrorDetail(record.platform)
  )
}

function isBleErrorCode(value: unknown): value is BleErrorCode {
  return typeof value === 'string' && BLE_ERROR_CODES.some(code => code === value)
}

function isBleErrorDomain(value: unknown): value is BleErrorDomain {
  return typeof value === 'string' && BLE_ERROR_DOMAINS.some(domain => domain === value)
}

function isPlatformErrorDetail(value: unknown): value is PlatformErrorDetail | null {
  if (value === null) return true
  const record = wireRecord(value)
  return (
    record !== null &&
    exactKeys(record, ['domain', 'code', 'safeMessage', 'metadata']) &&
    nonEmptyString(record.domain) &&
    nonEmptyString(record.code) &&
    typeof record.safeMessage === 'string' &&
    serializableRecord(record.metadata)
  )
}

function serializableRecord(value: unknown): value is SerializableRecord {
  const record = wireRecord(value)
  return record !== null && Object.values(record).every(serializableValue)
}

function serializableValue(value: unknown): value is SerializableValue {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return true
  if (typeof value === 'number') return Number.isFinite(value)
  if (value instanceof Uint8Array) return true
  if (Array.isArray(value)) return value.every(serializableValue)
  return serializableRecord(value)
}

function wireRecord(value: unknown): Record<string, unknown> | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array) return null
  return Object.fromEntries(Object.entries(value))
}

function exactKeys(record: Record<string, unknown>, keys: readonly string[]): boolean {
  const actual = Object.keys(record).sort()
  return actual.length === keys.length && actual.every((key, index) => key === [...keys].sort()[index])
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0
}

function stringArray(value: unknown): value is readonly string[] {
  return Array.isArray(value) && value.every(item => typeof item === 'string')
}

function safeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

function finiteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value)
}
