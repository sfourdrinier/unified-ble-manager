import { BackendContractError, contractError, type CleanupRecord } from '../backend-contract/errors'
import { capacity } from '../backend-contract/primitives'
import { CoreBoundedStream } from '../core/bounded-stream'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { rehydratePublicError } from './error-bridge'
import { BleError } from './errors'
import type { BleAdapterState } from './ble-adapter'
import type { BleConnection, BleConnectionEvent, BleManager, BlePeer, ConnectOptions } from './ble-manager'
import type { PeerReference } from './peer-reference'
import { encodePeerReference } from './peer-reference'

export type ConnectionSupervisorState =
  | 'idle'
  | 'waiting-for-gate'
  | 'connecting'
  | 'configuring'
  | 'connected'
  | 'disconnecting'
  | 'backoff'
  | 'cleanup-failed'
  | 'stopped'

export interface RetryPolicy {
  readonly initialDelayMs: number
  readonly maximumDelayMs: number
  readonly multiplier: number
  readonly jitter: number
  readonly maximumAttempts?: number
  readonly maximumElapsedMs?: number
}

export interface ConnectionGateContext {
  readonly attempt: number
  readonly adapter: BleAdapterState
  readonly lastError: BleError | null
  readonly lastDisconnect: BleConnectionEvent | null
}

export type ConnectionGateDecision = 'allow' | 'pause' | 'stop'
export type ConnectionGate = (
  context: ConnectionGateContext
) => ConnectionGateDecision | Promise<ConnectionGateDecision>

export interface ConnectionSupervisorEvent<Session> {
  readonly kind: 'state'
  readonly supervisorId: string
  readonly previous: ConnectionSupervisorState
  readonly state: ConnectionSupervisorState
  readonly attempt: number
  readonly connectionGeneration: string | null
  readonly timestamp: number
  readonly delayMs: number | null
  readonly gateDecision: ConnectionGateDecision | null
  readonly error: BleError | null
  readonly session: Session | null
  readonly cleanup?: CleanupRecord
}

export interface ConnectionSupervisorSnapshot<Session> {
  readonly supervisorId: string
  readonly state: ConnectionSupervisorState
  readonly attempt: number
  readonly connectionGeneration: string | null
  readonly session: Session | null
  readonly lastError: BleError | null
  readonly lastDisconnect: BleConnectionEvent | null
}

export interface ConnectionSupervisorOptions<Session> {
  readonly connection?: Omit<ConnectOptions, 'signal'>
  readonly retry: RetryPolicy
  readonly gate?: ConnectionGate
  readonly configure?: (connection: BleConnection) => Promise<Session>
  readonly disposeSession?: (session: Session) => Promise<void>
  readonly resetBackoffAfterConnectedMs?: number
  readonly now?: () => number
  readonly random?: () => number
  readonly setTimeout?: (callback: () => void, delayMs: number) => unknown
  readonly clearTimeout?: (handle: unknown) => void
}

export interface ConnectionSupervisor<Session = undefined> {
  readonly events: BoundedAsyncStream<ConnectionSupervisorEvent<Session>>
  readonly snapshot: ConnectionSupervisorSnapshot<Session>
  start(): void
  pause(reason?: string): Promise<void>
  resume(): void
  reconnectNow(): void
  stop(): Promise<CleanupRecord>
}

const supervisors = new WeakMap<object, Set<string>>()
let nextSupervisorId = 1

export function createConnectionSupervisor<Session = undefined>(
  manager: BleManager,
  peer: BlePeer | string | PeerReference,
  options: ConnectionSupervisorOptions<Session>
): ConnectionSupervisor<Session> {
  validateRetryPolicy(options.retry)
  validateStableConnectionReset(options.resetBackoffAfterConnectedMs)
  const managerSupervisors = supervisors.get(manager)
  const keys = managerSupervisors ?? new Set<string>()
  const peerKey = typeof peer === 'string' ? peer : 'version' in peer ? encodePeerReference(peer) : peer.id
  if (keys.has(peerKey)) {
    throw rehydratePublicError(contractError('connection.already-owned', 'connection', 'connection-supervisor.create'))
  }
  if (managerSupervisors === undefined) supervisors.set(manager, keys)
  keys.add(peerKey)
  const supervisor = new ConnectionSupervisorImpl(manager, peer, options, () => keys.delete(peerKey))
  return supervisor
}

class ConnectionSupervisorImpl<Session> implements ConnectionSupervisor<Session> {
  readonly events: BoundedAsyncStream<ConnectionSupervisorEvent<Session>>
  private readonly eventStream: CoreBoundedStream<ConnectionSupervisorEvent<Session>>
  private state: ConnectionSupervisorState = 'idle'
  private attempt = 0
  private connectionGeneration: string | null = null
  private session: Session | null = null
  private lastError: BleError | null = null
  private lastDisconnect: BleConnectionEvent | null = null
  private startedAt: number | null = null
  private stopRequested = false
  private paused = false
  private activeAbort: AbortController | null = null
  private activeConnection: BleConnection | null = null
  private activeIterator: AsyncIterator<BleConnectionEvent> | null = null
  private retryTimer: unknown = null
  private runPromise: Promise<void> | null = null
  private stopPromise: Promise<CleanupRecord> | null = null
  private cleanupPromise: Promise<CleanupRecord> | null = null
  private lastCleanup: CleanupRecord | null = null
  private stableTimer: unknown = null
  private waitForAdapter = false
  private ownershipReleased = false
  private readonly supervisorAbort = new AbortController()
  private readonly wakeWaiters = new Set<() => void>()
  private controlBarrier: Promise<void> | null = null
  private lateConfigureBarrier: Promise<CleanupRecord> | null = null
  private lateSessionRetry: (() => Promise<CleanupRecord>) | null = null
  private pauseCleanupRequired = false
  private readonly now: () => number
  private readonly random: () => number
  private readonly setTimer: (callback: () => void, delayMs: number) => unknown
  private readonly clearTimer: (handle: unknown) => void

  constructor(
    private readonly manager: BleManager,
    private readonly peer: BlePeer | string | PeerReference,
    private readonly options: ConnectionSupervisorOptions<Session>,
    private readonly unregister: () => void
  ) {
    this.now = options.now ?? (() => globalThis.performance.now())
    this.random = options.random ?? Math.random
    this.setTimer = options.setTimeout ?? ((callback, delayMs) => globalThis.setTimeout(callback, delayMs))
    this.clearTimer =
      options.clearTimeout ??
      (handle => {
        Reflect.apply(globalThis.clearTimeout, globalThis, [handle])
      })
    this.eventStream = new CoreBoundedStream(
      { itemCapacity: capacity(64), byteCapacity: capacity(64 * 1024), reservedControlCapacity: capacity(256) },
      'drop-oldest'
    )
    this.events = this.eventStream
  }

  get snapshot(): ConnectionSupervisorSnapshot<Session> {
    return Object.freeze({
      supervisorId: this.supervisorId,
      state: this.state,
      attempt: this.attempt,
      connectionGeneration: this.connectionGeneration,
      session: this.session,
      lastError: this.lastError,
      lastDisconnect: this.lastDisconnect
    })
  }

  start(): void {
    if (this.state === 'stopped' || this.state === 'cleanup-failed')
      throw rehydratePublicError(contractError('lifecycle.invalid-state', 'connection', 'connection-supervisor.start'))
    if (this.runPromise !== null) return
    this.startedAt = this.now()
    this.runPromise = this.run()
  }

  async pause(_reason?: string): Promise<void> {
    if (this.state === 'stopped') return
    if (this.activeAbort !== null || this.activeConnection !== null || this.session !== null) {
      this.pauseCleanupRequired = true
    }
    this.paused = true
    this.activeAbort?.abort()
    this.wake()
  }

  resume(): void {
    if (this.state === 'stopped') return
    this.paused = false
    this.wake()
  }

  reconnectNow(): void {
    if (this.state !== 'backoff') return
    this.clearRetryTimer()
    this.wake()
  }

  async stop(): Promise<CleanupRecord> {
    if (this.stopPromise !== null) return this.stopPromise
    if (this.lateSessionRetry !== null) {
      const retry = this.lateSessionRetry()
      this.stopPromise = retry.then(cleanup => {
        if (cleanup.state === 'released') this.finalize(cleanup)
        else this.stopPromise = null
        return cleanup
      })
      return this.stopPromise
    }
    this.stopRequested = true
    this.supervisorAbort.abort()
    this.activeAbort?.abort()
    this.clearRetryTimer()
    this.wake()
    const stopOperation = (this.runPromise ?? Promise.resolve()).then(async () => {
      const cleanup = await this.cleanupCurrentConnection()
      this.finalize(cleanup)
      return cleanup
    })
    this.stopPromise = stopOperation.then(
      cleanup => {
        if (cleanup.state === 'release-failed') this.stopPromise = null
        return cleanup
      },
      error => {
        this.stopPromise = null
        throw error
      }
    )
    return this.stopPromise
  }

  private readonly supervisorId = `connection-supervisor-${nextSupervisorId++}`

  private async run(): Promise<void> {
    try {
      while (!this.stopRequested) {
        if (!(await this.awaitLateConfigureCleanup())) break
        const gate = await this.awaitGate()
        if (gate === 'stop' || this.stopRequested) break
        if (gate === 'pause') continue
        const outcome = await this.connectAttempt()
        if (outcome === 'cleanup-failed') {
          this.stopRequested = true
          break
        }
        if (outcome === 'connected') {
          const monitor = await this.monitorConnection()
          if (monitor === 'stopped') break
          if (monitor === 'terminal') {
            this.stopRequested = true
            break
          }
          if (this.pauseCleanupRequired || this.paused) {
            const cleanup = await this.cleanupCurrentConnection()
            if (cleanup.state === 'release-failed') {
              this.lastError = toBleError(cleanup.failures[0]?.error)
              this.stopRequested = true
              break
            }
          }
        }
        if (this.stopRequested) break
        if (!(await this.awaitLateConfigureCleanup())) break
        if (this.waitForAdapter || this.paused) continue
        if (this.attemptLimitReached()) break
        const delayMs = this.nextDelay()
        this.transition('backoff', null, delayMs, null)
        const delayOutcome = await this.waitForDelay(delayMs)
        if (delayOutcome === 'stop') break
      }
    } finally {
      const cleanup = await this.cleanupCurrentConnection()
      this.finalize(cleanup)
    }
  }

  private async awaitGate(): Promise<ConnectionGateDecision> {
    while (!this.stopRequested) {
      if (this.controlBarrier !== null) {
        const barrier = this.controlBarrier
        this.controlBarrier = null
        await barrier
        if (this.stopRequested) return 'stop'
      }
      if (this.paused) {
        this.transition('waiting-for-gate', 'pause', null, null)
        await this.waitForWake()
        continue
      }
      let adapter: BleAdapterState
      try {
        const state = await this.awaitControl(this.manager.adapter.state())
        if (state.kind === 'control') {
          this.deferControl(state.pending)
          return state.reason === 'stop' ? 'stop' : 'pause'
        }
        adapter = state.value
      } catch (error) {
        this.lastError = toBleError(error)
        await this.waitForWake()
        continue
      }
      if (this.waitForAdapter) {
        try {
          const readiness = await this.awaitControl(
            this.manager.adapter.waitUntilReady({ signal: this.supervisorAbort.signal, operation: 'connect' })
          )
          if (readiness.kind === 'control') {
            this.deferControl(readiness.pending)
            return readiness.reason === 'stop' ? 'stop' : 'pause'
          }
          this.waitForAdapter = false
          this.lastError = null
        } catch (error) {
          this.lastError = toBleError(error)
          if (this.stopRequested) return 'stop'
          await this.waitForWake()
          continue
        }
      }
      const decisionResult =
        this.options.gate === undefined
          ? ({ kind: 'value' as const, value: 'allow' as const } satisfies {
              kind: 'value'
              value: ConnectionGateDecision
            })
          : await this.awaitControl(
              Promise.resolve(
                this.options.gate({
                  attempt: this.attempt,
                  adapter,
                  lastError: this.lastError,
                  lastDisconnect: this.lastDisconnect
                })
              )
            )
      if (decisionResult.kind === 'control') {
        this.deferControl(decisionResult.pending)
        return decisionResult.reason === 'stop' ? 'stop' : 'pause'
      }
      const decision = decisionResult.value
      if (decision === 'stop') return 'stop'
      this.transition('waiting-for-gate', decision, null, null)
      if (decision === 'pause') {
        await this.waitForWake()
        continue
      }
      return 'allow'
    }
    return 'stop'
  }

  private async connectAttempt(): Promise<'connected' | 'interrupted' | 'cleanup-failed'> {
    this.attempt += 1
    const token = this.attempt
    const controller = new AbortController()
    this.activeAbort = controller
    this.transition('connecting', null, null, null)
    const connectPromise = this.manager
      .connect(this.peer, { ...this.options.connection, signal: controller.signal })
      .then(connection => ({ kind: 'connected' as const, connection }))
      .catch(error => ({ kind: 'failed' as const, error }))
    const control = this.controlWaiter()
    const outcome = await Promise.race([connectPromise, control.promise.then(() => ({ kind: 'control' as const }))])
    control.cancel()
    if (outcome.kind === 'control') {
      controller.abort()
      await connectPromise.then(async result => {
        if (result.kind === 'connected') {
          this.activeConnection = result.connection
          this.connectionGeneration = extractGeneration(result.connection)
          const cleanup = await this.cleanupCurrentConnection()
          if (cleanup.state === 'release-failed') this.lastError = toBleError(cleanup.failures[0]?.error)
        }
      })
      this.activeAbort = null
      return this.activeConnection === null ? 'interrupted' : 'cleanup-failed'
    }
    this.activeAbort = null
    if (outcome.kind === 'failed') {
      this.lastError = toBleError(outcome.error)
      this.transition('waiting-for-gate', null, null, null)
      if (isAdapterWaitError(this.lastError)) {
        this.attempt -= 1
        this.waitForAdapter = true
      } else if (this.lastError.code === 'permission.not-determined') {
        this.attempt -= 1
        this.paused = true
      } else if (!isRetryableConnectionError(this.lastError)) {
        this.stopRequested = true
      }
      return 'interrupted'
    }
    if (this.stopRequested || this.paused || this.pauseCleanupRequired || token !== this.attempt) {
      this.activeConnection = outcome.connection
      this.connectionGeneration = extractGeneration(outcome.connection)
      const cleanup = await this.cleanupCurrentConnection()
      if (cleanup.state === 'release-failed') {
        this.lastError = toBleError(cleanup.failures[0]?.error)
        return 'cleanup-failed'
      }
      return 'interrupted'
    }
    this.activeConnection = outcome.connection
    this.lastCleanup = null
    this.connectionGeneration = extractGeneration(outcome.connection)
    this.transition('configuring', null, null, null)
    if (this.options.configure !== undefined) {
      try {
        const configurePromise = Promise.resolve(this.options.configure(outcome.connection))
        const configured = await this.awaitControl(configurePromise)
        if (configured.kind === 'control') {
          const lateBarrier = configurePromise
            .then(
              session => this.disposeLateSession(session),
              error => {
                this.lastError = toBleError(error)
                return { state: 'released', failures: [] } satisfies CleanupRecord
              }
            )
            .finally(() => {
              if (this.lateConfigureBarrier === lateBarrier) this.lateConfigureBarrier = null
            })
          this.lateConfigureBarrier = lateBarrier
          lateBarrier
            .then(cleanup => {
              if (!this.stopRequested || this.activeConnection !== null) return
              if (cleanup.state === 'release-failed') {
                this.stopPromise = null
                this.transition('cleanup-failed', null, null, cleanup)
                return
              }
              this.finalize(cleanup)
            })
            .catch(error => {
              this.lastError = toBleError(error)
              this.stopRequested = true
              this.wake()
            })
          const cleanup = await this.cleanupCurrentConnection()
          if (cleanup.state === 'release-failed') {
            this.lastError = toBleError(cleanup.failures[0]?.error)
            return 'cleanup-failed'
          }
          return 'interrupted'
        }
        this.session = configured.value
      } catch (error) {
        this.lastError = toBleError(error)
        const cleanup = await this.cleanupCurrentConnection()
        if (cleanup.state === 'release-failed') {
          this.lastError = toBleError(cleanup.failures[0]?.error)
          return 'cleanup-failed'
        }
        return 'interrupted'
      }
    }
    this.lastError = null
    this.startStableResetTimer()
    this.transition('connected', 'allow', null, null)
    return 'connected'
  }

  private async monitorConnection(): Promise<'retry' | 'stopped' | 'terminal'> {
    const connection = this.activeConnection
    if (connection === null) return 'terminal'
    const iterator = connection.lifecycleEvents[Symbol.asyncIterator]()
    this.activeIterator = iterator
    while (!this.stopRequested) {
      const control = this.controlWaiter()
      let next: Awaited<ReturnType<typeof this.connectionEventRace>>
      try {
        next = await this.connectionEventRace(iterator, control.promise)
      } catch (error) {
        control.cancel()
        this.lastError = toBleError(error)
        return 'terminal'
      }
      control.cancel()
      if (next.kind === 'control') return this.stopRequested ? 'stopped' : 'retry'
      if (next.result.done) {
        this.lastError = toBleError(contractError('stream.closed', 'connection', 'connection-supervisor.events'))
        return 'terminal'
      }
      const event = next.result.value
      if (event.current === 'disconnecting') this.transition('disconnecting', null, null, null)
      if (event.current === 'disconnected' || event.current === 'lost') {
        this.lastDisconnect = event
        this.transition('disconnecting', null, null, null)
        const cleanup = await this.cleanupCurrentConnection()
        if (cleanup.state === 'release-failed') {
          this.lastError = toBleError(contractError('connection.failed', 'connection', 'connection-supervisor.cleanup'))
          return 'terminal'
        }
        return 'retry'
      }
    }
    return 'stopped'
  }

  private async cleanupCurrentConnection(): Promise<CleanupRecord> {
    if (this.cleanupPromise !== null) return this.cleanupPromise
    const connection = this.activeConnection
    const iterator = this.activeIterator
    const session = this.session
    if (connection === null && iterator === null && session === null && this.lastCleanup !== null)
      return this.lastCleanup
    this.clearStableResetTimer()
    const cleanupPromise: Promise<CleanupRecord> = (async (): Promise<CleanupRecord> => {
      const failures: CleanupRecord['failures'][number][] = []
      if (iterator !== null && iterator.return !== undefined) {
        try {
          await iterator.return()
          this.activeIterator = null
        } catch (error) {
          failures.push(...cleanupFailure('connection-events', error, 'connection-supervisor.events-return'))
        }
      }
      if (session !== null && this.options.disposeSession === undefined) {
        this.session = null
      } else if (session !== null && this.options.disposeSession !== undefined) {
        try {
          await this.options.disposeSession(session)
          this.session = null
        } catch (error) {
          failures.push(...cleanupFailure('session', error, 'connection-supervisor.session-dispose'))
          this.lastError = toBleError(error)
        }
      }
      if (connection !== null) {
        try {
          const cleanup = await connection.release()
          failures.push(...cleanup.failures)
          if (cleanup.state === 'released') {
            this.activeConnection = null
            this.connectionGeneration = null
          }
        } catch (error) {
          failures.push(...cleanupFailure('connection', error, 'connection-supervisor.connection-release'))
        }
      }
      return failures.length === 0
        ? ({ state: 'released', failures: [] } satisfies CleanupRecord)
        : { state: 'release-failed', failures }
    })().finally(() => {
      this.cleanupPromise = null
    })
    this.cleanupPromise = cleanupPromise
    const cleanup = await cleanupPromise
    this.lastCleanup = cleanup
    if (cleanup.state === 'released') this.pauseCleanupRequired = false
    return cleanup
  }

  private async disposeLateSession(session: Session): Promise<CleanupRecord> {
    if (this.options.disposeSession === undefined) return { state: 'released', failures: [] }
    try {
      await this.options.disposeSession(session)
      return { state: 'released', failures: [] }
    } catch (error) {
      this.lastError = toBleError(error)
      const cleanup: CleanupRecord = {
        state: 'release-failed',
        failures: cleanupFailure('session', error, 'connection-supervisor.late-session-dispose')
      }
      this.lastCleanup = cleanup
      this.lateSessionRetry = async () => {
        try {
          await this.options.disposeSession?.(session)
          this.lateSessionRetry = null
          this.lastCleanup = { state: 'released', failures: [] }
          return this.lastCleanup
        } catch (retryError) {
          this.lastError = toBleError(retryError)
          const retryCleanup: CleanupRecord = {
            state: 'release-failed',
            failures: cleanupFailure('session', retryError, 'connection-supervisor.late-session-dispose-retry')
          }
          this.lastCleanup = retryCleanup
          return retryCleanup
        }
      }
      if (this.stopRequested && this.activeConnection === null) this.transition('cleanup-failed', null, null, cleanup)
      return cleanup
    }
  }

  private finalize(cleanup: CleanupRecord): void {
    if (cleanup.state === 'released' && (this.lateConfigureBarrier !== null || this.lateSessionRetry !== null)) {
      this.transition('stopped', null, null, cleanup)
      return
    }
    if (cleanup.state === 'released') {
      if (this.ownershipReleased) return
      this.ownershipReleased = true
      this.transition('stopped', null, null, cleanup)
      this.eventStream.closeWithReason('closed')
      this.unregister()
      return
    }
    this.lastError = toBleError(cleanup.failures[0]?.error)
    this.transition('cleanup-failed', null, null, cleanup)
  }

  private waitForDelay(delayMs: number): Promise<'elapsed' | 'stop'> {
    if (delayMs <= 0) return Promise.resolve(this.stopRequested ? 'stop' : 'elapsed')
    return new Promise(resolve => {
      const control = this.controlWaiter()
      this.retryTimer = this.setTimer(() => {
        control.cancel()
        this.retryTimer = null
        resolve(this.stopRequested ? 'stop' : 'elapsed')
      }, delayMs)
      control.promise.then(() => {
        if (this.retryTimer !== null) this.clearRetryTimer()
        resolve(this.stopRequested ? 'stop' : 'elapsed')
      })
    })
  }

  private nextDelay(): number {
    const { initialDelayMs, maximumDelayMs, multiplier, jitter } = this.options.retry
    const base = Math.min(maximumDelayMs, initialDelayMs * multiplier ** Math.max(0, this.attempt - 1))
    const variation = base * jitter
    return Math.max(0, Math.round(base - variation + this.random() * variation * 2))
  }

  private attemptLimitReached(): boolean {
    if (this.options.retry.maximumAttempts !== undefined && this.attempt >= this.options.retry.maximumAttempts)
      return true
    return (
      this.options.retry.maximumElapsedMs !== undefined &&
      this.startedAt !== null &&
      this.now() - this.startedAt >= this.options.retry.maximumElapsedMs
    )
  }

  private transition(
    state: ConnectionSupervisorState,
    gateDecision: ConnectionGateDecision | null,
    delayMs: number | null,
    cleanup: CleanupRecord | null
  ): void {
    const previous = this.state
    this.state = state
    this.eventStream.emit(
      Object.freeze({
        kind: 'state',
        supervisorId: this.supervisorId,
        previous,
        state,
        attempt: this.attempt,
        connectionGeneration: this.connectionGeneration,
        timestamp: this.now(),
        delayMs,
        gateDecision,
        error: this.lastError,
        session: this.session,
        ...(cleanup === null ? {} : { cleanup })
      }),
      512
    )
  }

  private controlWaiter(): { promise: Promise<void>; cancel: () => void } {
    let resolveWaiter: (() => void) | null = null
    const promise = new Promise<void>(resolve => {
      resolveWaiter = resolve
      this.wakeWaiters.add(resolve)
    })
    return {
      promise,
      cancel: () => {
        if (resolveWaiter !== null) this.wakeWaiters.delete(resolveWaiter)
      }
    }
  }

  private async awaitControl<Value>(
    pending: Promise<Value>
  ): Promise<{ kind: 'value'; value: Value } | { kind: 'control'; reason: 'stop' | 'wake'; pending: Promise<void> }> {
    const control = this.controlWaiter()
    const pendingBarrier = pending.then(
      () => undefined,
      () => undefined
    )
    const result = await Promise.race([
      pending.then(value => ({ kind: 'value' as const, value })),
      control.promise.then(() => ({
        kind: 'control' as const,
        reason: this.stopRequested ? ('stop' as const) : ('wake' as const),
        pending: pendingBarrier
      }))
    ])
    control.cancel()
    return result
  }

  private deferControl(pending: Promise<void>): void {
    this.controlBarrier = pending
  }

  private async awaitLateConfigureCleanup(): Promise<boolean> {
    if (this.lateConfigureBarrier === null) return true
    const cleanup = await this.lateConfigureBarrier
    if (cleanup.state === 'release-failed') {
      this.lastError = toBleError(cleanup.failures[0]?.error)
      this.stopRequested = true
      return false
    }
    return true
  }

  private waitForWake(): Promise<void> {
    return this.controlWaiter().promise
  }

  private wake(): void {
    for (const resolve of [...this.wakeWaiters]) resolve()
    this.wakeWaiters.clear()
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== null) {
      this.clearTimer(this.retryTimer)
      this.retryTimer = null
    }
  }

  private async connectionEventRace(
    iterator: AsyncIterator<BleConnectionEvent>,
    controlPromise: Promise<void>
  ): Promise<{ kind: 'event'; result: IteratorResult<BleConnectionEvent> } | { kind: 'control' }> {
    return Promise.race([
      iterator.next().then(result => ({ kind: 'event' as const, result })),
      controlPromise.then(() => ({ kind: 'control' as const }))
    ])
  }

  private startStableResetTimer(): void {
    this.clearStableResetTimer()
    const delay = this.options.resetBackoffAfterConnectedMs
    if (delay === undefined) return
    this.stableTimer = this.setTimer(() => {
      this.stableTimer = null
      if (this.state === 'connected') this.attempt = 0
    }, delay)
  }

  private clearStableResetTimer(): void {
    if (this.stableTimer !== null) {
      this.clearTimer(this.stableTimer)
      this.stableTimer = null
    }
  }
}

function extractGeneration(connection: BleConnection): string | null {
  return connection.connectionGeneration
}

function toBleError(error: unknown): BleError {
  const publicError = rehydratePublicError(error)
  if (publicError instanceof BleError) return publicError
  return new BleError('connection.failed', 'connection', 'connection-supervisor.attempt')
}

function cleanupFailure(resourceKind: string, error: unknown, operation: string): CleanupRecord['failures'] {
  const publicError = rehydratePublicError(error)
  const normalized =
    publicError instanceof BleError
      ? contractError(publicError.code, publicError.domain, publicError.operation, publicError.platform).normalized
      : error instanceof BackendContractError
        ? error.normalized
        : contractError('platform.failure', 'cleanup', operation).normalized
  return [{ resourceKind, error: normalized }]
}

function isAdapterWaitError(error: BleError): boolean {
  return (
    error.code === 'adapter.unavailable' || error.code === 'adapter.powered-off' || error.code === 'adapter.resetting'
  )
}

function isRetryableConnectionError(error: BleError): boolean {
  return (
    error.code === 'backend.reset' ||
    error.code === 'connection.failed' ||
    error.code === 'connection.lost' ||
    error.code === 'connection.stale' ||
    error.code === 'operation.timed-out' ||
    error.code === 'platform.failure' ||
    error.code === 'platform.transport'
  )
}

function validateRetryPolicy(policy: RetryPolicy): void {
  if (
    !Number.isFinite(policy.initialDelayMs) ||
    policy.initialDelayMs < 0 ||
    !Number.isFinite(policy.maximumDelayMs) ||
    policy.maximumDelayMs < policy.initialDelayMs ||
    !Number.isFinite(policy.multiplier) ||
    policy.multiplier < 1 ||
    !Number.isFinite(policy.jitter) ||
    policy.jitter < 0 ||
    policy.jitter > 1 ||
    (policy.maximumAttempts !== undefined &&
      (!Number.isSafeInteger(policy.maximumAttempts) || policy.maximumAttempts < 1)) ||
    (policy.maximumElapsedMs !== undefined &&
      (!Number.isSafeInteger(policy.maximumElapsedMs) || policy.maximumElapsedMs <= 0))
  ) {
    throw rehydratePublicError(contractError('argument.invalid', 'connection', 'connection-supervisor.retry'))
  }
}

function validateStableConnectionReset(value: number | undefined): void {
  if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) {
    throw rehydratePublicError(contractError('argument.invalid', 'connection', 'connection-supervisor.reset-backoff'))
  }
}
