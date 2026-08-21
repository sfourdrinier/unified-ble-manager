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
import type {
  IpcBleEvent,
  IpcBleRequest,
  IpcBleResponse,
  IpcClientTransport,
  IpcEventAcknowledgeResponse,
  IpcFailureResponse
} from '../ipc/protocol'

/** Tauri v2 plugin command registered by the Rust crate. */
export const TAURI_BLE_PLUGIN_COMMAND = 'plugin:unified-ble-manager|invoke'
const TAURI_BYTES_WIRE_TAG = '$__unifiedBleBytesV2'

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
    const response = await this.invokeCore<unknown>(this.command, {
      request: encodeTauriWireValue(request),
      ...this.eventChannelArgument(request)
    })
    return decodeIpcBleResponse<Attachment, Client>(response)
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
  assertEncodableTauriValue(value)
  if (value instanceof Uint8Array) {
    return { [TAURI_BYTES_WIRE_TAG]: Array.from(value) }
  }
  if (Array.isArray(value)) {
    return value.map(item => encodeTauriWireValue(item))
  }
  if (isWireRecord(value)) {
    const encoded: Record<string, unknown> = {}
    for (const [key, item] of Object.entries(value)) {
      encoded[key] = encodeTauriWireValue(item)
    }
    return encoded
  }
  if (value !== null && typeof value === 'object') {
    throw contractError('protocol.malformed', 'ipc', 'tauri.transport.encode-object')
  }
  return value
}

/** Reconstructs independently owned bytes from Rust responses and Channel messages. */
export function decodeTauriWireValue(value: unknown): unknown {
  if (value instanceof Uint8Array) {
    return new Uint8Array(value)
  }
  if (Array.isArray(value)) {
    return value.map(item => decodeTauriWireValue(item))
  }
  if (!isWireRecord(value)) {
    if (value !== null && typeof value === 'object') {
      throw contractError('protocol.malformed', 'ipc', 'tauri.transport.decode-object')
    }
    return value
  }
  if (Object.prototype.hasOwnProperty.call(value, TAURI_BYTES_WIRE_TAG)) {
    const keys = Object.keys(value)
    const bytes = value[TAURI_BYTES_WIRE_TAG]
    if (keys.length !== 1 || !Array.isArray(bytes) || !bytes.every(isByte)) {
      throw new TypeError('Malformed Unified BLE Tauri byte value')
    }
    return new Uint8Array(bytes)
  }
  const decoded: Record<string, unknown> = {}
  for (const [key, item] of Object.entries(value)) {
    decoded[key] = decodeTauriWireValue(item)
  }
  return decoded
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
    throw contractError('protocol.malformed', 'ipc', 'tauri.transport.response')
  }
  return decoded
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
    !exactKeys(record, ['attachment', 'attachmentId', 'versions', 'renderer', 'rendererLease']) ||
    !nonEmptyString(record.attachmentId) ||
    !isAttachment(record.attachment) ||
    !isIpcVersionAxes(record.versions) ||
    !isRenderer(record.renderer) ||
    !isLease(record.rendererLease)
  ) {
    return false
  }
  const attachment = wireRecord(record.attachment)
  if (attachment === null || attachment.attachmentId !== record.attachmentId) return false
  return true
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
    exactKeys(record, ['availability', 'authorization', 'power', 'backendGeneration', 'updatedAt', 'safeReason']) &&
    isAdapterAvailability(record.availability) &&
    isAdapterAuthorization(record.authorization) &&
    isAdapterPower(record.power) &&
    nonEmptyString(record.backendGeneration) &&
    finiteNumber(record.updatedAt) &&
    (record.safeReason === null || typeof record.safeReason === 'string')
  )
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
