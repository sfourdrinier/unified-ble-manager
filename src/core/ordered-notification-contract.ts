// src/core/ordered-notification-contract.ts

import type { CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { NotificationDelivery, NotificationReceipt } from '../backend-contract/gatt'
import type { OwnedBytes } from '../backend-contract/primitives'
import type { StreamLimits } from '../backend-contract/streams'
import type { PortableCurrentCharacteristicPath } from '../manager/consumer-handles'

/** Canonical decimal representation of an unsigned 64-bit ingress ordinal. */
export type IngressOrdinal = string

const MAX_INGRESS_ORDINAL = '18446744073709551615'

/** Validates untrusted wire data without losing precision through a JS number. */
export function isCanonicalIngressOrdinal(value: unknown): value is IngressOrdinal {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)$/.test(value)) return false
  return (
    value.length < MAX_INGRESS_ORDINAL.length ||
    (value.length === MAX_INGRESS_ORDINAL.length && value <= MAX_INGRESS_ORDINAL)
  )
}

export interface OrderedNotificationMember {
  readonly memberId: string
  readonly path: PortableCurrentCharacteristicPath
  readonly requestedDelivery:
    | 'prefer-notification'
    | 'prefer-indication'
    | 'require-notification'
    | 'require-indication'
  readonly effectiveDelivery: NotificationDelivery
}

export interface OrderedNotificationValue {
  readonly kind: 'value'
  readonly collectionId: string
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly memberId: string
  readonly ordinal: IngressOrdinal
  readonly value: OwnedBytes
  readonly delivery: NotificationDelivery
  /** Callback classification is independent of notification versus indication. */
  readonly provenance: 'notification' | 'read-or-notification' | 'unknown'
  readonly receipt?: NotificationReceipt
}

export interface OrderedNotificationLoss {
  readonly kind: 'loss'
  readonly scope:
    | { readonly kind: 'member'; readonly memberId: string }
    | { readonly kind: 'group' }
    | { readonly kind: 'unknown' }
  readonly reason: 'overflow' | 'source-loss' | 'control-loss'
  /** Null means the source cannot attribute an exact count. */
  readonly lostValues: number | null
  readonly lostBytes: number | null
  /** Source records can include controls and must not be interpreted as values. */
  readonly lostSourceRecords: number | null
}

export interface OrderedNotificationTerminal {
  readonly kind: 'terminal'
  readonly reason:
    | 'closed'
    | 'overflow'
    | 'source-failed'
    | 'owner-released'
    | 'connection-lost'
    | 'service-changed'
    | 'operation-aborted'
    | 'operation-timed-out'
    | 'ordinal-exhausted'
  readonly uncertainTail: boolean
  readonly error?: NormalizedBleError | null
  /** Failure to fence admission, separate from the terminal's source error. */
  readonly fenceError?: NormalizedBleError | null
}

export type OrderedNotificationItem = OrderedNotificationValue | OrderedNotificationLoss | OrderedNotificationTerminal

export interface OrderedNotificationCleanup {
  readonly group: CleanupRecord
  readonly members: readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]
}

export interface OrderedNotificationStream extends AsyncIterable<OrderedNotificationItem, undefined, undefined> {
  readonly limits: StreamLimits
  [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined>
  /** This result, rather than the terminal record, reports cleanup completion. */
  close(): Promise<OrderedNotificationCleanup>
}

export interface OrderedNotificationSetupFailure {
  readonly error: NormalizedBleError
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
  }[]
  /** Bounded observations include explicit loss when retained observations are incomplete. */
  readonly observations: readonly (OrderedNotificationValue | OrderedNotificationLoss)[]
  readonly uncertainTail: boolean
  readonly cleanup: OrderedNotificationCleanup
}
