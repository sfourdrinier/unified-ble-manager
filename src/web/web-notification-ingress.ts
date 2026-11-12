// src/web/web-notification-ingress.ts

import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'
import type { OrderedNotificationMarker } from '../core/ordered-notification-contract'

/** Internal source records; the eventual group operation owns translation and cleanup. */
export type WebIngressTerminalReason =
  | 'closed'
  | 'owner-released'
  | 'connection-lost'
  | 'source-failed'
  | 'service-changed'
  | 'host-reset'
  | 'operation-aborted'
  | 'operation-timed-out'
  | 'overflow'
  | 'ordinal-exhausted'

export type WebIngressRecord =
  | OrderedNotificationMarker
  | { readonly kind: 'failure-observations-omitted'; readonly count: string | null }
  | {
      readonly kind: 'failure-observation'
      readonly failure: null
      readonly sourceRejection: { readonly kind: 'value' | 'ordinal-exhausted'; readonly bytes: string }
      readonly sourceFailureMember: { readonly member: string; readonly epoch: string } | null
      readonly rejectedLoss: null
      readonly tailUncertain: boolean
    }
  | {
      readonly kind: 'value'
      readonly memberId: string
      readonly ordinal: string
      readonly value: Uint8Array
      readonly sourceMembership?: { readonly member: string; readonly epoch: string }
      readonly delivery?: 'unknown'
      readonly provenance?: 'read-or-notification'
    }
  | {
      readonly kind: 'loss'
      readonly lostValues: number
      readonly lostBytes: number
      readonly reason?: 'source-failed'
    }
  | {
      readonly kind: 'terminal'
      readonly reason: WebIngressTerminalReason
      readonly uncertainTail: boolean
    }

export interface WebIngressGroup {
  readonly terminal: boolean
  readonly terminalReason: WebIngressTerminalReason | null
  drain(): readonly WebIngressRecord[]
  admitMarker(admit: (ordinal: string) => boolean): string | null
  retire(reason: WebIngressTerminalReason): void
  onTerminal(callback: () => void): void
}

/** A private source queue for a recorder. A peek never transfers ownership. */
export interface WebRetainedIngressGroup {
  readonly terminal: boolean
  readonly terminalReason: WebIngressTerminalReason | null
  memberships(): readonly { readonly member: string; readonly epoch: string }[] | null
  peek(): { readonly id: string; readonly record: WebIngressRecord } | null
  ackExact(id: string): boolean
  /** One control credit is independent of the configured value capacity. */
  reserveMarker(collectionId: string, markerId: string): void
  admitMarker(admit: (ordinal: string) => boolean): string | null
  admitReservedMarker(marker: OrderedNotificationMarker): boolean
  releaseMarkerNotAdmitted(collectionId: string, markerId: string): void
  markerAdmissionStatus(
    collectionId: string,
    markerId: string
  ):
    | { readonly state: 'reserved' | 'rejected' | 'not-admitted' | 'unknown' }
    | { readonly state: 'admitted'; readonly ordinal: string }
  retire(reason: WebIngressTerminalReason): void
  /** Stop routing and publish the terminal after member cleanup has fenced callbacks. */
  seal?(): void
  onTerminal(callback: () => void): void
}

/** A single-use allocation; reserving does not register routes or listeners. */
export interface WebRetainedIngressReservation {
  memberships(): readonly { readonly member: string; readonly epoch: string }[]
  install(wake: () => void): WebRetainedIngressGroup
}

/** A failed removal has an ambiguous effect and cannot be presumed undone. */
export class WebIngressRegistrationCleanupError extends AggregateError {
  constructor(
    failures: readonly unknown[],
    private readonly retryCleanup?: () => void,
    readonly latestCleanupErrors: readonly unknown[] = failures.slice(1)
  ) {
    super(failures, 'Web ingress registration cleanup failed')
  }

  retry(): void {
    if (this.retryCleanup === undefined) throw this
    this.retryCleanup()
  }
}

interface GroupState {
  readonly members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>
  readonly pendingMembershipRemoval: Set<WebBluetoothCharacteristicBoundary>
  readonly sink: ((memberId: string, ordinal: string, value: Uint8Array) => boolean) | null
  readonly capacity: number
  readonly byteCapacity: number
  readonly records: WebIngressRecord[]
  readonly retained: boolean
  readonly recordingVersion: 1 | 2
  readonly deliveryIds: string[]
  readonly memberships: ReadonlyMap<
    WebBluetoothCharacteristicBoundary,
    { readonly member: string; readonly epoch: string }
  > | null
  readonly wake: (() => void) | null
  pendingWakes: number
  pendingSinkValues: number
  pendingSinkBytes: number
  readonly pendingSinks: {
    readonly memberId: string
    readonly ordinal: string
    readonly value: Uint8Array
    scheduled: boolean
  }[]
  deliveringSink: boolean
  sinkFailed: boolean
  readonly failedSinkRecords: WebIngressRecord[]
  nextDeliveryId: bigint
  valueCount: number
  marker: {
    readonly collectionId: string
    readonly markerId: string
    record: OrderedNotificationMarker | null
    acknowledged: boolean
    rejected: boolean
  } | null
  markerOrdinal: string | null
  bytes: number
  terminal: boolean
  terminalReason: WebIngressTerminalReason | null
  terminalCallback: (() => void) | null
  terminalScheduled: boolean
  sealed: boolean
  failureCount: number
  omissionPending: boolean
}

const MAX_ORDINAL = 18446744073709551615n

function copyMarker(marker: OrderedNotificationMarker): OrderedNotificationMarker {
  return {
    ...marker,
    ...(marker.sourceGeneration == null ? {} : { sourceGeneration: { ...marker.sourceGeneration } }),
    ...(marker.sourceMembership == null ? {} : { sourceMembership: { ...marker.sourceMembership } })
  }
}

function admittedMarkerOrdinal(group: GroupState): string | undefined {
  return group.marker?.record?.ordinal
}

/** Synchronous DOM callbacks, group registration, and retirement share this owner. */
export class WebNotificationIngress {
  private readonly groups = new Set<GroupState>()
  private ordinal = 0n
  private membershipEpoch = 0n
  private retired = false
  private readonly pendingCallbacks: (() => void)[] = []
  private flushingCallbacks = false
  private callbackHolds = 0

  constructor(
    private readonly membershipChanged?: (
      characteristic: WebBluetoothCharacteristicBoundary,
      added: boolean,
      registration: object
    ) => void,
    private readonly retryMembershipRemoval = false,
    private readonly membershipRemovalStarting?: (
      characteristic: WebBluetoothCharacteristicBoundary,
      registration: object
    ) => void,
    private readonly membershipRemovalFinished?: (
      characteristic: WebBluetoothCharacteristicBoundary,
      registration: object
    ) => void
  ) {}

  /** Defer externally supplied wakes until a physical observation reaches every graph. */
  holdCallbacks(): void {
    this.callbackHolds += 1
  }

  releaseCallbacks(): void {
    if (this.callbackHolds === 0) throw new Error('Unbalanced Web ingress callback hold')
    this.callbackHolds -= 1
    this.flushCallbacks()
  }

  register(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    sink: ((memberId: string, ordinal: string, value: Uint8Array) => boolean) | null = null
  ): WebIngressGroup {
    return this.registerGroup(members, capacity, byteCapacity, sink, false).group
  }

  registerRetained(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    options?: { readonly wake?: () => void; readonly membership?: boolean }
  ): WebRetainedIngressGroup {
    return this.retainedGroup(members, capacity, byteCapacity, options)
  }

  reserveRetainedMemberships(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    recordingVersion: 1 | 2 = 1
  ): WebRetainedIngressReservation {
    this.validateGroup(members, capacity, byteCapacity, true)
    const snapshot = new Map(members)
    const memberships = new Map(
      [...snapshot].map(([characteristic, member]) => {
        this.membershipEpoch += 1n
        return [characteristic, { member, epoch: this.membershipEpoch.toString() }] as const
      })
    )
    let used = false
    return {
      memberships: () => [...memberships.values()].map(value => ({ ...value })),
      install: wake => {
        if (used) throw new Error('Web ingress membership reservation already consumed')
        used = true
        return this.retainedGroup(snapshot, capacity, byteCapacity, { wake }, memberships, recordingVersion)
      }
    }
  }

  private retainedGroup(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    options?: { readonly wake?: () => void; readonly membership?: boolean },
    reservedMemberships?: ReadonlyMap<
      WebBluetoothCharacteristicBoundary,
      { readonly member: string; readonly epoch: string }
    >,
    recordingVersion: 1 | 2 = 1
  ): WebRetainedIngressGroup {
    const { group, state } = this.registerGroup(
      members,
      capacity,
      byteCapacity,
      null,
      true,
      options,
      reservedMemberships,
      recordingVersion
    )
    return {
      get terminal() {
        return group.terminal
      },
      get terminalReason() {
        return group.terminalReason
      },
      memberships: () =>
        state.memberships === null ? null : [...state.memberships.values()].map(value => ({ ...value })),
      peek: () => {
        const record = state.records[0]
        const id = state.deliveryIds[0]
        if (record === undefined || id === undefined) return null
        // The last omission credit must remain open until fencing proves that
        // no further observations can need the same reserved identity.
        if (!state.sealed && record.kind === 'failure-observations-omitted' && id === (MAX_ORDINAL - 1n).toString())
          return null
        return {
          id,
          record:
            record.kind === 'value'
              ? {
                  ...record,
                  value: new Uint8Array(record.value),
                  ...(record.sourceMembership === undefined ? {} : { sourceMembership: { ...record.sourceMembership } })
                }
              : record.kind === 'marker'
                ? copyMarker(record)
                : record.kind === 'failure-observation'
                  ? {
                      ...record,
                      sourceRejection: { ...record.sourceRejection },
                      sourceFailureMember:
                        record.sourceFailureMember === null ? null : { ...record.sourceFailureMember }
                    }
                  : { ...record }
        }
      },
      ackExact: id => {
        if (state.deliveryIds[0] !== id) return false
        if (
          !state.sealed &&
          state.records[0]?.kind === 'failure-observations-omitted' &&
          id === (MAX_ORDINAL - 1n).toString()
        )
          return false
        const record = state.records.shift()
        state.deliveryIds.shift()
        if (record?.kind === 'value') {
          state.bytes -= record.value.byteLength
          state.valueCount -= 1
        }
        if (record?.kind === 'marker' && state.marker?.record?.ordinal === record.ordinal)
          state.marker.acknowledged = true
        if (record?.kind === 'failure-observation') state.failureCount -= 1
        if (record?.kind === 'failure-observations-omitted') state.omissionPending = false
        return true
      },
      reserveMarker: (collectionId, markerId) => {
        if (
          this.retired ||
          state.terminal ||
          state.markerOrdinal !== null ||
          (state.marker !== null && !state.marker.acknowledged) ||
          state.nextDeliveryId > MAX_ORDINAL - 2n
        )
          throw new Error('Web retained marker credit unavailable')
        state.marker = { collectionId, markerId, record: null, acknowledged: false, rejected: false }
      },
      admitMarker: admit => {
        if (
          this.retired ||
          state.terminal ||
          state.markerOrdinal !== null ||
          state.marker === null ||
          state.marker.record !== null ||
          state.marker.rejected
        )
          return null
        if (this.ordinal === MAX_ORDINAL) {
          this.retire('ordinal-exhausted')
          return null
        }
        this.ordinal += 1n
        const ordinal = this.ordinal.toString()
        state.markerOrdinal = ordinal
        try {
          const accepted = admit(ordinal)
          if (accepted !== (admittedMarkerOrdinal(state) === ordinal))
            throw new Error('Inconsistent Web retained marker admission')
          if (!accepted && state.marker !== null) {
            state.marker.rejected = true
            state.marker.acknowledged = true
          }
          return accepted ? ordinal : null
        } finally {
          state.markerOrdinal = null
          this.flushCallbacks()
        }
      },
      admitReservedMarker: marker => {
        const credit = state.marker
        if (
          credit === null ||
          credit.record !== null ||
          state.terminal ||
          state.markerOrdinal !== marker.ordinal ||
          this.ordinal.toString() !== marker.ordinal ||
          credit.collectionId !== marker.collectionId ||
          credit.markerId !== marker.markerId ||
          state.nextDeliveryId > MAX_ORDINAL - 2n
        )
          return false
        const record = copyMarker(marker)
        credit.record = record
        this.append(state, record)
        return true
      },
      releaseMarkerNotAdmitted: (collectionId, markerId) => {
        const credit = state.marker
        if (
          credit === null ||
          credit.collectionId !== collectionId ||
          credit.markerId !== markerId ||
          credit.record !== null
        )
          throw new Error('Invalid Web retained marker credit')
        state.marker = null
      },
      markerAdmissionStatus: (collectionId, markerId) => {
        const credit = state.marker
        if (credit === null || credit.collectionId !== collectionId || credit.markerId !== markerId)
          return { state: 'unknown' }
        if (credit.rejected) return { state: 'rejected' }
        return credit.record === null ? { state: 'reserved' } : { state: 'admitted', ordinal: credit.record.ordinal }
      },
      retire: group.retire,
      seal: () => this.seal(state),
      onTerminal: group.onTerminal
    }
  }

  private registerGroup(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    sink: ((memberId: string, ordinal: string, value: Uint8Array) => boolean) | null,
    retained: boolean,
    options?: { readonly wake?: () => void; readonly membership?: boolean },
    reservedMemberships?: ReadonlyMap<
      WebBluetoothCharacteristicBoundary,
      { readonly member: string; readonly epoch: string }
    >,
    recordingVersion: 1 | 2 = 1
  ): { group: WebIngressGroup; state: GroupState } {
    this.validateGroup(members, capacity, byteCapacity, options?.membership === true)
    const memberships =
      reservedMemberships ??
      (options?.membership === true
        ? new Map(
            [...members].map(([characteristic, member]) => {
              this.membershipEpoch += 1n
              return [characteristic, { member, epoch: this.membershipEpoch.toString() }] as const
            })
          )
        : null)
    const group: GroupState = {
      members: new Map(members),
      sink,
      capacity,
      byteCapacity,
      records: [],
      retained,
      recordingVersion,
      deliveryIds: [],
      memberships,
      wake: options?.wake ?? null,
      pendingMembershipRemoval: new Set(members.keys()),
      pendingWakes: 0,
      pendingSinkValues: 0,
      pendingSinkBytes: 0,
      pendingSinks: [],
      deliveringSink: false,
      sinkFailed: false,
      failedSinkRecords: [],
      nextDeliveryId: 1n,
      valueCount: 0,
      marker: null,
      markerOrdinal: null,
      bytes: 0,
      terminal: false,
      terminalReason: null,
      terminalCallback: null,
      terminalScheduled: false,
      sealed: false,
      failureCount: 0,
      omissionPending: false
    }
    this.groups.add(group)
    const registered: WebBluetoothCharacteristicBoundary[] = []
    try {
      for (const characteristic of group.members.keys()) {
        registered.push(characteristic)
        this.membershipChanged?.(characteristic, true, group)
      }
    } catch (error) {
      this.groups.delete(group)
      const failures: unknown[] = [error]
      let pending = registered.reverse()
      const rollback = (): void => {
        const remaining: WebBluetoothCharacteristicBoundary[] = []
        const latestFailures: unknown[] = []
        for (const characteristic of pending) {
          try {
            this.membershipChanged?.(characteristic, false, group)
          } catch (cleanupError) {
            failures.push(cleanupError)
            latestFailures.push(cleanupError)
            remaining.push(characteristic)
          }
        }
        pending = remaining
        if (pending.length > 0)
          throw new WebIngressRegistrationCleanupError(
            [...failures],
            this.retryMembershipRemoval ? rollback : undefined,
            latestFailures
          )
      }
      rollback()
      throw error
    }
    const result: WebIngressGroup = {
      get terminal() {
        return group.terminal
      },
      get terminalReason() {
        return group.terminalReason
      },
      drain: () => {
        const records = [...group.failedSinkRecords.splice(0), ...group.records.splice(0)]
        group.deliveryIds.splice(0)
        group.bytes = 0
        group.valueCount = 0
        return records
      },
      admitMarker: admit => {
        let markerResult: string | null = null
        const failures: unknown[] = []
        try {
          // An in-flight sink has not established whether its value was accepted.
          if (!this.retired && !group.terminal && !group.deliveringSink) {
            while (group.pendingSinks.length > 0 && !group.terminal) this.deliverSink(group)
            if (!this.retired && !group.terminal) {
              if (this.ordinal === MAX_ORDINAL) this.retire('ordinal-exhausted')
              else {
                this.ordinal += 1n
                const ordinal = this.ordinal.toString()
                markerResult = admit(ordinal) ? ordinal : null
              }
            }
          }
        } catch (error) {
          failures.push(error)
        }
        try {
          this.flushCallbacks()
        } catch (error) {
          failures.push(error)
        }
        if (failures.length === 1) throw failures[0]
        if (failures.length > 1) throw new AggregateError(failures, 'Web marker admission callbacks failed')
        return markerResult
      },
      retire: reason => {
        const failures: unknown[] = []
        try {
          this.finish(group, reason, true)
        } catch (error) {
          failures.push(error)
        }
        while (!group.deliveringSink && group.pendingSinks.length > 0) {
          try {
            this.deliverSink(group)
          } catch (error) {
            failures.push(error)
          }
        }
        try {
          this.flushCallbacks()
        } catch (error) {
          failures.push(error)
        }
        if (failures.length === 1) throw failures[0]
        if (failures.length > 1) throw new AggregateError(failures, 'Web ingress retirement failed')
      },
      onTerminal: callback => {
        group.terminalCallback = callback
        this.scheduleTerminal(group)
      }
    }
    return { group: result, state: group }
  }

  private validateGroup(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    allocateMembership: boolean
  ): void {
    if (
      this.retired ||
      members.size === 0 ||
      !Number.isSafeInteger(capacity) ||
      capacity < 1 ||
      !Number.isSafeInteger(byteCapacity) ||
      byteCapacity < 0 ||
      new Set(members.values()).size !== members.size ||
      (allocateMembership && this.membershipEpoch + BigInt(members.size) > MAX_ORDINAL)
    )
      throw new Error('Invalid or retired Web notification ingress group')
  }

  admit(characteristic: WebBluetoothCharacteristicBoundary, value: Uint8Array): void {
    if (this.retired && this.groups.size === 0) return
    const failures: unknown[] = []
    if (this.ordinal === MAX_ORDINAL) {
      for (const group of this.groups) {
        try {
          if (group.terminal && group.retained && group.recordingVersion === 2 && group.members.has(characteristic))
            this.observeRejection(group, value.byteLength, characteristic, 'ordinal-exhausted')
          else
            this.finish(
              group,
              'ordinal-exhausted',
              true,
              group.members.has(characteristic) ? value.byteLength : undefined,
              characteristic
            )
        } catch (error) {
          failures.push(error)
        }
      }
      this.retired = true
      try {
        this.flushCallbacks()
      } catch (error) {
        failures.push(error)
      }
      if (failures.length === 1) throw failures[0]
      if (failures.length > 1) throw new AggregateError(failures, 'Web ingress admission failed')
      return
    }
    this.ordinal += 1n
    const ordinal = this.ordinal.toString()
    for (const group of this.groups) {
      try {
        const memberId = group.members.get(characteristic)
        if (memberId === undefined) continue
        if (group.terminal) {
          if (group.retained && group.recordingVersion === 2 && !group.sealed)
            this.observeRejection(group, value.byteLength, characteristic, 'value')
          continue
        }
        // Keep two identities available for the rejected observation and terminal.
        if (
          group.retained &&
          group.nextDeliveryId > MAX_ORDINAL - (group.marker !== null && group.marker.record === null ? 3n : 2n)
        ) {
          this.finish(group, 'ordinal-exhausted', true, value.byteLength, characteristic)
          continue
        }
        if (group.sink !== null) {
          if (
            group.pendingSinkValues >= group.capacity ||
            group.pendingSinkBytes + value.byteLength > group.byteCapacity
          ) {
            this.finish(group, 'overflow', true, value.byteLength, characteristic)
            continue
          }
          const bytes = new Uint8Array(value)
          group.pendingSinkValues += 1
          group.pendingSinkBytes += bytes.byteLength
          const item = { memberId, ordinal, value: bytes, scheduled: false }
          group.pendingSinks.push(item)
          this.scheduleSink(group, item)
          continue
        }
        if (group.valueCount >= group.capacity || group.bytes + value.byteLength > group.byteCapacity) {
          this.finish(group, 'overflow', true, value.byteLength, characteristic)
          continue
        }
        const bytes = new Uint8Array(value)
        group.bytes += bytes.byteLength
        group.valueCount += 1
        const membership = group.memberships?.get(characteristic)
        this.append(group, {
          kind: 'value',
          memberId,
          ordinal,
          value: bytes,
          ...(membership === undefined
            ? {}
            : {
                sourceMembership: membership,
                delivery: 'unknown' as const,
                provenance: 'read-or-notification' as const
              })
        })
      } catch (error) {
        failures.push(error)
      }
    }
    try {
      this.flushCallbacks()
    } catch (error) {
      failures.push(error)
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Web ingress admission failed')
  }

  retire(reason: WebIngressTerminalReason): void {
    this.retired = true
    const failures: unknown[] = []
    for (const group of this.groups) {
      try {
        this.finish(group, reason, true)
      } catch (error) {
        failures.push(error)
      }
    }
    try {
      this.flushCallbacks()
    } catch (error) {
      failures.push(error)
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Web ingress retirement failed')
  }

  private finish(
    group: GroupState,
    reason: WebIngressTerminalReason,
    uncertainTail: boolean,
    rejectedBytes?: number,
    rejectedCharacteristic?: WebBluetoothCharacteristicBoundary
  ): void {
    if (group.terminal) {
      if (!(group.retained && group.recordingVersion === 2)) this.detach(group)
      return
    }
    group.terminal = true
    group.terminalReason = reason
    if (rejectedBytes !== undefined && group.recordingVersion === 2)
      this.observeRejection(
        group,
        rejectedBytes,
        rejectedCharacteristic,
        reason === 'ordinal-exhausted' ? 'ordinal-exhausted' : 'value'
      )
    else if (rejectedBytes !== undefined)
      this.append(group, {
        kind: 'loss',
        lostValues: 1,
        lostBytes: rejectedBytes,
        ...(reason === 'source-failed' ? { reason: 'source-failed' as const } : {})
      })
    const failures: unknown[] = []
    if (!(group.retained && group.recordingVersion === 2)) {
      try {
        this.detach(group)
      } catch (error) {
        failures.push(error)
      }
      this.append(group, { kind: 'terminal', reason, uncertainTail })
    }
    this.scheduleTerminal(group)
    if (failures.length > 0) throw failures[0]
  }

  private observeRejection(
    group: GroupState,
    bytes: number,
    characteristic: WebBluetoothCharacteristicBoundary | undefined,
    kind: 'value' | 'ordinal-exhausted'
  ): void {
    if (group.omissionPending) return
    if (group.failureCount >= group.capacity || group.nextDeliveryId >= MAX_ORDINAL - 1n) {
      group.omissionPending = true
      this.append(group, { kind: 'failure-observations-omitted', count: null })
      return
    }
    group.failureCount += 1
    this.append(group, {
      kind: 'failure-observation',
      failure: null,
      sourceRejection: { kind, bytes: String(bytes) },
      sourceFailureMember: characteristic === undefined ? null : (group.memberships?.get(characteristic) ?? null),
      rejectedLoss: null,
      tailUncertain: true
    })
  }

  private detach(group: GroupState): void {
    const failures: unknown[] = []
    for (const characteristic of group.pendingMembershipRemoval) {
      try {
        // Keep a separate dispatch route until removal returns successfully.
        this.membershipRemovalStarting?.(characteristic, group)
        this.membershipChanged?.(characteristic, false, group)
        this.membershipRemovalFinished?.(characteristic, group)
        group.pendingMembershipRemoval.delete(characteristic)
      } catch (error) {
        failures.push(error)
      }
    }
    if (group.pendingMembershipRemoval.size === 0) this.groups.delete(group)
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Web ingress membership removal failed')
  }

  private seal(group: GroupState): void {
    if (group.sealed || !group.terminal || group.recordingVersion !== 2) return
    this.detach(group)
    group.sealed = true
    this.append(group, { kind: 'terminal', reason: group.terminalReason ?? 'source-failed', uncertainTail: true })
    this.flushCallbacks()
  }

  private scheduleTerminal(group: GroupState): void {
    if (
      !group.terminal ||
      group.terminalCallback === null ||
      group.terminalScheduled ||
      group.deliveringSink ||
      group.pendingSinks.length > 0
    )
      return
    group.terminalScheduled = true
    this.pendingCallbacks.push(() => group.terminalCallback?.())
  }

  private scheduleSink(group: GroupState, item: GroupState['pendingSinks'][number]): void {
    if (item.scheduled) return
    item.scheduled = true
    this.pendingCallbacks.push(() => {
      item.scheduled = false
      if (group.pendingSinks[0] === item) this.deliverSink(group)
    })
  }

  private deliverSink(group: GroupState): void {
    if (group.deliveringSink) return
    const item = group.pendingSinks.shift()
    if (item === undefined) return
    group.pendingSinkValues -= 1
    group.pendingSinkBytes -= item.value.byteLength
    if (group.sinkFailed) {
      group.failedSinkRecords.push({
        kind: 'loss',
        lostValues: 1,
        lostBytes: item.value.byteLength,
        reason: 'source-failed'
      })
      const next = group.pendingSinks[0]
      if (next !== undefined) this.scheduleSink(group, next)
      this.scheduleTerminal(group)
      return
    }
    group.deliveringSink = true
    try {
      if (!group.sink?.(item.memberId, item.ordinal, item.value)) this.finish(group, 'overflow', true)
    } catch (error) {
      group.sinkFailed = true
      group.failedSinkRecords.push({
        kind: 'loss',
        lostValues: 1,
        lostBytes: item.value.byteLength,
        reason: 'source-failed'
      })
      try {
        this.finish(group, 'source-failed', true)
      } catch (cleanupError) {
        throw new AggregateError([error, cleanupError], 'Web ingress sink and retirement failed')
      }
      throw error
    } finally {
      group.deliveringSink = false
      const next = group.pendingSinks[0]
      if (next !== undefined) this.scheduleSink(group, next)
      this.scheduleTerminal(group)
    }
  }

  private append(group: GroupState, record: WebIngressRecord): void {
    group.records.push(record)
    if (group.retained) {
      group.deliveryIds.push(group.nextDeliveryId.toString())
      group.nextDeliveryId += 1n
    }
    if (group.wake !== null && (group.pendingWakes === 0 || (this.callbackHolds === 0 && !this.flushingCallbacks))) {
      group.pendingWakes += 1
      this.pendingCallbacks.push(() => {
        group.pendingWakes -= 1
        group.wake?.()
      })
    }
  }

  private flushCallbacks(): void {
    if (this.flushingCallbacks || this.callbackHolds !== 0) return
    this.flushingCallbacks = true
    const failures: unknown[] = []
    try {
      while (this.callbackHolds === 0 && this.pendingCallbacks.length > 0) {
        try {
          this.pendingCallbacks.shift()?.()
        } catch (error) {
          failures.push(error)
        }
      }
    } finally {
      this.flushingCallbacks = false
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Web ingress callbacks failed')
  }
}
