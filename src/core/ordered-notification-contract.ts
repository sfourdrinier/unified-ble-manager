// src/core/ordered-notification-contract.ts

import type { CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { NotificationDelivery, NotificationReceipt } from '../backend-contract/gatt'
import type { StreamLimits } from '../backend-contract/streams'
import type { PortableCurrentCharacteristicPath } from '../manager/consumer-handles'

/** Canonical decimal representation of an unsigned 64-bit ingress ordinal. */
export type IngressOrdinal = string

/** Native source identity when the host provides one. */
export type OrderedSourceGeneration = { readonly kind: 'counter' | 'opaque'; readonly value: string }

/** Result of increasing one installed source's variable-evidence byte budget. */
export interface OrderedByteCapacityChange {
  readonly previousMaxBytes: number
  readonly maxBytes: number
  readonly maximumMaxBytes: number
}

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
  readonly sourceGeneration?: OrderedSourceGeneration | null
  /** Exact native member epoch when the host supplies it. */
  readonly sourceMembership?: OrderedNotificationSourceMembership | null
  readonly value: Uint8Array
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
  readonly lostValues: string | null
  readonly lostBytes: string | null
  /** Source records can include controls and must not be interpreted as values. */
  readonly lostSourceRecords: string | null
}

/** A host-ingress marker ordered with values and other controls. */
export interface OrderedNotificationMarker {
  readonly kind: 'marker'
  readonly collectionId: string
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly sourceGeneration?: OrderedSourceGeneration | null
  readonly sourceMembership?: OrderedNotificationSourceMembership | null
  readonly markerId: string
  readonly ordinal: IngressOrdinal
}

/** Exact native source identity retained for rejected fence evidence. */
export interface OrderedNotificationSourceMembership {
  readonly member: string
  readonly epoch: string
}

export type OrderedNotificationSourceRejection =
  | { readonly kind: 'value'; readonly bytes: string }
  | { readonly kind: 'loss'; readonly skippedRecords: string }
  | { readonly kind: 'fence'; readonly membership: OrderedNotificationSourceMembership }
  | { readonly kind: 'barrier'; readonly id: string }
  | { readonly kind: 'marker'; readonly markerId: string }
  | { readonly kind: 'ordinal-exhausted'; readonly bytes: string }

export interface OrderedNotificationRejectedLoss {
  readonly scope: OrderedNotificationLoss['scope']
  readonly unit: 'values' | 'source-records'
  readonly count: string | null
  readonly bytes: string | null
}

/** One source report, retained as a distinct occurrence even when payloads match. */
export interface OrderedNotificationFailureObservation {
  readonly kind: 'failure-observation'
  readonly failure: NormalizedBleError | null
  readonly sourceRejection: OrderedNotificationSourceRejection | null
  readonly sourceFailureMember: OrderedNotificationSourceMembership | null
  readonly rejectedLoss: OrderedNotificationRejectedLoss | null
  readonly tailUncertain: boolean
}

/** Evidence that a positive number of observations was actually omitted. */
export interface OrderedNotificationFailureObservationsOmitted {
  readonly kind: 'failure-observations-omitted'
  readonly count: string | null
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
    | 'host-reset'
    | 'operation-aborted'
    | 'operation-timed-out'
    | 'ordinal-exhausted'
  readonly uncertainTail: boolean
  readonly error?: NormalizedBleError | null
  /** A distinct source failure retained after the native terminal and before source seal. */
  readonly secondaryError?: NormalizedBleError | null
  /** Separately observed failures after the first; exact unless failureCountOverflowed is true. */
  readonly additionalFailureCount?: string
  /** The observed count exceeded its representation and is a lower bound. */
  readonly failureCountOverflowed?: boolean
  /** Native overflow may have omitted an unknown number of failures. */
  readonly failureObservationsOmitted?: boolean
  /** Failure to fence admission, separate from the terminal's source error. */
  readonly fenceError?: NormalizedBleError | null
  /** Exact evidence for a value refused by the collection's bounded queue. */
  readonly rejectedValues?: string | null
  readonly rejectedLoss?: OrderedNotificationRejectedLoss | null
  /** Evidence rejected at the shared native source, not attributed per member. */
  readonly sourceRejection?: OrderedNotificationSourceRejection | null
  readonly sourceFailureMember?: OrderedNotificationSourceMembership | null
}

export type OrderedNotificationItem =
  | OrderedNotificationValue
  | OrderedNotificationLoss
  | OrderedNotificationMarker
  | OrderedNotificationFailureObservation
  | OrderedNotificationFailureObservationsOmitted
  | OrderedNotificationTerminal

export interface OrderedNotificationCleanup {
  readonly group: CleanupRecord
  readonly members: readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]
  /** Cumulative discard receipt; repeated retry results must not be added together. */
  readonly discarded?: {
    readonly values: string
    readonly bytes: string
    readonly controls: string
    readonly uncertainTail: boolean
    readonly terminal?: OrderedNotificationTerminal | null
    readonly losses?: readonly OrderedNotificationLoss[]
    readonly markers?: readonly OrderedNotificationMarker[]
    readonly failureObservations?: readonly OrderedNotificationFailureObservation[]
    readonly failureObservationsOmitted?: readonly OrderedNotificationFailureObservationsOmitted[]
    /** Exact per-delivery evidence supplied by a trusted native discard. */
    readonly trustedValues?: readonly {
      readonly memberId: string
      readonly ordinal: string
      readonly sourceMembership: OrderedNotificationSourceMembership
      readonly bytes: string
    }[]
  }
}

export interface OrderedNotificationStream<
  Target = import('../backend-contract/gatt').CharacteristicPath<string, string, string, string, string, 'current'>
> extends AsyncIterable<OrderedNotificationItem, undefined, undefined> {
  reserveMarkerWrite?(): import('./ordered-marker-wire').OrderedMarkerWriteHandle<Target>
  /** Increase the current source budget without acknowledging retained evidence. */
  increaseByteCapacity?(maxBytes: number): Promise<OrderedByteCapacityChange>
  readonly limits: StreamLimits
  [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined>
  /** This result, rather than the terminal record, reports cleanup completion. */
  close(): Promise<OrderedNotificationCleanup>
  /** Fence and account for consumer-unread records before discarding them. */
  discard(): Promise<OrderedNotificationCleanup>
}

export interface OrderedNotificationSetupFailure {
  readonly sourceGeneration?: OrderedSourceGeneration | null
  readonly error: NormalizedBleError
  readonly members: readonly {
    readonly memberId: string
    readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
  }[]
  /** Bounded observations include explicit loss when retained observations are incomplete. */
  readonly observations: readonly Exclude<OrderedNotificationItem, OrderedNotificationTerminal>[]
  readonly uncertainTail: boolean
  readonly cleanup: OrderedNotificationCleanup
}

/** Backend setup preserves observations and a retryable cleanup owner even on failure. */
export type OrderedNotificationSetup<
  Target = import('../backend-contract/gatt').CharacteristicPath<string, string, string, string, string, 'current'>
> =
  | {
      readonly state: 'ready'
      readonly sourceGeneration?: OrderedSourceGeneration | null
      readonly members: readonly OrderedNotificationMember[]
      readonly stream: OrderedNotificationStream<Target>
    }
  | ({
      readonly state: 'failed'
      /** Remaining observations and final terminal, including late admitted values. */
      readonly stream: OrderedNotificationStream<Target>
      readonly close: () => Promise<OrderedNotificationCleanup>
    } & OrderedNotificationSetupFailure)
