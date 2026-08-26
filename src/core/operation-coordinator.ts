// src/core/operation-coordinator.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import type { CleanupFailure, CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { OperationTerminalOutcome, PublicOperationOptions } from '../backend-contract/operations'
import type { OperationCorrelation } from '../backend-contract/primitives'
import { ResourceLedger } from './resource-ledger'
import { CoreTraceRecorder } from './trace-recorder'

export type CoreOperationOutcome = OperationTerminalOutcome

export type CoreCommitState = 'not-applicable' | 'confirmed' | 'unknown'

/**
 * Maximum number of waiting operations retained by one connection lane by default.
 *
 * A backpressure bound rather than a fixed invariant: it is already overridable
 * through the coordinator's construction options, so a host that wants a deeper
 * queue can ask for one. The default is small on purpose -- a GATT connection
 * executes one ATT transaction at a time, so a deep queue does not increase
 * throughput, it only converts a stalled peripheral into a large backlog of
 * operations that will each fail against their own deadline.
 */
const DEFAULT_MAXIMUM_QUEUED_OPERATIONS_PER_CONNECTION = 8
const DEFAULT_FAIRNESS_KEY = 'default'

export interface CoreOperationSuccess<Attachment extends string, Value> {
  readonly correlation: OperationCorrelation<Attachment, string>
  readonly outcome: 'succeeded'
  readonly value: Value
  readonly error: null
  readonly commitState: 'not-applicable' | 'confirmed'
}

export interface CoreOperationFailure<Attachment extends string> {
  readonly correlation: OperationCorrelation<Attachment, string>
  readonly outcome: Exclude<CoreOperationOutcome, 'succeeded'>
  readonly value: null
  readonly error: NormalizedBleError
  readonly commitState: CoreCommitState
}

export type CoreOperationResult<Attachment extends string, Value> =
  | CoreOperationSuccess<Attachment, Value>
  | CoreOperationFailure<Attachment>

export interface CoreOperationDispatch<Value> {
  readonly completion: Promise<Value>
  requestCancellation(): Promise<void>
}

/** A FIFO-head pre-native gate whose current state is rechecked at dispatch. */
export interface CoreOperationAdmission {
  waitUntilReady(): Promise<void>
  isReady(): boolean
  close(): Promise<CleanupRecord>
  onCleanupFailure?: (handler: (failure: CleanupFailure) => void) => void
}

export interface CoreOperationExecution<Attachment extends string, Value> {
  readonly queueKey: string | null
  /** Internal stable class used for deterministic per-connection round-robin selection. */
  readonly fairnessKey?: string
  readonly options: PublicOperationOptions
  readonly mayCommit: boolean
  readonly retainedPayloadBytes?: number
  readonly admission?: () => CoreOperationAdmission
  readonly dispatch: (correlation: OperationCorrelation<Attachment, string>) => CoreOperationDispatch<Value>
  readonly onQuarantined?: () => void
  readonly onLateSuccess?: (value: Value) => Promise<void>
  readonly onLateFailure?: (error: Error) => Promise<void>
}

type OperationPhase = 'queued' | 'admitting' | 'admission-cancelled' | 'dispatched' | 'quarantined' | 'completed'

interface TrackedOperation {
  readonly queueKey: string | null
  readonly fairnessKey: string
  phase: OperationPhase
  cancelOperation(outcome: Exclude<CoreOperationOutcome, 'succeeded' | 'failed'>): void
  beginOperation(): void
}

interface PendingOperation<Attachment extends string, Value> extends TrackedOperation {
  readonly correlation: OperationCorrelation<Attachment, string>
  readonly traceLabel: string
  readonly execution: CoreOperationExecution<Attachment, Value>
  readonly resolve: (result: CoreOperationResult<Attachment, Value>) => void
  readonly abortListener: () => void
  deadlineTimer: ReturnType<typeof setTimeout> | null
  dispatchHandle: CoreOperationDispatch<Value> | null
  admissionHandle: CoreOperationAdmission | null
  admissionClosePromise: Promise<CleanupRecord> | null
  publicResult: CoreOperationResult<Attachment, Value> | null
  readonly retainedPayloadBytes: number
  payloadRetained: boolean
}

interface ConnectionQueue {
  readonly lanes: Map<string, TrackedOperation[]>
  readonly fairnessOrder: string[]
  lastDispatchedFairnessKey: string | null
}

interface DrainWaiter {
  readonly queueKey: string | undefined
  readonly resolve: () => void
}

export interface CoreQuarantineDrainWait {
  readonly promise: Promise<void>
  cancel(): void
}

export interface CoreOperationCoordinatorOptions<Attachment extends string> {
  readonly now: () => number
  readonly createCorrelation: () => OperationCorrelation<Attachment, string>
  readonly resourceLedger: ResourceLedger
  readonly trace: CoreTraceRecorder
  /**
   * Finite per-connection waiting capacity. The active operation is not part of
   * this bound; an overflow is rejected before operation state or payload
   * ownership is allocated.
   */
  readonly maximumQueuedOperationsPerConnection?: number
}

/**
 * Serializes operations for each connection, chooses one public terminal result,
 * and keeps a cancelled dispatched operation at the FIFO head until its backend
 * acknowledgement arrives. That prevents a late native acknowledgement from
 * reordering same-connection radio work.
 */
export class CoreOperationCoordinator<Attachment extends string> {
  private readonly queues = new Map<string, ConnectionQueue>()
  private readonly operations = new Set<TrackedOperation>()
  private admissionOpen = true
  private nextTraceLabel = 1
  private quarantinedOperations = 0
  private drainingAdmissions = 0
  private readonly quarantineDrainWaiters = new Set<DrainWaiter>()
  private readonly retainedCleanupFailures = new Map<string | null, CleanupFailure[]>()
  private readonly maximumQueuedOperationsPerConnection: number

  constructor(private readonly options: CoreOperationCoordinatorOptions<Attachment>) {
    const maximumQueuedOperationsPerConnection =
      options.maximumQueuedOperationsPerConnection ?? DEFAULT_MAXIMUM_QUEUED_OPERATIONS_PER_CONNECTION
    if (!Number.isSafeInteger(maximumQueuedOperationsPerConnection) || maximumQueuedOperationsPerConnection < 1) {
      throw contractError('argument.invalid', 'core', 'operation-coordinator.maximum-queued-operations-per-connection')
    }
    this.maximumQueuedOperationsPerConnection = maximumQueuedOperationsPerConnection
  }

  run<Value>(execution: CoreOperationExecution<Attachment, Value>): Promise<CoreOperationResult<Attachment, Value>> {
    return this.runWithAdmission(execution, false)
  }

  /** Internal teardown lane: no public admission, but required cleanup may still dispatch. */
  runCleanup<Value>(
    execution: CoreOperationExecution<Attachment, Value>
  ): Promise<CoreOperationResult<Attachment, Value>> {
    return this.runWithAdmission(execution, true)
  }

  private runWithAdmission<Value>(
    execution: CoreOperationExecution<Attachment, Value>,
    allowAfterAdmissionClosed: boolean
  ): Promise<CoreOperationResult<Attachment, Value>> {
    const correlation = this.options.createCorrelation()
    if (!this.admissionOpen && !allowAfterAdmissionClosed) {
      return Promise.resolve(
        this.failure(correlation, 'destroyed', execution.mayCommit, 'operation-coordinator.admission-closed')
      )
    }
    if (execution.options.signal?.aborted === true) {
      return Promise.resolve(
        this.failure(correlation, 'aborted', execution.mayCommit, 'operation-coordinator.pre-abort')
      )
    }
    if (execution.options.deadline !== null && execution.options.deadline <= this.options.now()) {
      return Promise.resolve(
        this.failure(correlation, 'timed-out', execution.mayCommit, 'operation-coordinator.pre-deadline')
      )
    }
    const retainedPayloadBytes = execution.retainedPayloadBytes ?? 0
    if (!Number.isSafeInteger(retainedPayloadBytes) || retainedPayloadBytes < 0) {
      return Promise.resolve(
        this.failure(correlation, 'failed', execution.mayCommit, 'operation-coordinator.invalid-retained-payload-bytes')
      )
    }
    if (execution.queueKey !== null && !this.canAdmitQueuedOperation(execution.queueKey)) {
      const counts = this.activeCounts()
      this.options.trace.record({
        timestamp: this.options.now(),
        resource: 'operation',
        transition: 'queue-rejected',
        operation: null,
        cause: 'stream.quota',
        queuedOperations: counts.queued,
        dispatchedOperations: counts.dispatched,
        quarantinedOperations: counts.quarantined
      })
      return Promise.resolve(
        this.failure(correlation, 'failed', execution.mayCommit, 'operation-coordinator.queue-capacity', 'stream.quota')
      )
    }
    return new Promise(resolve => {
      const traceLabel = `operation-${this.nextTraceLabel}`
      this.nextTraceLabel += 1
      const operation: PendingOperation<Attachment, Value> = {
        correlation,
        traceLabel,
        execution,
        resolve,
        abortListener: () => this.cancel(operation, 'aborted'),
        deadlineTimer: null,
        dispatchHandle: null,
        admissionHandle: null,
        admissionClosePromise: null,
        phase: 'queued',
        queueKey: execution.queueKey,
        fairnessKey: execution.fairnessKey ?? DEFAULT_FAIRNESS_KEY,
        cancelOperation: outcome => this.cancel(operation, outcome),
        beginOperation: () => this.beginOperation(operation),
        publicResult: null,
        retainedPayloadBytes,
        payloadRetained: retainedPayloadBytes > 0
      }
      if (operation.payloadRetained) {
        this.options.resourceLedger.retainOperationBytes(retainedPayloadBytes)
      }
      this.operations.add(operation)
      execution.options.signal?.addEventListener('abort', operation.abortListener, { once: true })
      if (execution.options.deadline !== null) {
        const delay = Math.max(0, execution.options.deadline - this.options.now())
        operation.deadlineTimer = setTimeout(() => this.cancel(operation, 'timed-out'), delay)
      }
      this.options.resourceLedger.increment('queuedOperations')
      this.record(operation, 'queued', null)
      if (execution.queueKey === null) {
        this.beginOperation(operation)
        return
      }
      const queue = this.queues.get(execution.queueKey) ?? {
        lanes: new Map<string, TrackedOperation[]>(),
        fairnessOrder: [],
        lastDispatchedFairnessKey: null
      }
      let lane = queue.lanes.get(operation.fairnessKey)
      if (lane === undefined) {
        lane = []
        queue.lanes.set(operation.fairnessKey, lane)
        queue.fairnessOrder.push(operation.fairnessKey)
      }
      lane.push(operation)
      this.queues.set(execution.queueKey, queue)
      this.pump(execution.queueKey)
    })
  }

  cancelQueue(queueKey: string, outcome: Exclude<CoreOperationOutcome, 'succeeded' | 'failed'>): void {
    if (!this.queues.has(queueKey)) {
      return
    }
    for (const operation of [...this.operations]) {
      if (operation.queueKey === queueKey) {
        operation.cancelOperation(outcome)
      }
    }
  }

  destroy(): void {
    this.admissionOpen = false
    for (const operation of [...this.operations]) {
      operation.cancelOperation('destroyed')
    }
  }

  waitForQuarantineDrain(queueKey?: string): Promise<void> {
    return this.waitForQuarantineDrainCancellable(queueKey).promise
  }

  waitForQuarantineDrainCancellable(queueKey?: string): CoreQuarantineDrainWait {
    if (!this.hasPendingDrain(queueKey)) {
      return { promise: Promise.resolve(), cancel: () => undefined }
    }
    let waiter: DrainWaiter | null = null
    const promise = new Promise<void>(resolve => {
      waiter = { queueKey, resolve }
      this.quarantineDrainWaiters.add(waiter)
    })
    return {
      promise,
      cancel: () => {
        if (waiter !== null) {
          this.quarantineDrainWaiters.delete(waiter)
          waiter = null
        }
      }
    }
  }

  hasPendingDrain(queueKey?: string): boolean {
    for (const operation of this.operations) {
      if (
        (operation.phase === 'quarantined' || operation.phase === 'admission-cancelled') &&
        (queueKey === undefined || operation.queueKey === queueKey)
      ) {
        return true
      }
    }
    return false
  }

  takeCleanupFailures(queueKey?: string): readonly CleanupFailure[] {
    if (queueKey === undefined) {
      const failures = Object.freeze([...this.retainedCleanupFailures.values()].flat())
      this.retainedCleanupFailures.clear()
      return failures
    }
    const failures = Object.freeze([...(this.retainedCleanupFailures.get(queueKey) ?? [])])
    this.retainedCleanupFailures.delete(queueKey)
    return failures
  }

  activeCounts(): { readonly queued: number; readonly dispatched: number; readonly quarantined: number } {
    let queued = 0
    let dispatched = 0
    for (const operation of this.operations) {
      if (operation.phase === 'queued' || operation.phase === 'admitting') {
        queued += 1
      }
      if (operation.phase === 'dispatched') {
        dispatched += 1
      }
    }
    return { queued, dispatched, quarantined: this.quarantinedOperations }
  }

  private pump(queueKey: string): void {
    const queue = this.queues.get(queueKey)
    if (queue === undefined) {
      return
    }
    for (const lane of queue.lanes.values()) {
      const head = lane[0]
      if (head !== undefined && head.phase !== 'queued') {
        return
      }
    }
    this.selectNextQueuedOperation(queue)?.beginOperation()
  }

  private selectNextQueuedOperation(queue: ConnectionQueue): TrackedOperation | undefined {
    if (queue.fairnessOrder.length === 0) {
      return undefined
    }
    const lastIndex =
      queue.lastDispatchedFairnessKey === null ? -1 : queue.fairnessOrder.indexOf(queue.lastDispatchedFairnessKey)
    const startIndex = lastIndex < 0 ? 0 : (lastIndex + 1) % queue.fairnessOrder.length
    for (let offset = 0; offset < queue.fairnessOrder.length; offset += 1) {
      const index = (startIndex + offset) % queue.fairnessOrder.length
      const fairnessKey = queue.fairnessOrder[index]
      if (fairnessKey === undefined) {
        continue
      }
      const lane = queue.lanes.get(fairnessKey)
      const head = lane?.[0]
      if (head !== undefined && head.phase === 'queued') {
        queue.lastDispatchedFairnessKey = fairnessKey
        return head
      }
    }
    return undefined
  }

  private canAdmitQueuedOperation(queueKey: string): boolean {
    const queue = this.queues.get(queueKey)
    if (queue === undefined) {
      return true
    }
    let queued = 0
    for (const lane of queue.lanes.values()) {
      for (const operation of lane) {
        if (operation.phase === 'queued' || operation.phase === 'admitting') {
          queued += 1
        }
      }
    }
    return queued < this.maximumQueuedOperationsPerConnection
  }

  private beginOperation<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (operation.execution.admission === undefined) {
      this.dispatch(operation)
      return
    }
    this.beginAdmission(operation)
  }

  private beginAdmission<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (operation.phase !== 'queued') {
      return
    }
    operation.phase = 'admitting'
    this.record(operation, 'admitting', null)
    try {
      operation.admissionHandle = operation.execution.admission?.() ?? null
    } catch (error) {
      this.failAdmission(operation, error)
      return
    }
    if (operation.admissionHandle === null) {
      this.failAdmission(
        operation,
        contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.missing-admission-handle')
      )
      return
    }
    operation.admissionHandle.onCleanupFailure?.(failure => this.retainCleanupFailures(operation.queueKey, [failure]))
    this.waitForAdmission(operation)
  }

  private waitForAdmission<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (operation.phase !== 'admitting') {
      return
    }
    const admission = operation.admissionHandle
    if (admission === null) {
      this.failAdmission(
        operation,
        contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.missing-admission-wait')
      )
      return
    }
    let wait: Promise<void>
    try {
      wait = admission.waitUntilReady()
    } catch (error) {
      this.failAdmission(operation, error)
      return
    }
    wait.then(
      () => this.finishAdmission(operation),
      error => this.failAdmission(operation, error)
    )
  }

  private finishAdmission<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (operation.phase !== 'admitting') {
      return
    }
    if (operation.execution.options.signal?.aborted === true) {
      this.cancel(operation, 'aborted')
      return
    }
    if (operation.execution.options.deadline !== null && operation.execution.options.deadline <= this.options.now()) {
      this.cancel(operation, 'timed-out')
      return
    }
    const admission = operation.admissionHandle
    if (admission === null) {
      this.failAdmission(
        operation,
        contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.missing-admission-recheck')
      )
      return
    }
    let ready: boolean
    try {
      ready = admission.isReady()
    } catch (error) {
      this.failAdmission(operation, error)
      return
    }
    if (!ready) {
      this.waitForAdmission(operation)
      return
    }
    this.dispatch(operation)
  }

  private dispatch<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (operation.phase !== 'queued' && operation.phase !== 'admitting') {
      return
    }
    if (operation.phase === 'admitting') {
      const admission = operation.admissionHandle
      if (admission === null) {
        this.failAdmission(
          operation,
          contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.missing-admission-dispatch')
        )
        return
      }
      let ready: boolean
      try {
        ready = admission.isReady()
      } catch (error) {
        this.failAdmission(operation, error)
        return
      }
      if (!ready) {
        this.waitForAdmission(operation)
        return
      }
    }
    operation.phase = 'dispatched'
    this.options.resourceLedger.decrement('queuedOperations')
    this.options.resourceLedger.increment('dispatchedOperations')
    this.record(operation, 'dispatched', null)
    let dispatch: CoreOperationDispatch<Value>
    try {
      dispatch = operation.execution.dispatch(operation.correlation)
    } catch (error) {
      const normalized =
        error instanceof BackendContractError
          ? error.normalized
          : contractError('platform.failure', 'core', 'operation-coordinator.synchronous-dispatch').normalized
      const admissionClose = this.closeAdmission(operation)
      const completion = admissionClose.then(
        () => Promise.reject(new BackendContractError(normalized)),
        () => Promise.reject(new BackendContractError(normalized))
      )
      operation.dispatchHandle = {
        completion,
        requestCancellation: () =>
          admissionClose.then(
            () => undefined,
            () => undefined
          )
      }
      completion.catch(dispatchError => this.acknowledgeFailure(operation, dispatchError))
      return
    }
    const completion =
      operation.admissionHandle === null
        ? dispatch.completion
        : (() => {
            const admissionClose = this.closeAdmission(operation)
            return dispatch.completion.then(
              value => admissionClose.then(() => value),
              error =>
                admissionClose.then(
                  () => Promise.reject(error),
                  () => Promise.reject(error)
                )
            )
          })()
    operation.dispatchHandle = {
      completion,
      requestCancellation: () => dispatch.requestCancellation()
    }
    completion.then(
      value => this.acknowledgeSuccess(operation, value),
      error => this.acknowledgeFailure(operation, error)
    )
  }

  private cancel<Value>(
    operation: PendingOperation<Attachment, Value>,
    outcome: Exclude<CoreOperationOutcome, 'succeeded' | 'failed'>
  ): void {
    if (operation.publicResult !== null || operation.phase === 'completed') {
      return
    }
    if (operation.phase === 'queued') {
      this.options.resourceLedger.decrement('queuedOperations')
      this.releasePayload(operation)
      this.removeQueuedOperation(operation)
      this.settlePublic(
        operation,
        this.failure(operation.correlation, outcome, false, 'operation-coordinator.cancel-queued')
      )
      this.completeAcknowledged(operation)
      return
    }
    if (operation.phase === 'admitting') {
      this.options.resourceLedger.decrement('queuedOperations')
      this.releasePayload(operation)
      this.settlePublic(
        operation,
        this.failure(operation.correlation, outcome, false, 'operation-coordinator.cancel-admitting')
      )
      operation.phase = 'admission-cancelled'
      this.drainingAdmissions += 1
      this.record(operation, 'admission-cancelled', this.codeForOutcome(outcome))
      this.closeAdmission(operation).then(
        () => this.releaseAdmissionDrain(operation, 'admission-cancelled-drained'),
        error => {
          this.record(
            operation,
            'admission-cancel-close-failed',
            error instanceof BackendContractError ? error.normalized.code : 'platform.failure'
          )
          this.releaseAdmissionDrain(operation, 'admission-cancelled-drained')
        }
      )
      return
    }
    if (operation.phase === 'dispatched') {
      this.options.resourceLedger.decrement('dispatchedOperations')
      this.settlePublic(
        operation,
        this.failure(
          operation.correlation,
          outcome,
          operation.execution.mayCommit,
          'operation-coordinator.cancel-dispatched'
        )
      )
      operation.phase = 'quarantined'
      this.quarantinedOperations += 1
      this.notifyQuarantined(operation)
      this.record(operation, 'quarantined', this.codeForOutcome(outcome))
      const dispatch = operation.dispatchHandle
      if (dispatch === null) {
        throw contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.cancel-without-dispatch')
      }
      dispatch.requestCancellation().then(
        () => this.record(operation, 'cancellation-requested', this.codeForOutcome(outcome)),
        () => this.record(operation, 'cancellation-request-failed', 'platform.failure')
      )
    }
  }

  private async acknowledgeSuccess<Value>(operation: PendingOperation<Attachment, Value>, value: Value): Promise<void> {
    if (operation.phase === 'quarantined') {
      await this.runLateSuccess(operation, value)
      this.releaseQuarantine(operation, 'late-success')
      return
    }
    if (operation.phase !== 'dispatched') {
      return
    }
    this.options.resourceLedger.decrement('dispatchedOperations')
    this.releasePayload(operation)
    this.settlePublic<Value>(operation, {
      correlation: operation.correlation,
      outcome: 'succeeded',
      value,
      error: null,
      commitState: operation.execution.mayCommit ? 'confirmed' : 'not-applicable'
    })
    this.completeAcknowledged(operation)
  }

  private async acknowledgeFailure<Value>(operation: PendingOperation<Attachment, Value>, error: Error): Promise<void> {
    if (operation.phase === 'quarantined') {
      await this.runLateFailure(operation, error)
      this.releaseQuarantine(operation, 'late-failure')
      return
    }
    if (operation.phase !== 'dispatched') {
      return
    }
    this.options.resourceLedger.decrement('dispatchedOperations')
    this.releasePayload(operation)
    const normalized =
      error instanceof BackendContractError
        ? error.normalized
        : contractError('platform.failure', 'core', 'operation-coordinator.backend-rejection').normalized
    const result: CoreOperationFailure<Attachment> = {
      correlation: operation.correlation,
      outcome: 'failed',
      value: null,
      error: normalized,
      commitState: operation.execution.mayCommit ? 'unknown' : 'not-applicable'
    }
    this.settlePublic(operation, result)
    this.completeAcknowledged(operation)
  }

  private releaseQuarantine<Value>(operation: PendingOperation<Attachment, Value>, transition: string): void {
    this.quarantinedOperations -= 1
    if (this.quarantinedOperations < 0) {
      throw contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.quarantine-underflow')
    }
    this.record(operation, transition, operation.publicResult?.error?.code ?? null)
    this.releasePayload(operation)
    this.completeAcknowledged(operation)
    this.resolveOperationDrain()
  }

  private releasePayload<Value>(operation: PendingOperation<Attachment, Value>): void {
    if (!operation.payloadRetained) {
      return
    }
    operation.payloadRetained = false
    this.options.resourceLedger.releaseOperationBytes(operation.retainedPayloadBytes)
  }

  private resolveOperationDrain(): void {
    for (const waiter of this.quarantineDrainWaiters) {
      if (!this.hasPendingDrain(waiter.queueKey)) {
        waiter.resolve()
        this.quarantineDrainWaiters.delete(waiter)
      }
    }
  }

  private failAdmission<Value>(operation: PendingOperation<Attachment, Value>, error: unknown): void {
    if (operation.phase !== 'admitting') {
      return
    }
    this.options.resourceLedger.decrement('queuedOperations')
    this.releasePayload(operation)
    const normalized =
      error instanceof BackendContractError
        ? error.normalized
        : contractError('platform.failure', 'core', 'operation-coordinator.admission-rejection').normalized
    const outcome =
      normalized.code === 'operation.aborted'
        ? 'aborted'
        : normalized.code === 'operation.timed-out'
          ? 'timed-out'
          : 'failed'
    this.settlePublic<Value>(
      operation,
      this.failure(operation.correlation, outcome, false, normalized.operation, normalized.code)
    )
    operation.phase = 'admission-cancelled'
    this.drainingAdmissions += 1
    this.record(operation, 'admission-failed', normalized.code)
    this.closeAdmission(operation).then(
      () => this.releaseAdmissionDrain(operation, 'admission-failed-drained'),
      closeError => {
        this.record(
          operation,
          'admission-failure-close-failed',
          closeError instanceof BackendContractError ? closeError.normalized.code : 'platform.failure'
        )
        this.releaseAdmissionDrain(operation, 'admission-failed-drained')
      }
    )
  }

  private closeAdmission<Value>(operation: PendingOperation<Attachment, Value>): Promise<CleanupRecord> {
    if (operation.admissionClosePromise !== null) {
      return operation.admissionClosePromise
    }
    const admission = operation.admissionHandle
    if (admission === null) {
      operation.admissionClosePromise = Promise.resolve({ state: 'released', failures: [] })
      return operation.admissionClosePromise
    }
    const admissionOwnsCleanupFailures = admission.onCleanupFailure !== undefined
    try {
      operation.admissionClosePromise = Promise.resolve(admission.close()).then(
        cleanup => {
          const normalized = cleanup ?? { state: 'released', failures: [] }
          if (!admissionOwnsCleanupFailures) {
            this.retainCleanupFailures(operation.queueKey, normalized.failures)
          }
          return normalized
        },
        error => {
          const failure: CleanupFailure = {
            resourceKind: 'operation-admission',
            error:
              error instanceof BackendContractError
                ? error.normalized
                : contractError('platform.failure', 'cleanup', 'operation-coordinator.admission-close').normalized
          }
          if (!admissionOwnsCleanupFailures) {
            this.retainCleanupFailures(operation.queueKey, [failure])
          }
          return { state: 'release-failed', failures: [failure] }
        }
      )
    } catch (error) {
      const failure: CleanupFailure = {
        resourceKind: 'operation-admission',
        error:
          error instanceof BackendContractError
            ? error.normalized
            : contractError('platform.failure', 'cleanup', 'operation-coordinator.admission-close').normalized
      }
      this.retainCleanupFailures(operation.queueKey, [failure])
      operation.admissionClosePromise = Promise.resolve({ state: 'release-failed', failures: [failure] })
    }
    return operation.admissionClosePromise
  }

  private releaseAdmissionDrain<Value>(operation: PendingOperation<Attachment, Value>, transition: string): void {
    if (operation.phase !== 'admission-cancelled') {
      return
    }
    this.drainingAdmissions -= 1
    if (this.drainingAdmissions < 0) {
      throw contractError('lifecycle.invariant-violation', 'core', 'operation-coordinator.admission-drain-underflow')
    }
    this.record(operation, transition, operation.publicResult?.error?.code ?? null)
    this.completeAcknowledged(operation)
    this.resolveOperationDrain()
  }

  private retainCleanupFailures(queueKey: string | null, failures: readonly CleanupFailure[]): void {
    if (failures.length === 0) {
      return
    }
    const retained = this.retainedCleanupFailures.get(queueKey) ?? []
    retained.push(...failures)
    this.retainedCleanupFailures.set(queueKey, retained)
  }

  private notifyQuarantined<Value>(operation: PendingOperation<Attachment, Value>): void {
    try {
      operation.execution.onQuarantined?.()
    } catch (error) {
      this.record(
        operation,
        'quarantine-notification-failed',
        error instanceof BackendContractError ? error.normalized.code : 'platform.failure'
      )
    }
  }

  private async runLateSuccess<Value>(operation: PendingOperation<Attachment, Value>, value: Value): Promise<void> {
    if (operation.execution.onLateSuccess === undefined) {
      return
    }
    try {
      await operation.execution.onLateSuccess(value)
    } catch (error) {
      this.record(
        operation,
        'late-success-compensation-failed',
        error instanceof BackendContractError ? error.normalized.code : 'platform.failure'
      )
    }
  }

  private async runLateFailure<Value>(operation: PendingOperation<Attachment, Value>, error: Error): Promise<void> {
    if (operation.execution.onLateFailure === undefined) {
      return
    }
    try {
      await operation.execution.onLateFailure(error)
    } catch (lateFailure) {
      this.record(
        operation,
        'late-failure-cleanup-failed',
        lateFailure instanceof BackendContractError ? lateFailure.normalized.code : 'platform.failure'
      )
    }
  }

  private settlePublic<Value>(
    operation: PendingOperation<Attachment, Value>,
    result: CoreOperationResult<Attachment, Value>
  ): void {
    if (operation.publicResult !== null) {
      return
    }
    operation.publicResult = result
    if (operation.deadlineTimer !== null) {
      clearTimeout(operation.deadlineTimer)
      operation.deadlineTimer = null
    }
    operation.execution.options.signal?.removeEventListener('abort', operation.abortListener)
    this.record(operation, result.outcome, result.error?.code ?? null)
    operation.resolve(result)
  }

  private completeAcknowledged<Value>(operation: PendingOperation<Attachment, Value>): void {
    operation.phase = 'completed'
    this.operations.delete(operation)
    this.removeQueuedOperation(operation)
  }

  private removeQueuedOperation<Value>(operation: PendingOperation<Attachment, Value>): void {
    const queueKey = operation.execution.queueKey
    if (queueKey === null) {
      return
    }
    const queue = this.queues.get(queueKey)
    if (queue === undefined) {
      return
    }
    const lane = queue.lanes.get(operation.fairnessKey)
    if (lane === undefined) {
      return
    }
    const index = lane.indexOf(operation)
    if (index < 0) {
      return
    }
    lane.splice(index, 1)
    if (lane.length === 0) {
      queue.lanes.delete(operation.fairnessKey)
    }
    if (queue.lanes.size === 0) {
      this.queues.delete(queueKey)
      return
    }
    this.pump(queueKey)
  }

  private failure(
    correlation: OperationCorrelation<Attachment, string>,
    outcome: Exclude<CoreOperationOutcome, 'succeeded'>,
    mayCommit: boolean,
    operation: string,
    code: NormalizedBleError['code'] = this.codeForOutcome(outcome)
  ): CoreOperationFailure<Attachment> {
    return {
      correlation,
      outcome,
      value: null,
      error: contractError(code, 'core', operation).normalized,
      commitState: mayCommit && outcome !== 'failed' ? 'unknown' : 'not-applicable'
    }
  }

  private codeForOutcome(outcome: Exclude<CoreOperationOutcome, 'succeeded'>): NormalizedBleError['code'] {
    if (outcome === 'failed') {
      return 'platform.failure'
    }
    if (outcome === 'aborted') {
      return 'operation.aborted'
    }
    if (outcome === 'timed-out') {
      return 'operation.timed-out'
    }
    if (outcome === 'disconnected') {
      return 'operation.disconnected'
    }
    if (outcome === 'reset') {
      return 'operation.reset'
    }
    if (outcome === 'adapter-unavailable') {
      return 'operation.adapter-unavailable'
    }
    return 'operation.cancelled-by-destroy'
  }

  private record<Value>(
    operation: PendingOperation<Attachment, Value>,
    transition: string,
    cause: NormalizedBleError['code'] | null
  ): void {
    const counts = this.activeCounts()
    this.options.trace.record({
      timestamp: this.options.now(),
      resource: 'operation',
      transition,
      operation: operation.traceLabel,
      cause,
      queuedOperations: counts.queued,
      dispatchedOperations: counts.dispatched,
      quarantinedOperations: counts.quarantined
    })
  }
}
