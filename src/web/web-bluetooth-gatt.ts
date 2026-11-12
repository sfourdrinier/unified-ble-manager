// src/web/web-bluetooth-gatt.ts

import { trustedWebRecordingError, webHostErrorEvidence, webRecordingContractError } from './web-recording-errors'

import type { BackendConnection, BackendSubscription, GattBackend } from '../backend-contract/backend'
import { BackendContractError, contractError } from '../backend-contract/errors'
import type { CleanupFailure, CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { AttachmentRecord } from '../backend-contract/identity'
import type { TerminalEvidenceSource } from '../backend-contract/terminal-evidence'
import {
  createGattCharacteristicProperties,
  createGattDescriptorProperties,
  type Characteristic,
  type CharacteristicPath,
  type Descriptor,
  type DescriptorPath,
  type GattDatabase,
  type NotificationValue,
  type Service
} from '../backend-contract/gatt'
import { createBackendOperationDispatch, createOperationSettlementCoordinator } from '../backend-contract/operations'
import type {
  BackendOperationDispatch,
  CancellationAcknowledgement,
  CharacteristicRead,
  CharacteristicReadResult,
  OperationOptions,
  OperationTerminalRecord,
  PublicOperationOptions,
  ReadProvenance,
  ReadRequest,
  ReadResult,
  SubscribeRequest,
  SubscriptionOptions,
  WritePolicy,
  WriteReceipt,
  WriteRequest,
  WriteResult
} from '../backend-contract/operations'
import { canonicalUuid, opaqueId, ownBytes } from '../backend-contract/primitives'
import type { AttachmentBoundIdFactory, ByteLimit, Deadline, OwnedBytes } from '../backend-contract/primitives'
import { byteLimit } from '../backend-contract/primitives'
import { CoreBoundedStream } from '../core/bounded-stream'
import { TerminalEvidenceFence } from '../core/terminal-evidence-fence'
import { accountOrderedDiscard, CoreOrderedNotificationStream } from '../core/ordered-notification-stream'
import { assertWebDeliveryMode } from './web-delivery-mode'
import type {
  OrderedNotificationCleanup,
  OrderedNotificationItem,
  OrderedNotificationLoss,
  OrderedNotificationStream,
  OrderedNotificationTerminal,
  OrderedNotificationValue
} from '../core/ordered-notification-contract'
import type {
  WebBluetoothCharacteristicBoundary,
  WebBluetoothDescriptorBoundary,
  WebBluetoothServiceBoundary
} from './web-bluetooth-boundary'
import { orderedSetupTerminalError, webCleanupFailure } from './web-bluetooth-errors'
import { WebNotificationIngress } from './web-notification-ingress'
import type { WebIngressGroup } from './web-notification-ingress'
import { WebOrderedMarkerTransactions } from './web-ordered-marker'
import type { WebMarkerReceipt } from './web-ordered-marker'
import type { WebRecordingMemberLease } from './web-ordered-recording-supervisor'
import { characteristicKey, descriptorKey, WebBackendSubscription, WebGattDatabase } from './web-bluetooth-handles'
import type {
  WebConnectionRecord,
  WebGattDatabaseHost,
  WebLinkEnd,
  WebManagedSubscription
} from './web-bluetooth-handles'

const notificationAdmissionFailures = new WeakMap<object, BackendContractError>()
export function webNotificationAdmissionFailure(error: unknown): BackendContractError | undefined {
  return typeof error === 'object' && error !== null ? notificationAdmissionFailures.get(error) : undefined
}
function notificationAdmissionError(code: 'operation.aborted' | 'operation.timed-out'): BackendContractError {
  const error = webRecordingContractError(code, 'gatt', 'web-gatt.subscribe')
  notificationAdmissionFailures.set(error, error)
  return error
}

/** Preserve source controls that were already ordered behind a consumer overflow. */
class WebOrderedNotificationStream extends CoreOrderedNotificationStream {
  private readonly lateItems: Exclude<OrderedNotificationItem, OrderedNotificationTerminal>[] = []
  private readonly discardedLateItems: Exclude<OrderedNotificationItem, OrderedNotificationTerminal>[] = []
  private lateBytes = 0
  private lateOmission = false
  private heldTerminal: OrderedNotificationTerminal | null = null
  private discardedTerminal: OrderedNotificationTerminal | null = null
  private terminalPublished = false
  private terminalReason: OrderedNotificationTerminal['reason'] | null = null
  private finishing = false
  private nestedTerminal: OrderedNotificationTerminal | null = null
  private consumerOverflow = false
  private discardStarted = false
  private nextTail: Promise<void> = Promise.resolve()
  prepareSourceFinish: ((reason: OrderedNotificationTerminal['reason']) => void) | null = null
  reportSourceFinishFailure: (() => void) | null = null

  override finish(terminal: OrderedNotificationTerminal): void {
    if (this.finishing) {
      this.nestedTerminal ??= terminal
      return
    }
    if (this.terminalPublished) {
      super.finish(terminal)
      return
    }
    this.finishing = true
    try {
      this.prepareSourceFinish?.(terminal.reason)
      terminal = this.nestedTerminal ?? terminal
      this.terminalReason = terminal.reason
      this.terminalPublished = true
      super.finish(terminal)
      this.reportSourceFinishFailure?.()
    } finally {
      this.finishing = false
    }
  }

  emitSourceValue(item: OrderedNotificationValue): boolean {
    if (this.terminalPublished || this.consumerOverflow) {
      if (!this.consumerOverflow && this.terminalReason !== 'overflow') {
        this.queueLate(item)
        return true
      }
      this.queueLate({
        kind: 'loss',
        scope: { kind: 'member', memberId: item.memberId },
        reason: 'overflow',
        lostValues: '1',
        lostBytes: String(item.value.byteLength),
        lostSourceRecords: null
      })
      return false
    }
    const accepted = this.emit(item)
    if (!accepted) this.consumerOverflow = true
    return accepted
  }

  emitSourceLoss(loss: OrderedNotificationLoss): void {
    if (this.terminalPublished || this.consumerOverflow) this.queueLate(loss)
    else this.emit(loss)
  }

  private queueLate(item: OrderedNotificationValue | OrderedNotificationLoss): void {
    if (this.lateOmission) return
    const bytes = item.kind === 'value' ? item.value.byteLength : 0
    if (
      this.lateItems.length >= this.limits.itemCapacity ||
      this.lateBytes + bytes > this.limits.byteCapacity - this.limits.reservedControlCapacity
    ) {
      this.lateOmission = true
      this.lateItems.push({ kind: 'failure-observations-omitted', count: null })
      return
    }
    this.lateBytes += bytes
    this.lateItems.push(item)
  }

  private shiftLate(): Exclude<OrderedNotificationItem, OrderedNotificationTerminal> | undefined {
    const item = this.lateItems.shift()
    if (item?.kind === 'value') this.lateBytes -= item.value.byteLength
    if (item?.kind === 'failure-observations-omitted') this.lateOmission = false
    return item
  }

  override [Symbol.asyncIterator](): AsyncIterator<OrderedNotificationItem, undefined, undefined> {
    const inner = super[Symbol.asyncIterator]()
    return {
      next: () => {
        const result = this.nextTail.then(async (): Promise<IteratorResult<OrderedNotificationItem, undefined>> => {
          if (this.discardStarted) return { done: true, value: undefined }
          if (this.heldTerminal !== null) {
            const item = this.shiftLate()
            if (item !== undefined) return { done: false, value: item }
            const terminal = this.heldTerminal
            this.heldTerminal = null
            return { done: false, value: terminal }
          }
          const innerResult = await inner.next()
          if (!innerResult.done && innerResult.value.kind === 'terminal' && this.lateItems.length > 0) {
            this.heldTerminal = innerResult.value
            const item = this.shiftLate()
            if (item !== undefined) return { done: false, value: item }
          }
          return innerResult
        })
        this.nextTail = result.then(
          () => undefined,
          () => undefined
        )
        return result
      },
      return: async () => {
        await this.discard()
        return { done: true, value: undefined }
      }
    }
  }

  override async discard(): Promise<OrderedNotificationCleanup> {
    this.discardStarted = true
    const result = await super.discard()
    if (result.group.state !== 'released' || result.members.some(member => member.cleanup.state !== 'released'))
      return result
    this.discardedLateItems.push(...this.lateItems.splice(0))
    this.lateBytes = 0
    if (this.heldTerminal !== null) {
      this.discardedTerminal = this.heldTerminal
      this.heldTerminal = null
    }
    const discarded = result.discarded
    if (discarded === undefined) return result
    const late = accountOrderedDiscard(this.discardedLateItems)
    return {
      ...result,
      discarded: {
        ...discarded,
        values: (BigInt(discarded.values) + BigInt(late.values)).toString(),
        bytes: (BigInt(discarded.bytes) + BigInt(late.bytes)).toString(),
        controls: (
          BigInt(discarded.controls) +
          BigInt(late.controls) +
          (this.discardedTerminal === null ? 0n : 1n)
        ).toString(),
        losses: [...(discarded.losses ?? []), ...(late.losses ?? [])],
        failureObservationsOmitted: [
          ...(discarded.failureObservationsOmitted ?? []),
          ...(late.failureObservationsOmitted ?? [])
        ],
        terminal: this.discardedTerminal ?? discarded.terminal
      }
    }
  }
}

export interface WebGattHost extends WebGattDatabaseHost {
  readonly attachment: AttachmentRecord<string>
  notificationNow(): number
  identifiers(): AttachmentBoundIdFactory<string>
  requireConnection(connection: BackendConnection<string, string>, operation: string): WebConnectionRecord
  requireDatabase(
    path:
      | CharacteristicPath<string, string, string, string, string>
      | DescriptorPath<string, string, string, string, string, string>,
    operation: string
  ): WebGattDatabase
  runAbortable<Result>(
    record: WebConnectionRecord | null,
    operation: { readonly signal: AbortSignal | null; readonly deadline: Deadline | null },
    start: () => Promise<Result>,
    fallbackCode: import('../backend-contract/errors').BleErrorCode,
    domain: 'chooser' | 'connection' | 'gatt',
    operationName: string,
    onLateSuccess?: ((result: Result) => Promise<void> | void) | null
  ): Promise<Result>
  staleGattError(operation: string): import('../backend-contract/errors').BackendContractError
}

export interface WebOrderedMember {
  readonly memberId: string
  readonly path: CharacteristicPath<string, string, string, string, string, 'current'>
  readonly deliveryMode: NonNullable<SubscriptionOptions['deliveryMode']>
}

/** Internal adapter port. The caller installs its retained ingress before enabling. */
export interface WebRecordingMemberReservation {
  readonly id: string
  readonly characteristic: WebBluetoothCharacteristicBoundary
  readonly lease: WebRecordingMemberLease
  isCurrent(): boolean
}

export class WebRecordingMemberCleanupError extends Error {
  constructor(readonly cleanup: CleanupRecord) {
    super('Web recording member cleanup failed')
    this.name = 'WebRecordingMemberCleanupError'
  }
}

export interface WebOrderedOwner {
  readonly ingress: WebIngressGroup
  readonly stream: OrderedNotificationStream
  readonly members: readonly (WebOrderedMember & {
    readonly effectiveDelivery: 'notification' | 'indication' | 'unknown'
  })[]
  markerWrite(
    id: string,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    value: Readonly<Uint8Array>,
    mode: 'with-response' | 'without-response',
    signal: AbortSignal | null,
    deadlineExpired?: boolean
  ): WebMarkerReceipt
  hasMarkerReceipt(id: string): boolean
  acknowledgeMarker(id: string): void
  forgetMarker(id: string): boolean
  close(): Promise<readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]>
}

export type WebOrderedSetup =
  | { readonly state: 'ready'; readonly owner: WebOrderedOwner }
  | {
      readonly state: 'failed'
      readonly error: NormalizedBleError
      readonly owner: WebOrderedOwner
      readonly cleanup: readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]
      readonly acquisitions: readonly {
        readonly memberId: string
        readonly state: 'not-started' | 'acquired' | 'indeterminate'
      }[]
    }

interface WebPhysicalNotifications {
  readonly characteristic: WebBluetoothCharacteristicBoundary
  readonly members: Set<WebManagedSubscription>
  readonly listener: (value: Uint8Array) => void
  readonly start: Promise<void>
  listening: boolean
  stop: Promise<CleanupRecord> | null
}

const RELEASED: CleanupRecord = { state: 'released', failures: [] }
const MAXIMUM_VALUE_BYTES: ByteLimit = byteLimit(512 * 1024)

/**
 * Web Bluetooth's `readValue()` resolves with the value of this read's own
 * Read Characteristic Value procedure. The same read also fires a
 * `characteristicvaluechanged` event, which is not attributed by the browser.
 * The browser's own attribution on its host OS is not observable from the page.
 */
const WEB_BLUETOOTH_READ_PROVENANCE: ReadProvenance = 'read-response'

export class WebBluetoothGattRuntime {
  readonly gatt: GattBackend<string>
  private nextDatabase = 1
  private nextSubscription = 1
  private nextOrderedCollection = 1
  private nextOperation = 1
  private readonly ingressByDatabase = new WeakMap<WebGattDatabase, WebNotificationIngress>()
  private readonly ingressMemberships = new Map<WebBluetoothCharacteristicBoundary, Map<WebGattDatabase, Set<object>>>()
  private readonly removingIngressMemberships = new Map<
    WebBluetoothCharacteristicBoundary,
    Map<WebGattDatabase, Set<object>>
  >()
  private readonly subscriptions = new Map<string, WebManagedSubscription>()
  private readonly physicalNotifications = new Map<WebBluetoothCharacteristicBoundary, WebPhysicalNotifications>()
  private readonly physicalBySubscription = new Map<WebManagedSubscription, WebPhysicalNotifications>()
  private readonly orderedBySubscription = new Map<WebManagedSubscription, WebIngressGroup>()
  private readonly recordingSubscriptions = new Set<WebManagedSubscription>()
  private readonly startAuthorizations = new WeakMap<WebManagedSubscription, () => void>()
  private readonly startAdmissionFailures = new WeakMap<WebManagedSubscription, BackendContractError>()

  constructor(
    private readonly host: WebGattHost,
    private readonly terminalEvidence?: TerminalEvidenceSource & {
      reservationForScope(scope: {
        readonly managerId: string
        readonly operationId: string
        readonly incarnation: string
      }): import('../backend-contract/terminal-evidence').TerminalEvidenceReservation | undefined
      commitTerminal(
        scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string },
        terminal: import('../backend-contract/streams').StreamTerminalNotice
      ): Promise<import('../backend-contract/terminal-evidence').TerminalEvidenceAppendResult>
    }
  ) {
    this.gatt = this.createGattBackend()
  }

  notificationIngress(database: WebGattDatabase): WebNotificationIngress {
    database.assertCurrent('web-gatt.notification-ingress')
    let ingress = this.ingressByDatabase.get(database)
    if (ingress === undefined) {
      const update = (
        routes: Map<WebBluetoothCharacteristicBoundary, Map<WebGattDatabase, Set<object>>>,
        characteristic: WebBluetoothCharacteristicBoundary,
        registration: object,
        added: boolean
      ): void => {
        let databases = routes.get(characteristic)
        if (added) {
          if (databases === undefined) {
            databases = new Map()
            routes.set(characteristic, databases)
          }
          let registrations = databases.get(database)
          if (registrations === undefined) {
            registrations = new Set()
            databases.set(database, registrations)
          }
          registrations.add(registration)
        } else if (databases !== undefined) {
          const registrations = databases.get(database)
          registrations?.delete(registration)
          if (registrations?.size === 0) databases.delete(database)
          if (databases.size === 0) routes.delete(characteristic)
        }
      }
      ingress = new WebNotificationIngress(
        (characteristic, added, registration) => update(this.ingressMemberships, characteristic, registration, added),
        true,
        (characteristic, registration) => update(this.removingIngressMemberships, characteristic, registration, true),
        (characteristic, registration) => update(this.removingIngressMemberships, characteristic, registration, false)
      )
      this.ingressByDatabase.set(database, ingress)
    }
    return ingress
  }

  subscriptionCount(): number {
    return this.subscriptions.size
  }

  physicalNotificationCount(): number {
    return this.physicalNotifications.size
  }

  retainedSubscriptionCount(): number {
    return [...this.subscriptions.values()].filter(subscription => subscription.stream.retainedPayloadBytes() > 0)
      .length
  }

  hasTerminalEvidenceSubscription(record: WebConnectionRecord): boolean {
    return (
      this.terminalEvidence !== undefined &&
      [...this.subscriptions.values()].some(
        subscription => subscription.database.record === record && subscription.terminalEvidenceScope !== undefined
      )
    )
  }

  async destroySubscriptions(): Promise<readonly CleanupFailure[]> {
    const failures: CleanupFailure[] = []
    for (const subscription of [...this.subscriptions.values()]) {
      if (subscription.state === 'cleanup-failed' && !subscription.cleanupFailureReported) {
        subscription.cleanupFailureReported = true
        failures.push(...webCleanupFailure('subscription', 'web-gatt.retained-cleanup-failure').failures)
        continue
      }
      const cleanup = await this.removeManagedSubscription(subscription)
      failures.push(...cleanup.failures)
    }
    return failures
  }

  async invalidateConnection(record: WebConnectionRecord, end: WebLinkEnd): Promise<boolean> {
    this.fenceConnection(record, end)
    // An adapter loss failed the streams at their source, as on every host.
    const reason = end === 'adapter-loss' ? 'source-failed' : end
    let settled = true
    for (const subscription of [...this.subscriptions.values()]) {
      if (subscription.database.record === record) {
        // Starting a logical terminal must not remove this managed
        // subscription. The connection teardown that follows still owns the
        // browser `stopNotifications()` call and the corresponding cleanup
        // diagnostic. Removing it here bypasses both and leaks the record.
        if (!(await this.beginLogicalSubscriptionStop(subscription, reason))) settled = false
      }
    }
    return settled
  }

  /** Explicit recovery retries only terminals whose original durable commit was unresolved. */
  async retryPendingTerminalEvidence(record: WebConnectionRecord, end: WebLinkEnd): Promise<boolean> {
    const reason = end === 'adapter-loss' ? 'source-failed' : end
    let settled = true
    for (const subscription of [...this.subscriptions.values()]) {
      if (subscription.database.record !== record || subscription.state !== 'stopping') continue
      const scope = subscription.terminalEvidenceScope
      if (scope === undefined || this.terminalEvidence === undefined) continue
      const terminal = subscription.stream.previewTerminal(reason)
      if (terminal === null) continue
      const receipt = await this.terminalEvidence.commitTerminal(scope, terminal)
      if ('kind' in receipt) {
        settled = false
        continue
      }
      subscription.stream.closeWithReason(reason, null, receipt)
      this.finishStoppedSubscription(subscription)
    }
    return settled
  }

  fenceConnection(record: WebConnectionRecord, end: WebLinkEnd): void {
    if (record.database !== null) {
      try {
        this.ingressByDatabase.get(record.database)?.retire(end === 'adapter-loss' ? 'source-failed' : end)
      } catch (error) {
        console.error(
          '[WebBluetoothGattRuntime.fenceConnection] Ingress callback failed:',
          webHostErrorEvidence([error])
        )
      }
    }
  }

  async stopConnectionSubscriptions(record: WebConnectionRecord): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const subscription of [...this.subscriptions.values()]) {
      if (subscription.database.record === record) {
        const cleanup = await this.removeManagedSubscription(subscription)
        failures.push(...cleanup.failures)
      }
    }
    return failures.length === 0 ? RELEASED : { state: 'release-failed', failures }
  }

  async readDirect(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<CharacteristicRead> {
    return this.readCharacteristic(database, path, options)
  }

  async writeDirect(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    value: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<WriteReceipt<string, string>> {
    await this.writeCharacteristic(database, path, value, options)
    return terminalWriteReceipt(this.operationCorrelation('direct-write'), 'confirmed')
  }

  async readDescriptorDirect(
    database: WebGattDatabase,
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<OwnedBytes> {
    return this.readDescriptorValue(database, path, options)
  }

  async writeDescriptorDirect(
    database: WebGattDatabase,
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    value: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<WriteReceipt<string, string>> {
    await this.writeDescriptorValue(database, path, value, options)
    return terminalWriteReceipt(this.operationCorrelation('direct-descriptor-write'), 'confirmed')
  }

  async subscribeDirect(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: SubscriptionOptions
  ): Promise<import('../backend-contract/gatt').Subscription<string, string, string, string, string, string>> {
    const managed = await this.enableSubscription(
      database,
      path,
      options,
      this.operationCorrelation('direct-subscribe')
    )
    return {
      subscriptionId: managed.subscriptionId,
      path: managed.path,
      values: managed.stream,
      remove: () => this.removeManagedSubscription(managed)
    }
  }

  /**
   * Reserve without starting notifications. The caller registers this exact
   * characteristic with retained ingress, then reports that installation via
   * isInstalled. Keep the registration live through enable and release.
   */
  reserveRecordingMember(
    connection: BackendConnection<string, string>,
    database: WebGattDatabase,
    member: WebOrderedMember,
    options: SubscriptionOptions,
    lifecycle: { readonly isInstalled: () => boolean }
  ): WebRecordingMemberReservation {
    const operation = 'web-gatt.recording-member'
    const validate = (): WebBluetoothCharacteristicBoundary => {
      if (this.host.requireConnection(connection, operation) !== database.record) {
        throw this.host.staleGattError(operation)
      }
      return this.requireCharacteristic(database, member.path, operation)
    }
    if (member.memberId.length === 0) throw contractError('argument.invalid', 'gatt', operation)
    const characteristic = validate()
    assertWebDeliveryMode(characteristic, member.deliveryMode, operation)
    const isCurrent = (): boolean => {
      try {
        return validate() === characteristic
      } catch {
        return false
      }
    }
    let managed: WebManagedSubscription | null = null
    let started = false
    let settled = false
    let releasing = false
    const lease: WebRecordingMemberLease = {
      id: member.memberId,
      enable: async () => {
        if (started || releasing || !lifecycle.isInstalled()) {
          throw webRecordingContractError('lifecycle.invalid-state', 'gatt', operation)
        }
        if (!isCurrent()) throw this.host.staleGattError(operation)
        started = true
        try {
          await this.enableSubscription(
            database,
            member.path,
            { ...options, deliveryMode: member.deliveryMode },
            this.operationCorrelation('recording-subscribe'),
            created => {
              managed = created
              this.recordingSubscriptions.add(created)
            },
            true,
            () => {
              if (releasing || !lifecycle.isInstalled()) {
                throw webRecordingContractError('lifecycle.invalid-state', 'gatt', operation)
              }
              if (!isCurrent()) throw this.host.staleGattError(operation)
            }
          )
        } finally {
          settled = true
        }
      },
      release: async () => {
        releasing = true
        if (managed === null) {
          if (started && !settled) {
            throw new WebRecordingMemberCleanupError(
              webCleanupFailure('subscription', 'web-gatt.recording-enable-pending')
            )
          }
          return
        }
        const cleanup = await this.removeManagedSubscription(managed)
        if (cleanup.state !== 'released') throw new WebRecordingMemberCleanupError(cleanup)
      }
    }
    return { id: member.memberId, characteristic, lease, isCurrent }
  }

  /** Internal acquisition boundary; callers must retain the owner even after failed setup. */
  async acquireOrdered(
    database: WebGattDatabase,
    members: readonly WebOrderedMember[],
    options: SubscriptionOptions
  ): Promise<WebOrderedSetup> {
    const boundaries = new Map<WebBluetoothCharacteristicBoundary, string>()
    const resolved: {
      readonly characteristic: WebBluetoothCharacteristicBoundary
      readonly member: WebOrderedMember
    }[] = []
    for (const member of members) {
      const characteristic = this.requireCharacteristic(database, member.path, 'web-gatt.ordered-subscribe')
      if (boundaries.has(characteristic) || [...boundaries.values()].includes(member.memberId)) {
        throw contractError('argument.invalid', 'gatt', 'web-gatt.ordered-duplicate-member')
      }
      boundaries.set(characteristic, member.memberId)
      resolved.push({ characteristic, member })
    }
    if (boundaries.size === 0) throw contractError('argument.invalid', 'gatt', 'web-gatt.ordered-empty')
    let deliveryFailure: BackendContractError | null = null
    for (const { characteristic, member } of resolved) {
      try {
        assertWebDeliveryMode(characteristic, member.deliveryMode, 'web-gatt.subscribe')
      } catch (error) {
        if (!(error instanceof BackendContractError)) throw error
        deliveryFailure ??= error
      }
    }
    const collectionId = `web-ordered-${this.nextOrderedCollection}`
    this.nextOrderedCollection += 1
    if (deliveryFailure !== null) {
      const stream = new WebOrderedNotificationStream(
        options.delivery,
        () => undefined,
        async () => ({ group: RELEASED, members: [] }),
        collectionId
      )
      stream.finish({ kind: 'terminal', reason: 'source-failed', uncertainTail: true })
      const unavailable = (): never => {
        throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.ordered-subscribe')
      }
      const owner: WebOrderedOwner = {
        get ingress() {
          return unavailable()
        },
        stream,
        members: members.map(member => ({ ...member, effectiveDelivery: 'unknown' })),
        markerWrite: unavailable,
        hasMarkerReceipt: () => false,
        acknowledgeMarker: unavailable,
        forgetMarker: () => false,
        close: async () => []
      }
      return {
        state: 'failed',
        error: deliveryFailure.normalized,
        owner,
        cleanup: [],
        acquisitions: members.map(member => ({ memberId: member.memberId, state: 'not-started' }))
      }
    }
    let preparationFailure: { readonly error: unknown } | null = null
    const stream = new WebOrderedNotificationStream(
      options.delivery,
      reason => {
        ingress.retire(reason)
        if (preparationFailure !== null) {
          const { error } = preparationFailure
          preparationFailure = null
          throw error
        }
      },
      async () => ({ group: RELEASED, members: await owner.close() }),
      collectionId
    )
    const ingress = this.notificationIngress(database).register(
      boundaries,
      options.delivery.itemCapacity,
      options.delivery.byteCapacity,
      (memberId, ordinal, value) =>
        stream.emitSourceValue({
          kind: 'value',
          collectionId,
          connectionGeneration: String(database.path.connectionGeneration),
          databaseGeneration: String(database.path.databaseGeneration),
          sourceGeneration: null,
          sourceMembership: null,
          memberId,
          ordinal,
          value: ownBytes(value, byteLimit(Number.MAX_SAFE_INTEGER)),
          delivery: 'unknown',
          provenance: 'read-or-notification'
        })
    )
    stream.prepareSourceFinish = reason => {
      try {
        ingress.retire(reason)
      } catch (error) {
        preparationFailure = { error }
      }
    }
    stream.reportSourceFinishFailure = () => {
      if (preparationFailure !== null) {
        const { error } = preparationFailure
        preparationFailure = null
        throw error
      }
    }
    const markers = new WebOrderedMarkerTransactions(
      ingress,
      stream,
      collectionId,
      String(database.path.connectionGeneration),
      String(database.path.databaseGeneration)
    )
    let markerTarget: {
      readonly id: string
      readonly key: string
      readonly target: WebBluetoothCharacteristicBoundary
    } | null = null
    const owned: { readonly memberId: string; readonly managed: WebManagedSubscription }[] = []
    const acquisitions: { readonly memberId: string; state: 'not-started' | 'acquired' | 'indeterminate' }[] =
      members.map(member => ({ memberId: member.memberId, state: 'not-started' }))
    let closing: Promise<readonly { readonly memberId: string; readonly cleanup: CleanupRecord }[]> | null = null
    let ready = false
    const owner: WebOrderedOwner = {
      ingress,
      stream,
      members: members.map(member => ({ ...member, effectiveDelivery: 'unknown' })),
      markerWrite: (id, path, value, mode, signal, deadlineExpired) => {
        const key = JSON.stringify([
          path.attachment,
          path.attachmentId,
          path.peerId,
          path.ownerLeaseId,
          path.connectionId,
          path.connectionGeneration,
          path.databaseId,
          path.databaseGeneration,
          path.serviceUuid,
          path.serviceOccurrence,
          path.characteristicUuid,
          path.characteristicOccurrence,
          path.validity
        ])
        if (markerTarget !== null && (markerTarget.id !== id || markerTarget.key !== key))
          throw contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.marker-retry')
        const target = markerTarget?.target ?? this.requireCharacteristic(database, path, 'web-gatt.marker-write')
        if (
          (mode === 'with-response' && !target.properties.write) ||
          (mode === 'without-response' && !target.properties.writeWithoutResponse)
        )
          throw contractError('gatt.property-not-supported', 'gatt', 'web-gatt.marker-write')
        const copied = ownBytes(value, MAXIMUM_VALUE_BYTES)
        try {
          return markers.write(id, target, copied, mode, signal, deadlineExpired)
        } finally {
          if (markers.hasReceipt(id)) markerTarget = { id, key, target }
        }
      },
      hasMarkerReceipt: id => markers.hasReceipt(id),
      acknowledgeMarker: id => markers.acknowledge(id),
      forgetMarker: id => {
        const forgotten = markers.forget(id)
        if (forgotten && markerTarget?.id === id) markerTarget = null
        return forgotten
      },
      close: () => {
        ingress.retire('closed')
        if (closing === null) {
          const cleanup = async () => {
            const results: { memberId: string; cleanup: CleanupRecord }[] = []
            for (const entry of owned) {
              results.push({ memberId: entry.memberId, cleanup: await this.removeManagedSubscription(entry.managed) })
            }
            return results
          }
          closing = cleanup().then(
            results => {
              if (results.some(result => result.cleanup.state === 'release-failed')) closing = null
              return results
            },
            error => {
              closing = null
              throw error
            }
          )
        }
        return closing
      }
    }
    ingress.onTerminal(() => {
      for (const record of ingress.drain()) {
        if (record.kind === 'loss')
          stream.emitSourceLoss({
            kind: 'loss',
            scope: { kind: 'group' },
            reason:
              record.reason === 'source-failed' || ingress.terminalReason === 'ordinal-exhausted'
                ? 'source-loss'
                : 'overflow',
            lostValues: String(record.lostValues),
            lostBytes: String(record.lostBytes),
            lostSourceRecords: null
          })
        else if (record.kind === 'terminal')
          stream.finish({ kind: 'terminal', reason: record.reason, uncertainTail: record.uncertainTail })
      }
      if (ready) {
        owner.close().catch(error => {
          console.error(
            '[WebBluetoothGattRuntime.acquireOrdered] Automatic cleanup rejected:',
            webHostErrorEvidence([error])
          )
        })
      }
    })
    try {
      for (const [index, member] of members.entries()) {
        const acquisition = acquisitions[index]
        if (acquisition === undefined)
          throw contractError('lifecycle.invariant-violation', 'gatt', 'web-gatt.ordered-member')
        await this.enableSubscription(
          database,
          member.path,
          { ...options, deliveryMode: member.deliveryMode },
          this.operationCorrelation('ordered-subscribe'),
          managed => {
            acquisition.state = 'indeterminate'
            owned.push({ memberId: member.memberId, managed })
            this.orderedBySubscription.set(managed, ingress)
          },
          true
        )
        acquisition.state = 'acquired'
        if (ingress.terminalReason !== null) throw orderedSetupTerminalError(ingress.terminalReason)
      }
      ready = true
      return { state: 'ready', owner }
    } catch (error) {
      const terminalReason = ingress.terminalReason
      ingress.retire(
        error instanceof BackendContractError && error.normalized.code === 'operation.aborted'
          ? 'operation-aborted'
          : error instanceof BackendContractError && error.normalized.code === 'operation.timed-out'
            ? 'operation-timed-out'
            : 'source-failed'
      )
      const cleanup = await owner.close()
      const normalized =
        terminalReason !== null
          ? orderedSetupTerminalError(terminalReason).normalized
          : error instanceof BackendContractError
            ? error.normalized
            : contractError('gatt.subscribe-failed', 'gatt', 'web-gatt.ordered-subscribe').normalized
      return { state: 'failed', error: normalized, owner, acquisitions, cleanup }
    }
  }

  private createGattBackend(): GattBackend<string> {
    return {
      ...(this.terminalEvidence === undefined
        ? {}
        : {
            reserveTerminalEvidence: this.terminalEvidence.reserveTerminalEvidence,
            readTerminalEvidence: this.terminalEvidence.readTerminalEvidence,
            releaseTerminalEvidence: this.terminalEvidence.releaseTerminalEvidence
          }),
      discover: async (connection, options) => this.discover(connection, options),
      read: (path, request) => this.dispatch(request.operation, operation => this.readResult(path, { operation })),
      write: (path, request) =>
        this.dispatch(request.operation, operation => this.writeResult(path, { ...request, operation })),
      readDescriptor: (path, request) =>
        this.dispatch(request.operation, operation => this.readDescriptorResult(path, { operation })),
      writeDescriptor: (path, request) =>
        this.dispatch(request.operation, operation => this.writeDescriptorResult(path, { ...request, operation })),
      subscribe: (path, request) =>
        this.dispatch(request.operation, operation => this.subscribeResult(path, { ...request, operation })),
      unsubscribe: (subscription, operation) =>
        this.dispatch(operation, dispatched => this.unsubscribeResult(subscription, dispatched))
    }
  }

  private async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    const record = this.host.requireConnection(connection, 'web-gatt.discover')
    if (record.database !== null) {
      const cleanup = await this.stopDatabaseSubscriptions(record.database)
      if (cleanup.state === 'release-failed') {
        throw contractError('gatt.subscribe-failed', 'gatt', 'web-gatt.rediscovery-subscription-cleanup')
      }
      try {
        this.ingressByDatabase.get(record.database)?.retire('service-changed')
      } catch (error) {
        console.error('[WebBluetoothGattRuntime.discover] Ingress callback failed:', webHostErrorEvidence([error]))
      }
      record.database.invalidate()
      record.database = null
    }
    const nativeServices = await this.host.runAbortable(
      record,
      options,
      () => record.device.gatt.getPrimaryServices(),
      'gatt.not-found',
      'gatt',
      'web-gatt.discover'
    )
    const database = await this.buildDatabase(record, nativeServices, options)
    record.database = database
    return database
  }

  private async buildDatabase(
    record: WebConnectionRecord,
    rootServices: readonly WebBluetoothServiceBoundary[],
    options: PublicOperationOptions
  ): Promise<WebGattDatabase> {
    const maximumServices = 4096
    const maximumInclusionEdges = 65536
    if (rootServices.length > maximumServices) throw contractError('stream.quota', 'gatt', 'web-gatt.service-capacity')
    let inclusionEdges = 0
    const nativeServices = [...rootServices]
    const known = new Set(rootServices)
    if (known.size !== rootServices.length)
      throw contractError('protocol.violation', 'gatt', 'web-gatt.duplicate-service-object')
    const inclusions = new Map<WebBluetoothServiceBoundary, readonly WebBluetoothServiceBoundary[] | null>()
    for (let index = 0; index < nativeServices.length; index += 1) {
      const service = nativeServices[index]
      if (service === undefined) throw contractError('protocol.malformed', 'gatt', 'web-gatt.discovery-service')
      const included =
        service.getIncludedServices === undefined
          ? null
          : await this.host.runAbortable(
              record,
              options,
              async () => {
                try {
                  return (await service.getIncludedServices?.()) ?? null
                } catch (error) {
                  // GetGATTChildren rejects an empty result with NotFoundError.
                  // Handle only that native absence, before generic normalization.
                  if (typeof error === 'object' && error !== null && 'name' in error && error.name === 'NotFoundError')
                    return []
                  throw error
                }
              },
              'platform.failure',
              'gatt',
              'web-gatt.discover-included-services'
            )
      inclusionEdges += included?.length ?? 0
      if (inclusionEdges > maximumInclusionEdges)
        throw contractError('stream.quota', 'gatt', 'web-gatt.inclusion-capacity')
      inclusions.set(service, included)
      for (const target of included ?? []) {
        if (!known.has(target)) {
          if (known.size >= maximumServices) throw contractError('stream.quota', 'gatt', 'web-gatt.service-capacity')
          known.add(target)
          nativeServices.push(target)
        }
      }
    }
    const sourceIdentities = new Map<
      WebBluetoothServiceBoundary,
      { readonly uuid: ReturnType<typeof canonicalUuid>; readonly occurrence: string }
    >()
    const sourceCounts = new Map<string, number>()
    for (const service of nativeServices) {
      const uuid = canonicalUuid(service.uuid)
      const occurrence = sourceCounts.get(uuid) ?? 0
      sourceCounts.set(uuid, occurrence + 1)
      sourceIdentities.set(service, { uuid, occurrence: String(occurrence) })
    }
    const databaseNumber = this.nextDatabase
    this.nextDatabase += 1
    const attachment = this.host.attachment
    const databasePath = {
      attachment,
      attachmentId: attachment.attachmentId,
      peerId: record.peerId,
      connectionId: record.connection.connectionId,
      ownerLeaseId: record.leaseId,
      connectionGeneration: record.connection.connectionGeneration,
      databaseId: this.host.identifiers().databaseId(`web-database-${databaseNumber}`),
      databaseGeneration: opaqueId(
        `web-database-generation-${databaseNumber}`,
        'database-generation',
        `web-bluetooth:${String(record.connection.connectionId)}`
      )
    }
    const services: Service<string, string, string, string>[] = []
    const characteristics: Characteristic<string, string, string, string, string>[] = []
    const descriptors: Descriptor<string, string, string, string, string, string>[] = []
    const characteristicBoundaries = new Map<string, WebBluetoothCharacteristicBoundary>()
    const descriptorBoundaries = new Map<string, WebBluetoothDescriptorBoundary>()
    const serviceOccurrences = new Map<string, number>()
    for (let serviceIndex = 0; serviceIndex < nativeServices.length; serviceIndex += 1) {
      const nativeService = nativeServices[serviceIndex]
      if (nativeService === undefined) {
        throw contractError('protocol.malformed', 'gatt', 'web-gatt.discovery-service')
      }
      const serviceUuid = canonicalUuid(nativeService.uuid)
      const serviceOccurrence = serviceOccurrences.get(String(serviceUuid)) ?? 0
      serviceOccurrences.set(String(serviceUuid), serviceOccurrence + 1)
      const servicePath = {
        ...databasePath,
        serviceUuid,
        serviceOccurrence: opaqueId(
          String(serviceOccurrence),
          'service-occurrence',
          `${String(databasePath.databaseId)}:${String(serviceUuid)}`
        )
      }
      const included = inclusions.get(nativeService)
      services.push({
        path: servicePath,
        primary: nativeService.primary ?? (rootServices.includes(nativeService) ? true : null),
        includedServices:
          included == null
            ? null
            : Object.freeze(
                included.map(target => {
                  const identity = sourceIdentities.get(target)
                  if (identity === undefined)
                    throw contractError('protocol.violation', 'gatt', 'web-gatt.included-service-unresolved')
                  return Object.freeze({
                    uuid: identity.uuid,
                    occurrence: opaqueId(identity.occurrence, 'service-occurrence', String(databasePath.databaseId))
                  })
                })
              )
      })
      const nativeCharacteristics = await this.host.runAbortable(
        record,
        options,
        () => nativeService.getCharacteristics(),
        'gatt.not-found',
        'gatt',
        'web-gatt.discover-characteristics'
      )
      const characteristicOccurrences = new Map<string, number>()
      for (let characteristicIndex = 0; characteristicIndex < nativeCharacteristics.length; characteristicIndex += 1) {
        const nativeCharacteristic = nativeCharacteristics[characteristicIndex]
        if (nativeCharacteristic === undefined) {
          throw contractError('protocol.malformed', 'gatt', 'web-gatt.discovery-characteristic')
        }
        const characteristicUuid = canonicalUuid(nativeCharacteristic.uuid)
        const characteristicOccurrence = characteristicOccurrences.get(String(characteristicUuid)) ?? 0
        characteristicOccurrences.set(String(characteristicUuid), characteristicOccurrence + 1)
        const path: CharacteristicPath<string, string, string, string, string, 'current'> = {
          ...servicePath,
          characteristicUuid,
          characteristicOccurrence: opaqueId(
            String(characteristicOccurrence),
            'characteristic-occurrence',
            `${String(servicePath.serviceOccurrence)}:${String(characteristicUuid)}`
          ),
          validity: 'current'
        }
        characteristics.push(
          Object.freeze({
            path: Object.freeze(path),
            properties: createGattCharacteristicProperties({
              read: nativeCharacteristic.properties.read,
              writeWithResponse: nativeCharacteristic.properties.write,
              writeWithoutResponse: nativeCharacteristic.properties.writeWithoutResponse,
              notify: nativeCharacteristic.properties.notify,
              indicate: nativeCharacteristic.properties.indicate
            }),
            access: Object.freeze({ read: 'unknown', write: 'unknown' })
          })
        )
        characteristicBoundaries.set(characteristicKey(path), nativeCharacteristic)
        // Web Bluetooth's getDescriptors() rejects with NotFoundError when
        // the characteristic has no descriptors (finding 188, Polar H10 on
        // Chrome): that is an empty descriptor list, never a discovery
        // failure. Any other error stays an error.
        let nativeDescriptors: readonly WebBluetoothDescriptorBoundary[]
        try {
          nativeDescriptors = await this.host.runAbortable(
            record,
            options,
            () => nativeCharacteristic.getDescriptors(),
            'gatt.not-found',
            'gatt',
            'web-gatt.discover-descriptors'
          )
        } catch (error) {
          if (error instanceof BackendContractError && error.normalized.code === 'gatt.not-found') {
            nativeDescriptors = []
          } else {
            throw error
          }
        }
        const descriptorOccurrences = new Map<string, number>()
        for (let descriptorIndex = 0; descriptorIndex < nativeDescriptors.length; descriptorIndex += 1) {
          const nativeDescriptor = nativeDescriptors[descriptorIndex]
          if (nativeDescriptor === undefined) {
            throw contractError('protocol.malformed', 'gatt', 'web-gatt.discovery-descriptor')
          }
          const descriptorUuid = canonicalUuid(nativeDescriptor.uuid)
          const descriptorOccurrence = descriptorOccurrences.get(String(descriptorUuid)) ?? 0
          descriptorOccurrences.set(String(descriptorUuid), descriptorOccurrence + 1)
          const descriptorPath = {
            ...path,
            descriptorUuid,
            descriptorOccurrence: opaqueId(
              String(descriptorOccurrence),
              'descriptor-occurrence',
              `${String(path.characteristicOccurrence)}:${String(descriptorUuid)}`
            )
          }
          descriptors.push({
            path: descriptorPath,
            properties: createGattDescriptorProperties(
              true,
              true,
              { read: 'known', write: 'known' },
              { read: 'unknown', write: 'unknown' }
            )
          })
          descriptorBoundaries.set(descriptorKey(descriptorPath), nativeDescriptor)
        }
      }
    }
    return new WebGattDatabase(
      this.host,
      record,
      databasePath,
      services,
      characteristics,
      descriptors,
      characteristicBoundaries,
      descriptorBoundaries
    )
  }

  private async readCharacteristic(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<CharacteristicRead> {
    const characteristic = this.requireCharacteristic(database, path, 'web-gatt.read')
    if (!characteristic.properties.read) {
      throw contractError('gatt.property-not-supported', 'gatt', 'web-gatt.read')
    }
    const value = await this.host.runAbortable(
      database.record,
      options,
      () => characteristic.readValue(),
      'gatt.read-failed',
      'gatt',
      'web-gatt.read'
    )
    return Object.freeze({ value: ownBytes(value, MAXIMUM_VALUE_BYTES), provenance: WEB_BLUETOOTH_READ_PROVENANCE })
  }

  private async writeCharacteristic(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    value: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<void> {
    const characteristic = this.requireCharacteristic(database, path, 'web-gatt.write')
    const copied = ownBytes(value, MAXIMUM_VALUE_BYTES)
    if (options.mode === 'with-response') {
      if (!characteristic.properties.write) {
        throw contractError('gatt.property-not-supported', 'gatt', 'web-gatt.write-with-response')
      }
      await this.host.runAbortable(
        database.record,
        options,
        () => characteristic.writeValueWithResponse(new Uint8Array(copied)),
        'gatt.write-failed',
        'gatt',
        'web-gatt.write-with-response'
      )
      return
    }
    if (!characteristic.properties.writeWithoutResponse) {
      throw contractError('gatt.property-not-supported', 'gatt', 'web-gatt.write-without-response')
    }
    await this.host.runAbortable(
      database.record,
      options,
      () => characteristic.writeValueWithoutResponse(new Uint8Array(copied)),
      'gatt.write-failed',
      'gatt',
      'web-gatt.write-without-response'
    )
  }

  private async readDescriptorValue(
    database: WebGattDatabase,
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<OwnedBytes> {
    const descriptor = this.requireDescriptor(database, path, 'web-gatt.read-descriptor')
    const value = await this.host.runAbortable(
      database.record,
      options,
      () => descriptor.readValue(),
      'gatt.read-failed',
      'gatt',
      'web-gatt.read-descriptor'
    )
    return ownBytes(value, MAXIMUM_VALUE_BYTES)
  }

  private async writeDescriptorValue(
    database: WebGattDatabase,
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    value: Readonly<Uint8Array>,
    options: WritePolicy
  ): Promise<void> {
    const descriptor = this.requireDescriptor(database, path, 'web-gatt.write-descriptor')
    const copied = ownBytes(value, MAXIMUM_VALUE_BYTES)
    await this.host.runAbortable(
      database.record,
      options,
      () => descriptor.writeValue(new Uint8Array(copied)),
      'gatt.write-failed',
      'gatt',
      'web-gatt.write-descriptor'
    )
  }

  private async enableSubscription(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: SubscriptionOptions,
    correlation: OperationOptions<string, string>['correlation'],
    onCreated?: (managed: WebManagedSubscription) => void,
    orderedOnly = false,
    beforeCreated?: () => void,
    terminalEvidenceScope?: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
  ): Promise<WebManagedSubscription> {
    const characteristic = this.requireCharacteristic(database, path, 'web-gatt.subscribe')
    assertWebDeliveryMode(characteristic, options.deliveryMode, 'web-gatt.subscribe')
    let physical = this.physicalNotifications.get(characteristic)
    if (physical !== undefined && physical.stop !== null) {
      const pendingStop = physical.stop
      const cleanup = await this.host.runAbortable(
        database.record,
        options,
        () => pendingStop,
        'gatt.subscribe-failed',
        'gatt',
        'web-gatt.subscribe-pending-stop'
      )
      if (cleanup.state !== 'released') {
        throw webRecordingContractError('gatt.subscribe-failed', 'gatt', 'web-gatt.subscribe-pending-cleanup')
      }
      database.assertCurrent('web-gatt.subscribe')
      physical = this.physicalNotifications.get(characteristic)
    }
    if (physical !== undefined && [...physical.members].some(member => member.state === 'cleanup-failed')) {
      throw webRecordingContractError('gatt.subscribe-failed', 'gatt', 'web-gatt.subscribe-pending-cleanup')
    }
    beforeCreated?.()
    const stream = new CoreBoundedStream<NotificationValue>(options.delivery, options.delivery.overflowPolicy)
    const subscriptionId = this.host.identifiers().subscriptionId(`web-subscription-${this.nextSubscription}`)
    this.nextSubscription += 1
    const managedState: { current: WebManagedSubscription | null } = { current: null }
    const listener = (value: Uint8Array): void => {
      const current = managedState.current
      if (current === null) {
        return
      }
      const deliver = (): void => {
        if (
          (current.state !== 'enabling' && current.state !== 'ready') ||
          !database.record.valid ||
          database.record.database !== database
        ) {
          return
        }
        if (orderedOnly) return
        const copied = ownBytes(value, MAXIMUM_VALUE_BYTES)
        const result = stream.emit(
          {
            value: copied,
            delivery: 'unknown',
            provenance: 'read-or-notification'
          },
          copied.byteLength
        )
        if (result.terminated) {
          current.terminationCleanup = this.removeManagedSubscription(current)
        }
      }
      const fence = current.terminalEvidenceFence
      if (fence === undefined) deliver()
      else fence.enqueue(deliver)
    }
    const managed: WebManagedSubscription = {
      subscriptionId,
      path,
      database,
      characteristic,
      listener,
      stream,
      terminal: terminalRecord(correlation),
      ...(terminalEvidenceScope === undefined ? {} : { terminalEvidenceScope }),
      ...(terminalEvidenceScope === undefined ? {} : { terminalEvidenceFence: new TerminalEvidenceFence() }),
      state: 'enabling',
      startupSettled: false,
      cleanupFailureReported: false,
      removeResult: null,
      terminationCleanup: null
    }
    managedState.current = managed
    this.subscriptions.set(String(subscriptionId), managed)
    const checkOperation = (): void => {
      if (options.signal?.aborted) throw notificationAdmissionError('operation.aborted')
      if (options.deadline !== null && this.host.notificationNow() >= options.deadline)
        throw notificationAdmissionError('operation.timed-out')
    }
    const authorizeStart = (): void => {
      checkOperation()
      beforeCreated?.()
    }
    this.startAuthorizations.set(managed, authorizeStart)
    onCreated?.(managed)
    if (physical === undefined) {
      const members = new Set<WebManagedSubscription>()
      const sourceListener = (value: Uint8Array): void => {
        const owner = this.physicalNotifications.get(characteristic)
        if (owner !== undefined && owner.listening && owner.listener === sourceListener)
          this.dispatchNotification(owner, value)
      }
      characteristic.addNotificationListener(sourceListener)
      physical = {
        characteristic,
        members,
        listener: sourceListener,
        start: Promise.resolve().then(() => {
          // Other consumers may have joined before this microtask. Dispatch if
          // any live member still authorizes it, never for fenced recorders alone.
          let failure: unknown = contractError('lifecycle.invalid-state', 'gatt', 'web-gatt.subscribe')
          let authorized = false
          for (const member of members) {
            if (member.state !== 'enabling') continue
            try {
              this.startAuthorizations.get(member)?.()
              authorized = true
            } catch (error) {
              failure = error
              const admissionFailure =
                webNotificationAdmissionFailure(error) ??
                trustedWebRecordingError(error) ??
                webRecordingContractError('platform.failure', 'gatt', 'web-gatt.subscribe-authorization')
              notificationAdmissionFailures.set(admissionFailure, admissionFailure)
              this.startAdmissionFailures.set(member, admissionFailure)
              continue
            }
          }
          if (authorized) return characteristic.startNotifications()
          throw failure
        }),
        listening: true,
        stop: null
      }
      this.physicalNotifications.set(characteristic, physical)
    }
    if (!physical.listening) {
      characteristic.addNotificationListener(physical.listener)
      physical.listening = true
    }
    physical.members.add(managed)
    this.physicalBySubscription.set(managed, physical)
    const nativeStart = physical.start
    nativeStart.then(
      () => {
        managed.startupSettled = true
        if (managed.state === 'stopping') {
          managed.terminationCleanup = this.removeManagedSubscription(managed)
        }
      },
      error => {
        managed.startupSettled = true
        if (managed.state === 'stopping') {
          console.error(
            '[WebBluetoothGattRuntime.enableSubscription] Late notification start rejected:',
            webHostErrorEvidence([error])
          )
          this.finishStoppedSubscription(managed)
          return
        }
        if (managed.state === 'enabling') {
          console.error(
            '[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:',
            webHostErrorEvidence([error])
          )
          this.beginLogicalSubscriptionStop(managed, 'owner-released')
          this.finishStoppedSubscription(managed)
        }
      }
    )
    try {
      await this.host.runAbortable(
        database.record,
        options,
        () =>
          nativeStart.then(
            () => {
              const failure = this.startAdmissionFailures.get(managed)
              if (failure !== undefined) throw failure
            },
            error => {
              throw this.startAdmissionFailures.get(managed) ?? error
            }
          ),
        'gatt.subscribe-failed',
        'gatt',
        'web-gatt.subscribe'
      )
    } catch (error) {
      this.beginLogicalSubscriptionStop(managed, 'owner-released')
      if (managed.startupSettled && managed.state !== 'stopped') {
        managed.terminationCleanup = this.removeManagedSubscription(managed)
      }
      throw error
    }
    managed.startupSettled = true
    if (managed.state !== 'enabling') {
      const cleanup = await (managed.terminationCleanup ?? this.removeManagedSubscription(managed))
      if (cleanup.state === 'release-failed') {
        throw contractError('gatt.subscribe-failed', 'gatt', 'web-gatt.subscribe-overflow-cleanup')
      }
      throw contractError('stream.overflow', 'stream', 'web-gatt.subscribe-startup-overflow')
    }
    managed.state = 'ready'
    return managed
  }

  private dispatchNotification(physical: WebPhysicalNotifications, value: Uint8Array): void {
    // Admit to all graphs before waking any recorder. Nested callbacks are then
    // admitted immediately, against their arrival-time membership and capacity.
    const members = [...physical.members]
    const databases = new Set<WebGattDatabase>()
    for (const member of members) {
      const database = member.database
      if (
        (member.state === 'enabling' || member.state === 'ready') &&
        database.record.valid &&
        database.record.database === database
      )
        databases.add(database)
    }
    const devices = new Set(members.map(member => member.database.record.device))
    for (const routes of [this.ingressMemberships, this.removingIngressMemberships]) {
      for (const database of routes.get(physical.characteristic)?.keys() ?? []) {
        if (database.record.valid && database.record.database === database && devices.has(database.record.device))
          databases.add(database)
      }
    }
    const ingresses = [...databases]
      .map(database => this.ingressByDatabase.get(database))
      .filter(ingress => ingress !== undefined)
    for (const ingress of ingresses) ingress.holdCallbacks()
    const failures: unknown[] = []
    try {
      for (const ingress of ingresses) {
        try {
          ingress.admit(physical.characteristic, value)
        } catch (error) {
          failures.push(error)
          try {
            ingress.retire('source-failed')
          } catch (retirementError) {
            failures.push(retirementError)
          }
        }
      }
      for (const member of members) {
        try {
          member.listener(value)
        } catch (error) {
          failures.push(error)
          this.beginLogicalSubscriptionStop(member, 'source-failed')
          member.terminationCleanup = this.removeManagedSubscription(member).catch(cleanupError => {
            console.error(
              '[WebBluetoothGattRuntime.dispatchNotification] Subscription cleanup rejected:',
              webHostErrorEvidence([cleanupError])
            )
            return webCleanupFailure('subscription', 'web-gatt.notification-dispatch-cleanup')
          })
        }
      }
    } finally {
      for (const ingress of ingresses) {
        try {
          ingress.releaseCallbacks()
        } catch (error) {
          failures.push(error)
        }
      }
    }
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'Web notification dispatch failed')
  }

  private removeManagedSubscription(managed: WebManagedSubscription): Promise<CleanupRecord> {
    if (managed.removeResult === null) {
      const removal = this.stopManagedSubscription(managed)
      managed.removeResult = removal.then(
        result => {
          if (result.state === 'release-failed') {
            managed.removeResult = null
          }
          return result
        },
        error => {
          managed.removeResult = null
          throw error
        }
      )
    }
    return managed.removeResult
  }

  private async stopManagedSubscription(managed: WebManagedSubscription): Promise<CleanupRecord> {
    if (managed.state === 'stopped') {
      return RELEASED
    }
    this.orderedBySubscription.get(managed)?.retire('service-changed')
    this.beginLogicalSubscriptionStop(managed, 'owner-released')
    const physical = this.physicalBySubscription.get(managed)
    if (physical === undefined) {
      this.finishStoppedSubscription(managed)
      return RELEASED
    }
    const otherActive = [...physical.members].some(
      member => member !== managed && (member.state === 'enabling' || member.state === 'ready')
    )
    if (otherActive) {
      this.finishStoppedSubscription(managed)
      return RELEASED
    }
    if (
      !physical.members.has(managed) ||
      ![...physical.members].some(member => this.recordingSubscriptions.has(member))
    )
      this.detachPhysicalListener(physical)
    if (!managed.startupSettled) {
      return webCleanupFailure('subscription', 'web-gatt.notification-start-pending')
    }
    managed.state = 'stopping'
    if (physical.stop === null) {
      physical.stop = Promise.resolve()
        .then(() => physical.characteristic.stopNotifications())
        .then(
          () => RELEASED,
          error => {
            console.error(
              '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
              webHostErrorEvidence([error])
            )
            return webCleanupFailure('subscription', 'web-gatt.unsubscribe')
          }
        )
    }
    const cleanup = await physical.stop
    if (cleanup.state === 'released') {
      for (const member of [...physical.members]) this.finishStoppedSubscription(member)
    } else {
      managed.state = 'cleanup-failed'
      managed.cleanupFailureReported = false
      physical.stop = null
    }
    return cleanup
  }

  private beginLogicalSubscriptionStop(
    managed: WebManagedSubscription,
    reason: 'connection-lost' | 'owner-released' | 'source-failed'
  ): Promise<boolean> {
    if (managed.state === 'stopped' || managed.state === 'stopping' || managed.state === 'cleanup-failed') {
      return Promise.resolve(true)
    }
    managed.state = 'stopping'
    const physical = this.physicalBySubscription.get(managed)
    if (
      physical !== undefined &&
      ![...physical.members].some(member => this.recordingSubscriptions.has(member)) &&
      ![...physical.members].some(member => member.state === 'enabling' || member.state === 'ready')
    )
      this.detachPhysicalListener(physical)
    const scope = managed.terminalEvidenceScope
    if (scope === undefined || this.terminalEvidence === undefined) {
      managed.stream.closeWithReason(reason)
      return Promise.resolve(true)
    }
    return (managed.terminalEvidenceFence?.seal() ?? Promise.resolve())
      .then(() => {
        const terminal = managed.stream.previewTerminal(reason)
        if (terminal === null) return undefined
        return this.terminalEvidence?.commitTerminal(scope, terminal)
      })
      .then(
        receipt => {
          if (receipt === undefined) return false
          if (!('kind' in receipt)) managed.stream.closeWithReason(reason, null, receipt)
          // A refusal retains the original staged terminal; it must never be
          // replaced by a plausible failure after the source has stopped.
          return !('kind' in receipt)
        },
        () => {
          // Commit uncertainty retains the original staged terminal for recovery.
          // Do not publish or acknowledge a substituted terminal.
          return false
        }
      )
  }

  private finishStoppedSubscription(managed: WebManagedSubscription): void {
    managed.state = 'stopped'
    this.subscriptions.delete(String(managed.subscriptionId))
    this.orderedBySubscription.delete(managed)
    this.recordingSubscriptions.delete(managed)
    const physical = this.physicalBySubscription.get(managed)
    this.physicalBySubscription.delete(managed)
    if (physical !== undefined) {
      physical.members.delete(managed)
      if (physical.members.size === 0) {
        this.detachPhysicalListener(physical)
        if (this.physicalNotifications.get(physical.characteristic) === physical) {
          this.physicalNotifications.delete(physical.characteristic)
        }
      }
    }
  }

  private detachPhysicalListener(physical: WebPhysicalNotifications): void {
    if (!physical.listening) return
    physical.listening = false
    physical.characteristic.removeNotificationListener(physical.listener)
  }

  private async stopDatabaseSubscriptions(database: WebGattDatabase): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const subscription of [...this.subscriptions.values()]) {
      if (subscription.database === database) {
        const cleanup = await this.removeManagedSubscription(subscription)
        failures.push(...cleanup.failures)
      }
    }
    return failures.length === 0 ? RELEASED : { state: 'release-failed', failures }
  }

  private async readResult(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): Promise<CharacteristicReadResult<string, string>> {
    const database = this.host.requireDatabase(path, 'web-gatt.read')
    return {
      ...(await this.readCharacteristic(database, path, request.operation)),
      terminal: terminalRecord(request.operation.correlation)
    }
  }

  private async writeResult(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): Promise<WriteResult<string, string>> {
    const database = this.host.requireDatabase(path, 'web-gatt.write')
    await this.writeCharacteristic(database, path, request.bytes, {
      signal: request.operation.signal,
      deadline: request.operation.deadline,
      mode: request.mode
    })
    return terminalWriteReceipt(request.operation.correlation, 'confirmed')
  }

  private async readDescriptorResult(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): Promise<ReadResult<string, string>> {
    const database = this.host.requireDatabase(path, 'web-gatt.read-descriptor')
    return {
      value: await this.readDescriptorValue(database, path, request.operation),
      terminal: terminalRecord(request.operation.correlation)
    }
  }

  private async writeDescriptorResult(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): Promise<WriteResult<string, string>> {
    const database = this.host.requireDatabase(path, 'web-gatt.write-descriptor')
    await this.writeDescriptorValue(database, path, request.bytes, {
      signal: request.operation.signal,
      deadline: request.operation.deadline,
      mode: request.mode
    })
    return terminalWriteReceipt(request.operation.correlation, 'confirmed')
  }

  private async subscribeResult(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, string>
  ): Promise<BackendSubscription<string, string, string, string, string>> {
    const database = this.host.requireDatabase(path, 'web-gatt.subscribe')
    return new WebBackendSubscription(
      await this.enableSubscription(
        database,
        path,
        request.options,
        request.operation.correlation,
        undefined,
        false,
        undefined,
        request.terminalEvidenceScope
      )
    )
  }

  private async unsubscribeResult(
    subscription: BackendSubscription<string, string, string, string, string>,
    operation: OperationOptions<string, string>
  ): Promise<OperationTerminalRecord<string, string>> {
    if (!(subscription instanceof WebBackendSubscription)) {
      throw contractError('ownership.denied', 'gatt', 'web-gatt.unsubscribe.subscription')
    }
    const managed = this.subscriptions.get(String(subscription.subscriptionId))
    if (
      managed === undefined ||
      !subscription.isManagedBy(managed) ||
      characteristicKey(managed.path) !== characteristicKey(subscription.path)
    ) {
      throw contractError('gatt.stale-handle', 'gatt', 'web-gatt.unsubscribe')
    }
    const cleanup = await this.removeManagedSubscription(managed)
    if (cleanup.state === 'release-failed') {
      throw contractError('gatt.subscribe-failed', 'gatt', 'web-gatt.unsubscribe')
    }
    return terminalRecord(operation.correlation)
  }

  private requireCharacteristic(
    database: WebGattDatabase,
    path: CharacteristicPath<string, string, string, string, string>,
    operation: string
  ): WebBluetoothCharacteristicBoundary {
    database.assertPath(path, operation)
    if (!database.record.grantedServices.has(String(path.serviceUuid))) {
      throw contractError('chooser.optional-service-not-granted', 'chooser', operation)
    }
    const characteristic = database.characteristicBoundaries.get(characteristicKey(path))
    if (characteristic === undefined) {
      throw this.host.staleGattError(operation)
    }
    return characteristic
  }

  private requireDescriptor(
    database: WebGattDatabase,
    path: DescriptorPath<string, string, string, string, string, string>,
    operation: string
  ): WebBluetoothDescriptorBoundary {
    this.requireCharacteristic(database, path, operation)
    const descriptor = database.descriptorBoundaries.get(descriptorKey(path))
    if (descriptor === undefined) {
      throw this.host.staleGattError(operation)
    }
    return descriptor
  }

  private dispatch<Result>(
    operation: OperationOptions<string, string>,
    start: (operation: OperationOptions<string, string>) => Promise<Result>
  ): BackendOperationDispatch<string, Result> {
    const handle = this.host.identifiers().backendOperationHandle(`web-operation-${this.nextOperation}`)
    this.nextOperation += 1
    const settlement = createOperationSettlementCoordinator<string, Result>(handle)
    const controller = new AbortController()
    let terminal = false
    let acknowledgement: CancellationAcknowledgement<string> | null = null
    const abortFromCaller = () => {
      controller.abort()
    }
    if (operation.signal?.aborted === true) {
      controller.abort()
    } else {
      operation.signal?.addEventListener('abort', abortFromCaller, { once: true })
    }
    const completion = start({ ...operation, signal: controller.signal }).then(
      result => {
        terminal = true
        operation.signal?.removeEventListener('abort', abortFromCaller)
        return settlement.complete(result)
      },
      error => {
        terminal = true
        operation.signal?.removeEventListener('abort', abortFromCaller)
        throw error
      }
    )
    return createBackendOperationDispatch(handle, completion, async () => {
      if (acknowledgement !== null) {
        return acknowledgement
      }
      if (terminal) {
        acknowledgement = settlement.acknowledgeCancellation('already-terminal')
        return acknowledgement
      }
      controller.abort()
      acknowledgement = settlement.acknowledgeCancellation('cancellation-requested')
      return acknowledgement
    })
  }

  private operationCorrelation(label: string) {
    const correlation = this.host.identifiers().operationCorrelation(`${label}-${this.nextOperation}`)
    this.nextOperation += 1
    return correlation
  }
}

function terminalRecord(
  correlation: OperationOptions<string, string>['correlation']
): OperationTerminalRecord<string, string> {
  return { correlation, outcome: 'succeeded', cause: null }
}

function terminalWriteReceipt(
  correlation: OperationOptions<string, string>['correlation'],
  commitState: WriteReceipt<string, string>['commitState']
): WriteReceipt<string, string> {
  return { terminal: terminalRecord(correlation), commitState }
}
