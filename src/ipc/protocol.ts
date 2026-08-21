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
import type { AttachmentRecord } from '../backend-contract/identity'
import type { CapabilitySnapshot } from '../backend-contract/capabilities'

/** The one versioned request channel exposed by a host application's narrow preload bridge. */
export const IPC_BLE_PROTOCOL_CHANNEL = 'unified-ble-manager:v2'

/** The version of the lifecycle value carried by the desktop webview v2 IPC stream. */
export const IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION = 2

/** Client-originated lifecycle stream identifiers occupy a reserved namespace. */
export const IPC_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX = 'connection-events-'

function singletonVersionRange<Axis extends ProtocolAxis>(axis: Axis, value: number) {
  const selected = Object.freeze(version(axis, value))
  return Object.freeze(versionRange(selected, selected))
}

/** The IPC versions implemented by this package's desktop webview client. */
export const IPC_CLIENT_COMPATIBILITY_OFFER: IpcCompatibilityOffer = Object.freeze({
  backendContract: singletonVersionRange('backend-contract', 1),
  capabilitySchema: singletonVersionRange('capability-schema', 1),
  eventSchema: singletonVersionRange('event-schema', 1),
  traceFormat: singletonVersionRange('trace-format', 1),
  ipcProtocol: singletonVersionRange('ipc-protocol', 2)
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

/** Immutable bootstrap data issued by the host after it authenticates a client. */
export interface IpcClientBootstrap<Attachment extends string, Client extends string> {
  readonly attachment: AttachmentRecord<Attachment>
  readonly attachmentId: AttachmentId<Attachment>
  readonly versions: IpcVersionAxes
  readonly capabilities: IpcCapabilitySnapshotV2
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
