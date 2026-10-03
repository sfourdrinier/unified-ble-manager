// src/backend-contract/connection-lifecycle.ts

import type { ConnectionState } from './backend'
import type { AttachmentRecord } from './identity'
import type { AttachmentId, ConnectionId, GenerationId, LeaseId, PeerId } from './primitives'

export type ConnectionLifecycleCause =
  | 'connected'
  | 'backend-transition'
  | 'requested-disconnect'
  | 'peer-link-loss'
  | 'adapter-loss'
  | 'backend-restart'
  | 'released'
  | 'manager-destroyed'
  | 'backend-failure'
export type ConnectionLifecycleTerminalCause = Exclude<ConnectionLifecycleCause, 'connected' | 'backend-transition'>

/** One attachment-, lease-, and generation-bound public connection transition. */
export interface ConnectionLifecycleEvent<Attachment extends string> {
  readonly kind: 'connection-lifecycle'
  readonly attachment: AttachmentRecord<Attachment>
  readonly attachmentId: AttachmentId<Attachment>
  readonly peerId: PeerId<Attachment>
  readonly connectionId: ConnectionId<Attachment, string>
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  readonly ownerLeaseId: LeaseId<Attachment, string>
  readonly sequence: number
  readonly backendIngressOrdinal: number | null
  readonly previous: ConnectionState
  readonly current: ConnectionState
  readonly cause: ConnectionLifecycleCause
  /**
   * Exact snapshotted native detail, limited to 16 KiB of UTF-8 JSON in the
   * connection lifecycle stream. An oversized observation ends that stream
   * with explicit overflow/error accounting; it is never silently truncated
   * or mistaken for successful lifecycle delivery. Resource cleanup is separate.
   */
  readonly platform?: import('./errors').PlatformErrorDetail
}
