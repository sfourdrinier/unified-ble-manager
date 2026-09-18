// src/ipc/protocol.ts

import type { CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { IpcClientIdentity, IpcClientLeaseIdentity, IpcEnvelope } from '../backend-contract/ipc'
import type { ConnectionState } from '../backend-contract/backend'
import type { ConnectionLifecycleCause } from '../backend-contract/connection-lifecycle'
import type {
  AttachmentId,
  IpcOperationCorrelation,
  IpcVersionAxes,
  IpcCompatibilityOffer,
  ProtocolAxis,
  SerializableRecord
} from '../backend-contract/primitives'
import { version, versionRange } from '../backend-contract/primitives'
import type {
  AdapterAuthorization,
  AdapterAvailability,
  AdapterPower,
  AttachmentRecord
} from '../backend-contract/identity'
import type { CapabilitySnapshot } from '../backend-contract/capabilities'

/** The one versioned request channel exposed by a host application's narrow preload bridge. */
export const IPC_BLE_PROTOCOL_CHANNEL = 'unified-ble-manager:v2'

/** The version of the lifecycle value carried by the desktop webview v2 IPC stream. */
export const IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION = 2

/** GATT database payload schema negotiated by the IPC protocol bootstrap. */
export const IPC_GATT_DATABASE_SCHEMA_VERSION = 2

/** Client-originated lifecycle stream identifiers occupy a reserved namespace. */
export const IPC_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX = 'connection-events-'

function singletonVersionRange<Axis extends ProtocolAxis>(axis: Axis, value: number) {
  const selected = Object.freeze(version(axis, value))
  return Object.freeze(versionRange(selected, selected))
}

/**
 * The desktop IPC protocol spoken by the Electron renderer/main pair and the
 * Tauri webview/plugin pair. Version 3 carries the caller deadline as a
 * relative `budgetMs`, an optional `commit` on normalized errors, `delivery` on
 * subscriptions and connection-lifecycle events. Version 4 adds the host's
 * attachment rebind after an adapter loss (`IPC_ATTACHMENT_STREAM_ID`). Both
 * ends offer exactly this version, so a peer speaking 3 is refused at
 * bootstrap as `protocol.incompatible` before any operation.
 */
export const IPC_PROTOCOL_VERSION = 4

/**
 * The reserved stream on which the host announces that it rebound a renderer
 * lease to the backend's new attachment after an adapter loss (protocol 4).
 * Only the host rebinds: it announces the rebind for one lease, naming the
 * attachment that lease held; the renderer adopts nothing else and can never
 * choose an attachment. Until the host has rebound a lease, work naming the
 * replaced attachment is refused `backend.reset`; after it, work naming the
 * replaced attachment is still refused (releases excepted).
 */
export const IPC_ATTACHMENT_STREAM_ID = 'attachment'

/** The serializable projection of one attachment (bootstrap snapshots report `heard: null`). */
export function ipcAttachmentRecordV2(attachment: AttachmentRecord<string>): IpcAttachmentRecordV2 {
  return Object.freeze({
    attachmentId: String(attachment.attachmentId),
    backendInstanceId: String(attachment.backendInstanceId),
    backendGeneration: String(attachment.backendGeneration),
    adapter: Object.freeze({
      adapterId: String(attachment.adapter.adapterId),
      displayName: attachment.adapter.displayName,
      state: Object.freeze({
        availability: attachment.adapter.state.availability,
        authorization: attachment.adapter.state.authorization,
        power: attachment.adapter.state.power,
        heard: null,
        backendGeneration: String(attachment.adapter.state.backendGeneration),
        updatedAt: Number(attachment.adapter.state.updatedAt),
        safeReason: attachment.adapter.state.safeReason
      }),
      adapterGeneration: String(attachment.adapter.adapterGeneration),
      limitations: Object.freeze([...attachment.adapter.limitations])
    })
  })
}

/** The item value announced on `IPC_ATTACHMENT_STREAM_ID`. */
export interface IpcAttachmentReboundV1<Attachment extends string> {
  readonly kind: 'backend-restarted'
  readonly schemaVersion: 1
  /** The attachment the lease held until the rebind. */
  readonly previousAttachmentId: string
  readonly attachmentId: AttachmentId<Attachment>
  readonly attachment: AttachmentRecord<Attachment>
}

/** One attachment-stream item as the host sends it (`{ kind: 'value', value }`). */
export interface IpcAttachmentReboundItemV1<Attachment extends string> {
  readonly kind: 'value'
  readonly value: IpcAttachmentReboundV1<Attachment>
}

/**
 * Whether `item` is a well-formed attachment rebind. Anything else is
 * refused by the caller.
 */
export function isIpcAttachmentReboundItem<Attachment extends string>(
  item: unknown
): item is IpcAttachmentReboundItemV1<Attachment> {
  const wrapper = recordOf(item)
  const value = recordOf(wrapper?.value)
  if (wrapper?.kind !== 'value' || value === null) return false
  if (value.kind !== 'backend-restarted' || value.schemaVersion !== 1) return false
  const attachment = recordOf(value.attachment)
  return (
    nonEmpty(value.previousAttachmentId) &&
    isAttachmentRecord(value.attachment) &&
    attachment !== null &&
    value.attachmentId === attachment.attachmentId
  )
}

function isAttachmentRecord(value: unknown): boolean {
  const record = recordOf(value)
  const adapter = recordOf(record?.adapter)
  const state = recordOf(adapter?.state)
  if (record === null || adapter === null || state === null) return false
  const limitations = adapter.limitations
  return (
    nonEmpty(record.attachmentId) &&
    nonEmpty(record.backendInstanceId) &&
    nonEmpty(record.backendGeneration) &&
    nonEmpty(adapter.adapterId) &&
    nonEmpty(adapter.adapterGeneration) &&
    (adapter.displayName === null || typeof adapter.displayName === 'string') &&
    Array.isArray(limitations) &&
    limitations.every(entry => typeof entry === 'string') &&
    state.backendGeneration === record.backendGeneration &&
    isAvailability(state.availability) &&
    isAuthorization(state.authorization) &&
    isPower(state.power) &&
    typeof state.updatedAt === 'number' &&
    Number.isFinite(state.updatedAt) &&
    (state.safeReason === null || typeof state.safeReason === 'string')
  )
}

function recordOf(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
    ? Object.fromEntries(Object.entries(value))
    : null
}

function nonEmpty(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0
}

function isAvailability(value: unknown): value is AdapterAvailability {
  return value === 'available' || value === 'unavailable' || value === 'unsupported' || value === 'unknown'
}

function isAuthorization(value: unknown): value is AdapterAuthorization {
  return (
    value === 'granted' ||
    value === 'denied' ||
    value === 'restricted' ||
    value === 'not-determined' ||
    value === 'unavailable' ||
    value === 'unknown'
  )
}

function isPower(value: unknown): value is AdapterPower {
  return value === 'on' || value === 'off' || value === 'resetting' || value === 'unsupported' || value === 'unknown'
}

/** The IPC versions implemented by this package's desktop webview client. */
export const IPC_CLIENT_COMPATIBILITY_OFFER: IpcCompatibilityOffer = Object.freeze({
  backendContract: singletonVersionRange('backend-contract', 1),
  capabilitySchema: singletonVersionRange('capability-schema', 1),
  eventSchema: singletonVersionRange('event-schema', 1),
  traceFormat: singletonVersionRange('trace-format', 1),
  ipcProtocol: singletonVersionRange('ipc-protocol', IPC_PROTOCOL_VERSION)
})

/** Validates the public client-originated lifecycle stream identifier format. */
export function isIpcConnectionEventsStreamHandle(value: string): boolean {
  return /^connection-events-[A-Za-z0-9][A-Za-z0-9-]*$/.test(value)
}

/** Serializable attachment identity carried with a connection lifecycle event. */
export interface IpcAttachmentRecordV2 extends SerializableRecord {
  readonly attachmentId: string
  readonly backendInstanceId: string
  readonly backendGeneration: string
  readonly adapter: IpcAdapterRecordV2
}

export interface IpcAdapterRecordV2 extends SerializableRecord {
  readonly adapterId: string
  readonly displayName: string | null
  readonly state: IpcAdapterStateV2
  readonly adapterGeneration: string
  readonly limitations: readonly string[]
}

export interface IpcAdapterStateV2 extends SerializableRecord {
  readonly availability: 'available' | 'unavailable' | 'unsupported' | 'unknown'
  /**
   * `'unknown'` when the platform exposes no per-application Bluetooth
   * authorization concept at all, or when this host did not query one. It is
   * the absence of a measurement and never a denial: `'not-determined'`
   * asserts a pending user decision and `'unavailable'` asserts the platform
   * withheld access, so a host that did not measure reports `'unknown'`,
   * exactly as `availability` and `power` already do. `safeReason` states why.
   */
  readonly authorization: 'granted' | 'denied' | 'restricted' | 'not-determined' | 'unavailable' | 'unknown'
  readonly power: 'on' | 'off' | 'resetting' | 'unsupported' | 'unknown'
  /**
   * Unsampled bootstrap snapshots are `null`. Live `adapter.state` is a
   * non-negative peripheral count from the host adapter. Absence of a
   * measurement is never `0`.
   */
  readonly heard: number | null
  readonly backendGeneration: string
  readonly updatedAt: number
  readonly safeReason: string | null
}

export type IpcCapabilitySnapshotV2 = CapabilitySnapshot

/** Versioned, data-only projection of one public ConnectionLifecycleEvent. */
export interface IpcConnectionLifecycleEventV2 extends SerializableRecord {
  readonly kind: 'connection-lifecycle'
  readonly schemaVersion: typeof IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION
  readonly attachment: IpcAttachmentRecordV2
  readonly attachmentId: string
  readonly peerId: string
  readonly connectionId: string
  readonly connectionGeneration: string
  readonly ownerLeaseId: string
  readonly sequence: number
  readonly backendIngressOrdinal: number | null
  readonly previous: ConnectionState
  readonly current: ConnectionState
  readonly cause: ConnectionLifecycleCause
}

/**
 * Result of the first connection lifecycle admission phase. `handle` is the
 * client-generated opaque handle confirmed by the host; the host begins
 * forwarding only after the matching readiness command.
 */
export interface IpcConnectionEventsSubscribeResponseV2 extends SerializableRecord {
  readonly handle: string
  readonly connectionId: string
  readonly connectionGeneration: string
  readonly eventSchemaVersion: typeof IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION
}

export interface IpcDiscoveryDescriptor extends SerializableRecord {
  readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid'
}

/**
 * Shared-core identity issued by the native host: the linked `ubm-core`
 * contract revision plus the host implementation version. The 5.0 lane
 * Tauri plugin always sends it; older native attachments omit it and the
 * 5.0 factory refuses them (F01).
 */
export interface IpcCoreIdentity extends SerializableRecord {
  readonly contractRevision: string
  readonly implementationVersion: string
}

/** Immutable bootstrap data issued by the host after it authenticates a client. */
export interface IpcClientBootstrap<Attachment extends string, Client extends string> {
  readonly attachment: AttachmentRecord<Attachment>
  readonly attachmentId: AttachmentId<Attachment>
  readonly versions: IpcVersionAxes
  readonly capabilities: IpcCapabilitySnapshotV2
  /** Host-issued discovery model. Older native attachments may omit it; the client then derives it from capabilities. */
  readonly discovery?: IpcDiscoveryDescriptor
  /** Shared-core identity. Older native attachments may omit it; the 5.0 Tauri factory then refuses admission. */
  readonly core?: IpcCoreIdentity
  readonly renderer: IpcClientIdentity<Attachment, Client>
  readonly rendererLease: IpcClientLeaseIdentity
}

/** Host-to-client bounded stream item. The preload must forward this unchanged. */
export interface IpcBleEvent {
  /** Exact bootstrap lifetime that owns this event. */
  readonly rendererLease: IpcClientLeaseIdentity
  /** Host-issued opaque identifier acknowledged after the preload delivers this event. */
  readonly eventId: string
  readonly streamId: string
  readonly item: SerializableRecord
}

export interface IpcBootstrapRequest {
  readonly kind: 'bootstrap'
  readonly offer: IpcCompatibilityOffer
}

export function createIpcBootstrapRequest(): IpcBootstrapRequest {
  return Object.freeze({ kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
}

export interface IpcRouteRequest<Attachment extends string, Client extends string, Operation extends string> {
  readonly kind: 'route'
  readonly envelope: IpcEnvelope<Attachment, Client, Operation>
  /** Local cancellation signal carried across nested host transports; never serialized on the wire. */
  readonly signal?: AbortSignal | null
}

export interface IpcReleaseRequest {
  readonly kind: 'release'
  readonly rendererLease: IpcClientLeaseIdentity
}

/** Acknowledges a host-to-client event after the preload has delivered it. */
export interface IpcEventAcknowledgeRequest {
  readonly kind: 'event.ack'
  readonly rendererLease: IpcClientLeaseIdentity
  readonly eventId: string
}

export type IpcBleRequest<Attachment extends string, Client extends string, Operation extends string> =
  | IpcBootstrapRequest
  | IpcRouteRequest<Attachment, Client, Operation>
  | IpcReleaseRequest
  | IpcEventAcknowledgeRequest

export interface IpcBootstrapResponse<Attachment extends string, Client extends string> {
  readonly kind: 'bootstrap'
  readonly bootstrap: IpcClientBootstrap<Attachment, Client>
}

export interface IpcRouteResponse {
  readonly kind: 'route'
  readonly payload: SerializableRecord
}

export interface IpcReleaseResponse {
  readonly kind: 'release'
  readonly cleanup: CleanupRecord
}

export interface IpcEventAcknowledgeResponse {
  readonly kind: 'event.ack'
}

/** Typed failure returned by the host-process IPC boundary; client code rehydrates it into a contract error. */
export interface IpcFailureResponse {
  readonly kind: 'failure'
  readonly error: NormalizedBleError
}

export type IpcBleSuccessResponse<Attachment extends string, Client extends string> =
  | IpcBootstrapResponse<Attachment, Client>
  | IpcRouteResponse
  | IpcReleaseResponse
  | IpcEventAcknowledgeResponse

export type IpcBleResponse<Attachment extends string, Client extends string> =
  | IpcBleSuccessResponse<Attachment, Client>
  | IpcFailureResponse

/**
 * Host-neutral client transport contract. It deliberately contains no Electron,
 * Tauri, Node, native-addon, or direct-radio import, so every desktop webview
 * host implements exactly these three operations.
 *
 * Event-sink lifecycle invariant, which every transport author must preserve:
 *
 * - The event sink is established exactly once, by the attach (`bootstrap`)
 *   request, and remains valid for the lifetime of that attachment. `subscribe`
 *   only adds and removes local listeners on that one already-established sink.
 * - `invoke` is strictly request/response. It must never carry, re-send, or
 *   rebind the event sink, and neither a `route`, a `release`, nor an
 *   `acknowledge` round trip may disturb event delivery.
 * - Re-attaching, meaning a further `bootstrap` request, is the only operation
 *   that rebinds the sink.
 *
 * This is a correctness rule, not a style preference. Tauri deserializes every
 * `Channel` command argument into a *new* Rust `Channel` bound to the one
 * JavaScript callback id, and dropping any one of them evals
 * `{ end: true, index }` for that shared id. The Tauri JS runtime answers an end
 * message whose index matches its next expected index by calling
 * `unregisterCallback`, which tears the shared callback down permanently. A
 * transport that re-sent its sink on a later request would therefore let the
 * request/response path silently kill the event path: connection lifecycle,
 * advertisement, and notification streams stop with no rejection and no error
 * anywhere. Hosts whose sink is a long-lived listener registration carry the
 * same requirement for the same reason: one binding, one attachment lifetime.
 *
 * `executePublicIpcTransportScenario` in
 * `src/tck/runner-public-ipc-transport-scenario.ts` proves this invariant
 * against any implementation of this interface.
 */
export interface IpcClientTransport<Attachment extends string, Client extends string> {
  invoke<Operation extends string>(
    request: IpcBleRequest<Attachment, Client, Operation>
  ): Promise<IpcBleResponse<Attachment, Client>>
  subscribe(listener: (event: IpcBleEvent) => void): () => void
  acknowledge(
    rendererLease: IpcClientLeaseIdentity,
    eventId: string
  ): Promise<IpcEventAcknowledgeResponse | IpcFailureResponse>
}

export interface IpcOperationRequest {
  readonly command: string
  readonly payload: SerializableRecord
  readonly binaryPayload: Uint8Array | null
  readonly signal: AbortSignal | null
}

export interface IpcOperationReceipt {
  readonly correlation: IpcOperationCorrelation<string, string>
  readonly payload: SerializableRecord
}
