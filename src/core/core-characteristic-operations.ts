// src/core/core-characteristic-operations.ts

import type { BleCentralBackend } from '../backend-contract/backend'
import { BUILT_IN_FEATURE_IDS } from '../backend-contract/capabilities'
import type {
  ConnectionWriteReadinessObservation,
  ConnectionWriteReadinessWatch
} from '../backend-contract/connection-controls'
import { BackendContractError, contractError } from '../backend-contract/errors'
import type { CleanupFailure, CleanupRecord } from '../backend-contract/errors'
import type { CharacteristicPath } from '../backend-contract/gatt'
import type { BackendIdentity } from '../backend-contract/identity'
import type {
  LongWriteChunkProgress,
  LongWriteNotPlannedReceipt,
  LongWritePolicy,
  LongWriteReceipt,
  OperationTerminalRecord,
  PublicOperationOptions,
  WritePolicy,
  WriteReceipt
} from '../backend-contract/operations'
import { deadline, ownBytes, type ByteLimit, type OwnedBytes } from '../backend-contract/primitives'
import type { CoreGattDatabase } from './core-gatt-handles'
import type {
  CoreOperationAdmission,
  CoreOperationCoordinator,
  CoreOperationDispatch,
  CoreOperationResult
} from './operation-coordinator'
import { awaitWithOperationAdmission, coreDispatch, requireOperationValue } from './unified-ble-core-helpers'

type CurrentCharacteristicPath<Attachment extends string> = CharacteristicPath<
  Attachment,
  string,
  string,
  string,
  string,
  'current'
>

const READINESS_OPEN_CLEANUP_GRACE_MS = 1000

interface LongWritePlan {
  readonly maximumWriteLength: number
  readonly totalChunks: number
}

export async function readCoreCharacteristic<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  maximumValueBytes: ByteLimit,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  options: PublicOperationOptions
): Promise<OwnedBytes> {
  database.assertPath(path)
  const result = await operationCoordinator.run({
    queueKey: String(path.connectionId),
    fairnessKey: 'read',
    options,
    mayCommit: false,
    dispatch: correlation => {
      database.assertPath(path)
      const dispatch = backend.gatt.read(path, { operation: { ...options, correlation } })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  const read = requireOperationValue(result, 'unified-core.read')
  return ownBytes(read.value, maximumValueBytes)
}

export async function writeCoreCharacteristic<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  maximumValueBytes: ByteLimit,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  bytes: Readonly<Uint8Array>,
  options: WritePolicy
): Promise<WriteReceipt<Attachment, string>> {
  database.assertPath(path)
  const owned = ownBytes(bytes, maximumValueBytes)
  const result = await operationCoordinator.run({
    queueKey: String(path.connectionId),
    fairnessKey: 'write',
    options,
    mayCommit: true,
    retainedPayloadBytes: owned.byteLength,
    dispatch: correlation => {
      database.assertPath(path)
      const dispatch = backend.gatt.write(path, {
        operation: { ...options, correlation },
        bytes: owned,
        mode: options.mode
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.write')
}

export async function writeCoreCharacteristicWhenReady<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  maximumValueBytes: ByteLimit,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  bytes: Readonly<Uint8Array>,
  options: WritePolicy
): Promise<WriteReceipt<Attachment, string>> {
  if (options.mode !== 'without-response') {
    throw contractError('argument.invalid', 'gatt', 'unified-core.write-when-ready.mode')
  }
  const readinessRegistration = backend.features.registrations.find(
    registration => registration.id === BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness
  )
  if (readinessRegistration?.state === 'unavailable') {
    throw contractError('capability.unavailable', 'connection', 'unified-core.write-when-ready')
  }
  if (
    readinessRegistration === undefined ||
    readinessRegistration.state === 'unsupported' ||
    backend.connections.writeWithoutResponseReadiness === undefined
  ) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.write-when-ready')
  }
  database.assertPath(path)
  const owned = ownBytes(bytes, maximumValueBytes)
  const result = await operationCoordinator.run({
    queueKey: String(path.connectionId),
    fairnessKey: 'write',
    options,
    mayCommit: true,
    retainedPayloadBytes: owned.byteLength,
    admission: () => createWriteReadinessAdmission(database, path, options),
    dispatch: correlation => {
      database.assertPath(path)
      const dispatch = backend.gatt.write(path, {
        operation: { ...options, correlation },
        bytes: owned,
        mode: 'without-response'
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.write-when-ready')
}

interface WriteReadinessWaiter {
  readonly resolve: () => void
  readonly reject: (error: Error) => void
}

function createWriteReadinessAdmission<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  options: WritePolicy
): CoreOperationAdmission {
  let iterator: AsyncIterator<
    import('../backend-contract/streams').StreamItem<ConnectionWriteReadinessObservation<Attachment>>,
    undefined,
    undefined
  > | null = null
  let ready = false
  let closed = false
  let failure: Error | null = null
  let source: ConnectionWriteReadinessWatch<Attachment> | null = null
  let sourceClosePromise: Promise<CleanupRecord> | null = null
  let closePromise: Promise<CleanupRecord> | null = null
  const admissionCancellation = new AbortController()
  const cleanupFailureHandlers = new Set<(failure: CleanupFailure) => void>()
  const waiters = new Set<WriteReadinessWaiter>()

  const rejectWaiters = (error: Error): void => {
    for (const waiter of waiters) waiter.reject(error)
    waiters.clear()
  }
  const fail = (error: Error): void => {
    if (failure !== null || closed) return
    failure = error
    ready = false
    rejectWaiters(error)
  }
  const acceptReady = (): void => {
    for (const waiter of waiters) waiter.resolve()
    waiters.clear()
  }
  const observe = (observation: ConnectionWriteReadinessObservation<Attachment>): void => {
    if (
      String(observation.connectionId) !== String(path.connectionId) ||
      String(observation.connectionGeneration) !== String(path.connectionGeneration)
    ) {
      fail(contractError('protocol.violation', 'connection', 'unified-core.write-when-ready.generation'))
      return
    }
    ready = observation.ready
    if (ready) acceptReady()
  }
  const closeSource = (watch: ConnectionWriteReadinessWatch<Attachment>): Promise<CleanupRecord> => {
    if (sourceClosePromise !== null) return sourceClosePromise
    sourceClosePromise = Promise.resolve()
      .then(() => watch.close())
      .then(
        cleanup => {
          if (cleanup.state === 'release-failed') {
            for (const cleanupFailure of cleanup.failures) {
              for (const handler of cleanupFailureHandlers) handler(cleanupFailure)
            }
          }
          return cleanup
        },
        error => {
          const cleanupFailure: CleanupFailure = {
            resourceKind: 'gatt.write-readiness',
            error:
              error instanceof BackendContractError
                ? error.normalized
                : contractError('platform.failure', 'cleanup', 'unified-core.write-when-ready.close').normalized
          }
          for (const handler of cleanupFailureHandlers) handler(cleanupFailure)
          return {
            state: 'release-failed',
            failures: [cleanupFailure]
          }
        }
      )
    return sourceClosePromise
  }
  const pump = async (watch: ConnectionWriteReadinessWatch<Attachment>): Promise<void> => {
    const nextIterator = watch.events[Symbol.asyncIterator]()
    iterator = nextIterator
    while (!closed && failure === null) {
      const item = await nextIterator.next()
      if (closed || failure !== null) return
      if (item.done) {
        fail(contractError('operation.disconnected', 'connection', 'unified-core.write-when-ready.stream-closed'))
        return
      }
      if (item.value.kind === 'value') {
        observe(item.value.value)
        continue
      }
      if (item.value.kind === 'overflow') {
        fail(contractError('stream.overflow', 'connection', 'unified-core.write-when-ready.stream-overflow'))
        return
      }
      fail(readinessTerminalError(item.value.reason))
      return
    }
  }

  const pendingOpen = Promise.resolve().then(() =>
    database.connection.writeWithoutResponseReadiness({
      signal: admissionCancellation.signal,
      deadline: options.deadline
    })
  )
  pendingOpen
    .then(
      openedSource => {
        source = openedSource
        if (closed) {
          closeSource(openedSource).then(
            () => undefined,
            () => undefined
          )
          return
        }
        pump(openedSource).catch(error => {
          fail(
            error instanceof BackendContractError
              ? error
              : contractError('platform.failure', 'connection', 'unified-core.write-when-ready.readiness-pump')
          )
        })
      },
      error => {
        const normalized =
          error instanceof BackendContractError
            ? error
            : contractError('platform.failure', 'connection', 'unified-core.write-when-ready.readiness-open')
        if (!closed) fail(normalized)
      }
    )
    .catch(() => undefined)
  const boundedOpen = awaitWithOperationAdmission(
    pendingOpen,
    options,
    () => database.monotonicNow(),
    'unified-core.write-when-ready.readiness-open'
  )
  boundedOpen.catch(error => {
    if (!closed) {
      fail(
        error instanceof BackendContractError
          ? error
          : contractError('platform.failure', 'connection', 'unified-core.write-when-ready.readiness-open')
      )
    }
  })

  return {
    waitUntilReady(): Promise<void> {
      if (failure !== null) return Promise.reject(failure)
      if (closed) {
        return Promise.reject(
          contractError('operation.aborted', 'connection', 'unified-core.write-when-ready.admission-closed')
        )
      }
      if (ready) return Promise.resolve()
      return new Promise((resolve, reject) => {
        waiters.add({ resolve, reject })
      })
    },
    isReady(): boolean {
      if (closed || failure !== null) return false
      if (source?.events.isTerminal?.() === true) {
        fail(contractError('operation.disconnected', 'connection', 'unified-core.write-when-ready.stream-closed'))
        return false
      }
      if (!ready) return false
      database.assertPath(path)
      return true
    },
    close(): Promise<CleanupRecord> {
      if (closePromise !== null) return closePromise
      closed = true
      ready = false
      admissionCancellation.abort()
      rejectWaiters(contractError('operation.aborted', 'connection', 'unified-core.write-when-ready.close'))
      if (source === null) {
        const cleanupOpen = awaitWithOperationAdmission(
          pendingOpen,
          {
            signal: null,
            deadline: deadline(database.monotonicNow() + READINESS_OPEN_CLEANUP_GRACE_MS)
          },
          () => database.monotonicNow(),
          'unified-core.write-when-ready.cleanup-open'
        )
        const closing = cleanupOpen
          .then(
            openedSource => {
              source = openedSource
              return closeSource(openedSource)
            },
            error => {
              if (error instanceof BackendContractError && error.normalized.code === 'operation.timed-out') {
                const cleanupFailure: CleanupFailure = {
                  resourceKind: 'gatt.write-readiness-open',
                  error: error.normalized
                }
                for (const handler of cleanupFailureHandlers) handler(cleanupFailure)
                return { state: 'release-failed' as const, failures: [cleanupFailure] }
              }
              return { state: 'released' as const, failures: [] }
            }
          )
          .then(async cleanup => {
            if (iterator?.return !== undefined) await iterator.return()
            return cleanup
          })
        closePromise = closing
        return closing
      }
      const closing = closeSource(source).then(async cleanup => {
        if (iterator?.return !== undefined) await iterator.return()
        return cleanup
      })
      closePromise = closing
      return closing
    },
    onCleanupFailure(handler: (failure: CleanupFailure) => void): void {
      cleanupFailureHandlers.add(handler)
    }
  }
}

function readinessTerminalError(reason: import('../backend-contract/streams').StreamTerminalNotice['reason']): Error {
  if (reason === 'operation-aborted') {
    return contractError('operation.aborted', 'connection', 'unified-core.write-when-ready.readiness-terminal')
  }
  if (reason === 'operation-timed-out') {
    return contractError('operation.timed-out', 'connection', 'unified-core.write-when-ready.readiness-terminal')
  }
  if (reason === 'source-failed' || reason === 'overflow') {
    return contractError('platform.failure', 'connection', 'unified-core.write-when-ready.readiness-terminal')
  }
  return contractError('operation.disconnected', 'connection', 'unified-core.write-when-ready.readiness-terminal')
}

/**
 * Runs one logical long write at the existing connection FIFO head. The backend
 * sees ordinary writes only; each following chunk starts after the preceding
 * completion and after cancellation/deadline/disconnect/destroy boundaries.
 */
export async function writeCoreLongCharacteristic<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  maximumValueBytes: ByteLimit,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  bytes: Readonly<Uint8Array>,
  options: LongWritePolicy,
  planAtFifoHead: () => Promise<LongWritePlan>
): Promise<LongWriteReceipt<Attachment, string>> {
  database.assertPath(path)
  const owned = ownBytes(bytes, maximumValueBytes)
  let progress: MutableLongWriteProgress | null = null
  const result = await operationCoordinator.run({
    queueKey: String(path.connectionId),
    fairnessKey: 'write',
    options,
    mayCommit: true,
    retainedPayloadBytes: owned.byteLength,
    dispatch: correlation =>
      createPlannedSequentialChunkDispatch(
        backend,
        database,
        path,
        owned,
        options,
        planAtFifoHead,
        plan => {
          const nextProgress = createLongWriteProgress(owned.byteLength, plan.maximumWriteLength, plan.totalChunks)
          progress = nextProgress
          return nextProgress
        },
        correlation
      ),
    onQuarantined: () => progress?.markBoundary()
  })
  if (progress === null && result.outcome !== 'succeeded') {
    return receiptBeforeLongWritePlan(result, owned.byteLength)
  }
  if (progress === null) {
    throw contractError('lifecycle.invariant-violation', 'gatt', 'unified-core.write-long.missing-plan')
  }
  return receiptFromLongWriteResult(result, progress)
}

interface MutableLongWriteProgress {
  readonly totalBytes: number
  readonly maximumWriteLength: number
  readonly chunks: LongWriteChunkProgress[]
  activeChunkIndex: number | null
  boundaryReached: boolean
  markBoundary(): void
  markDispatchedChunkUncertain(index: number): void
}

function createLongWriteProgress(
  totalBytes: number,
  maximumWriteLength: number,
  totalChunks: number
): MutableLongWriteProgress {
  if (
    !Number.isSafeInteger(maximumWriteLength) ||
    maximumWriteLength < 1 ||
    !Number.isSafeInteger(totalChunks) ||
    totalChunks < 1 ||
    totalChunks !== Math.max(1, Math.ceil(totalBytes / maximumWriteLength))
  ) {
    throw contractError('protocol.violation', 'gatt', 'unified-core.write-long-plan')
  }
  const chunks: LongWriteChunkProgress[] = []
  for (let index = 0; index < totalChunks; index += 1) {
    const byteOffset = index * maximumWriteLength
    chunks.push({
      index,
      byteOffset,
      byteLength: totalBytes === 0 ? 0 : Math.min(maximumWriteLength, totalBytes - byteOffset),
      state: 'not-started'
    })
  }
  return {
    totalBytes,
    maximumWriteLength,
    chunks,
    activeChunkIndex: null,
    boundaryReached: false,
    markBoundary() {
      this.boundaryReached = true
      if (this.activeChunkIndex !== null) {
        this.markDispatchedChunkUncertain(this.activeChunkIndex)
      }
    },
    markDispatchedChunkUncertain(index: number) {
      const current = this.chunks[index]
      if (current !== undefined && current.state === 'not-started') {
        this.chunks[index] = { ...current, state: 'uncertain' }
      }
      if (this.activeChunkIndex === index) {
        this.activeChunkIndex = null
      }
    }
  }
}

function createPlannedSequentialChunkDispatch<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  bytes: OwnedBytes,
  options: LongWritePolicy,
  planAtFifoHead: () => Promise<LongWritePlan>,
  setProgress: (plan: LongWritePlan) => MutableLongWriteProgress,
  correlation: import('../backend-contract/primitives').OperationCorrelation<Attachment, string>
): CoreOperationDispatch<WriteReceipt<Attachment, string>> {
  let activeChunkDispatch: CoreOperationDispatch<WriteReceipt<Attachment, string>> | null = null
  let cancellationRequested = false
  const completion = planAtFifoHead().then(plan => {
    if (cancellationRequested) {
      throw contractError('operation.aborted', 'gatt', 'unified-core.write-long.cancelled-before-plan')
    }
    database.assertPath(path)
    const progress = setProgress(plan)
    const dispatch = createSequentialChunkDispatch(backend, database, path, bytes, options, progress, correlation)
    activeChunkDispatch = dispatch
    return dispatch.completion
  })
  return {
    completion,
    requestCancellation: (): Promise<void> => {
      cancellationRequested = true
      if (activeChunkDispatch === null) {
        return Promise.resolve()
      }
      return activeChunkDispatch.requestCancellation()
    }
  }
}

function createSequentialChunkDispatch<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  database: CoreGattDatabase<Attachment, Identity>,
  path: CurrentCharacteristicPath<Attachment>,
  bytes: OwnedBytes,
  options: LongWritePolicy,
  progress: MutableLongWriteProgress,
  correlation: import('../backend-contract/primitives').OperationCorrelation<Attachment, string>
): CoreOperationDispatch<WriteReceipt<Attachment, string>> {
  let activeDispatch: CoreOperationDispatch<WriteReceipt<Attachment, string>> | null = null
  let cancellationRequested = false
  let resolveCompletion: (value: WriteReceipt<Attachment, string>) => void = () => undefined
  let rejectCompletion: (reason: Error) => void = () => undefined
  const completion = new Promise<WriteReceipt<Attachment, string>>((resolve, reject) => {
    resolveCompletion = resolve
    rejectCompletion = reject
  })

  const runNext = (index: number): void => {
    if (cancellationRequested || progress.boundaryReached) {
      rejectCompletion(contractError('operation.aborted', 'gatt', 'unified-core.write-long.cancelled-between-chunks'))
      return
    }
    const chunk = progress.chunks[index]
    if (chunk === undefined) {
      resolveCompletion({
        terminal: { correlation, outcome: 'succeeded', cause: null },
        commitState: 'confirmed'
      })
      return
    }
    try {
      database.assertPath(path)
    } catch (error) {
      rejectCompletion(asBackendError(error, 'unified-core.write-long.assert-path'))
      return
    }
    let dispatch: CoreOperationDispatch<WriteReceipt<Attachment, string>>
    try {
      dispatch = coreDispatch(
        backend.gatt.write(path, {
          operation: { signal: null, deadline: null, correlation },
          bytes: bytes.subarray(chunk.byteOffset, chunk.byteOffset + chunk.byteLength),
          mode: options.mode
        }),
        correlation,
        value => value.terminal
      )
    } catch (error) {
      rejectCompletion(asBackendError(error, 'unified-core.write-long.dispatch'))
      return
    }
    activeDispatch = dispatch
    progress.activeChunkIndex = index
    dispatch.completion.then(
      () => {
        if (cancellationRequested || progress.boundaryReached) {
          rejectCompletion(contractError('operation.aborted', 'gatt', 'unified-core.write-long.cancelled-after-chunk'))
          return
        }
        progress.chunks[index] = { ...chunk, state: 'confirmed' }
        progress.activeChunkIndex = null
        runNext(index + 1)
      },
      error => {
        progress.markDispatchedChunkUncertain(index)
        rejectCompletion(asBackendError(error, 'unified-core.write-long.chunk'))
      }
    )
  }
  runNext(0)
  return {
    completion,
    requestCancellation: async (): Promise<void> => {
      cancellationRequested = true
      progress.markBoundary()
      if (activeDispatch === null) {
        return
      }
      await activeDispatch.requestCancellation()
    }
  }
}

function receiptFromLongWriteResult<Attachment extends string>(
  result: CoreOperationResult<Attachment, WriteReceipt<Attachment, string>>,
  progress: MutableLongWriteProgress
): LongWriteReceipt<Attachment, string> {
  const completedChunks = progress.chunks.filter(chunk => chunk.state === 'confirmed').length
  const committedBytes = progress.chunks
    .filter(chunk => chunk.state === 'confirmed')
    .reduce((total, chunk) => total + chunk.byteLength, 0)
  const uncertainChunk = progress.chunks.find(chunk => chunk.state === 'uncertain')
  const failedChunk = uncertainChunk ?? progress.chunks.find(chunk => chunk.state === 'not-started')
  if (result.outcome === 'succeeded') {
    return Object.freeze({
      terminal: terminalFromLongWriteResult(result),
      commitState: 'confirmed',
      planState: 'planned',
      totalBytes: progress.totalBytes,
      chunkSize: progress.maximumWriteLength,
      totalChunks: progress.chunks.length,
      chunks: Object.freeze(progress.chunks.map(chunk => Object.freeze({ ...chunk }))),
      completedChunks,
      committedBytes,
      failedChunkIndex: null
    })
  }
  return Object.freeze({
    terminal: terminalFromLongWriteResult(result),
    commitState: result.commitState === 'unknown' ? 'unknown' : 'confirmed',
    planState: 'planned',
    totalBytes: progress.totalBytes,
    chunkSize: progress.maximumWriteLength,
    totalChunks: progress.chunks.length,
    chunks: Object.freeze(progress.chunks.map(chunk => Object.freeze({ ...chunk }))),
    completedChunks,
    committedBytes,
    failedChunkIndex: failedChunk?.index ?? null
  })
}

function receiptBeforeLongWritePlan<Attachment extends string>(
  result: Exclude<CoreOperationResult<Attachment, WriteReceipt<Attachment, string>>, { readonly outcome: 'succeeded' }>,
  totalBytes: number
): LongWriteNotPlannedReceipt<Attachment, string> {
  return Object.freeze({
    terminal: terminalFromLongWriteResult(result),
    planState: 'not-planned',
    commitState: 'not-started',
    totalBytes,
    chunkSize: 0,
    totalChunks: 0,
    chunks: Object.freeze([]),
    completedChunks: 0,
    committedBytes: 0,
    failedChunkIndex: null
  })
}

function terminalFromLongWriteResult<Attachment extends string>(
  result: CoreOperationResult<Attachment, WriteReceipt<Attachment, string>>
): OperationTerminalRecord<Attachment, string> {
  if (result.outcome === 'succeeded') {
    return Object.freeze({ correlation: result.correlation, outcome: 'succeeded', cause: null })
  }
  return Object.freeze({ correlation: result.correlation, outcome: result.outcome, cause: result.error.code })
}

function asBackendError(error: unknown, operation: string): BackendContractError {
  if (error instanceof BackendContractError) {
    return error
  }
  console.error('[writeCoreLongCharacteristic] Backend chunk operation rejected with a non-contract error:', error)
  return contractError('platform.failure', 'gatt', operation)
}
