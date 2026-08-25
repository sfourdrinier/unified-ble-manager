import * as React from 'react'
import type { ReactNode } from 'react'
import { contractError, type CleanupRecord } from './backend-contract/errors'
import type {
  BleConnection,
  BleConnectionEvent,
  BleManager,
  BlePeer,
  ScanOptions,
  ScanSession
} from './public/ble-manager'
import type { BleAdapterState, BleAdapterStateWatch } from './public/ble-adapter'
import type { GattCharacteristic, GattSubscribeOptions, GattValueEvent } from './public/gatt'
import type { BleCapabilities, CapabilityDescriptor, FeatureId } from './public/capabilities'
import { normalizeScanQuery } from './public/scan-query'
import type { BleReadiness, ExpoBleManager } from './expo'

export type BleManagerFactory = () => Promise<BleManager>

export interface BleProviderProps {
  readonly createManager: BleManagerFactory
  readonly onError?: (error: Error) => void
  readonly children?: ReactNode
}

export interface BleContextValue {
  readonly manager: BleManager | null
  readonly loading: boolean
  readonly error: Error | null
}

export interface UseAdapterStateResult {
  readonly state: BleAdapterState | null
  readonly loading: boolean
  readonly error: Error | null
}

export interface UseBleReadinessResult {
  readonly readiness: BleReadiness | null
  readonly loading: boolean
  readonly error: Error | null
}

export interface UseDiscoveredPeersResult {
  readonly peers: readonly BlePeer[]
  readonly state: 'idle' | 'starting' | 'active' | 'stopped' | 'failed'
  readonly error: Error | null
}

export interface UseConnectionStateResult {
  readonly state: BleConnectionEvent['current'] | null
  readonly loading: boolean
  readonly error: Error | null
}

export interface UseCharacteristicValueResult {
  readonly value: GattValueEvent | null
  readonly loading: boolean
  readonly error: Error | null
}

const missingProviderError = new Error('useBle must be used within a BleProvider')
const expoReadinessUnavailableError = new Error('BLE readiness is available only from an Expo host manager.')
const emptyStoreSubscribe =
  (_listener: StoreListener): (() => void) =>
  () =>
    undefined
const emptyCapabilitySnapshot = (): CapabilityDescriptor | undefined => undefined
const BleContext = React.createContext<BleContextValue | null>(null)
const BleErrorContext = React.createContext<((error: Error) => void) | null>(null)
let pendingManagerRelease: Promise<void> | null = null
let pendingManagerReleaseRetry: (() => void) | null = null

export function BleProvider({ createManager, onError, children }: BleProviderProps): React.ReactElement {
  const [lease] = React.useState(() => new ManagerLease(createManager))
  const [value, setValue] = React.useState<BleContextValue>({ manager: null, loading: true, error: null })

  React.useEffect(() => {
    lease.cancelScheduledRelease()
    let active = true

    const create = async (): Promise<void> => {
      try {
        const manager = await lease.create()
        if (active && lease.isActive()) {
          setValue({ manager, loading: false, error: null })
        }
      } catch (error) {
        const normalized = toError(error)
        if (active && lease.isActive()) {
          setValue({ manager: null, loading: false, error: normalized })
        }
        lease.reportCreateFailure(normalized, onError)
      }
    }

    create().catch(() => undefined)

    return () => {
      active = false
      lease.scheduleRelease(error => lease.reportDestroyFailure(error, onError))
    }
  }, [lease, onError])

  return React.createElement(
    BleErrorContext.Provider,
    { value: onError ?? null },
    React.createElement(BleContext.Provider, { value }, children)
  )
}

export function useBle(): BleContextValue {
  return React.useContext(BleContext) ?? { manager: null, loading: false, error: missingProviderError }
}

export function getAdapterState(manager: Pick<BleManager, 'adapter'>): Promise<BleAdapterState> {
  return manager.adapter.state()
}

export function useAdapterState(): UseAdapterStateResult {
  const ble = useBle()
  const reportError = useReactErrorReporter()
  const emptySnapshot = React.useMemo(
    () => ({ state: null, loading: ble.loading, error: ble.error }),
    [ble.loading, ble.error]
  )
  const store = ble.manager === null ? null : getManagerStore(ble.manager)
  store?.setErrorReporter(reportError)
  const snapshot = React.useSyncExternalStore(
    store?.subscribe ?? emptyStoreSubscribe,
    store?.getAdapterSnapshot ?? (() => emptySnapshot),
    store?.getAdapterSnapshot ?? (() => emptySnapshot)
  )
  return store === null ? emptySnapshot : snapshot
}

export function getBleCapability(
  manager: Pick<BleManager, 'capabilities'>,
  id: FeatureId
): CapabilityDescriptor | undefined {
  return manager.capabilities.get(id)
}

export function useBleCapability(id: FeatureId): CapabilityDescriptor | undefined {
  const { manager } = useBle()
  const reportError = useReactErrorReporter()
  const store = manager === null ? null : getManagerStore(manager).getCapabilityStore(id)
  store?.setErrorReporter(reportError)
  return React.useSyncExternalStore(
    store?.subscribe ?? emptyStoreSubscribe,
    store?.getSnapshot ?? emptyCapabilitySnapshot,
    store?.getSnapshot ?? emptyCapabilitySnapshot
  )
}

export function getBleReadiness(manager: Pick<ExpoBleManager, 'readiness'>): Promise<BleReadiness> {
  return manager.readiness()
}

export function useBleReadiness(): UseBleReadinessResult {
  const { manager, loading, error } = useBle()
  const reportError = useReactErrorReporter()
  const emptySnapshot = React.useMemo(
    () => ({
      readiness: null,
      loading: manager === null ? loading : false,
      error: manager === null ? error : expoReadinessUnavailableError
    }),
    [manager, loading, error]
  )
  const store = manager !== null && hasReadiness(manager) ? getManagerStore(manager) : null
  store?.setErrorReporter(reportError)
  const snapshot = React.useSyncExternalStore(
    store?.subscribe ?? emptyStoreSubscribe,
    store?.getReadinessSnapshot ?? (() => emptySnapshot),
    store?.getReadinessSnapshot ?? (() => emptySnapshot)
  )
  return store === null ? emptySnapshot : snapshot
}

type StoreListener = () => void

type WatchPhase = 'starting' | 'active' | 'stopping' | 'cleanup-failed'

interface WatchRun {
  phase: WatchPhase
  readonly generation: number
  readonly creation: Promise<BleAdapterStateWatch>
  watch: BleAdapterStateWatch | null
  consumption: Promise<void> | null
  stopAttempt: Promise<CleanupRecord> | null
  cleanupFailure: CleanupRecord | Error | null
}

const managerStores = new WeakMap<BleManager, ManagerStore>()
const adapterWatchOwnershipInspectors = new WeakMap<
  BleManager,
  () => {
    readonly runCount: number
    readonly phase: 'idle' | 'starting' | 'active' | 'stopping' | 'cleanup-failed'
    readonly hasWatch: boolean
  }
>()

export function inspectReactAdapterWatchOwnershipForTests(manager: BleManager): {
  readonly runCount: number
  readonly phase: 'idle' | 'starting' | 'active' | 'stopping' | 'cleanup-failed'
  readonly hasWatch: boolean
} {
  const inspect = adapterWatchOwnershipInspectors.get(manager)
  if (inspect === undefined) {
    return { runCount: 0, phase: 'idle', hasWatch: false }
  }
  return inspect()
}

function getManagerStore(manager: BleManager): ManagerStore {
  const existing = managerStores.get(manager)
  if (existing !== undefined) return existing
  const created = new ManagerStore(manager)
  managerStores.set(manager, created)
  return created
}

class ManagerStore {
  private readonly listeners = new Set<StoreListener>()
  private readonly capabilityStores = new Map<FeatureId, CapabilityStore>()
  private watchRun: WatchRun | null = null
  private watchGeneration = 0
  private adapterState: BleAdapterState | null = null
  private errorReporter: (error: Error) => void = () => undefined
  private adapterSnapshot: UseAdapterStateResult = { state: null, loading: true, error: null }
  private readinessSnapshot: UseBleReadinessResult = { readiness: null, loading: true, error: null }
  private readinessRequest = 0

  readonly subscribe = (listener: StoreListener): (() => void) => {
    this.listeners.add(listener)
    if (this.listeners.size === 1) this.ensureWatch()
    let active = true
    return () => {
      if (!active) return
      active = false
      this.listeners.delete(listener)
      if (this.listeners.size === 0) this.stopWatch()
    }
  }

  readonly getAdapterSnapshot = (): UseAdapterStateResult => this.adapterSnapshot

  readonly getReadinessSnapshot = (): UseBleReadinessResult => this.readinessSnapshot

  constructor(private readonly managerInstance: BleManager) {
    adapterWatchOwnershipInspectors.set(managerInstance, () => {
      const run = this.watchRun
      if (run === null) {
        return { runCount: 0, phase: 'idle', hasWatch: false }
      }
      return { runCount: 1, phase: run.phase, hasWatch: run.watch !== null }
    })
  }

  manager(): BleManager {
    return this.managerInstance
  }

  setErrorReporter(reporter: (error: Error) => void): void {
    this.errorReporter = reporter
  }

  getCapabilityStore(id: FeatureId): CapabilityStore {
    const existing = this.capabilityStores.get(id)
    if (existing !== undefined) return existing
    const created = new CapabilityStore(this, id)
    this.capabilityStores.set(id, created)
    return created
  }

  currentState(): BleAdapterState | null {
    return this.adapterState
  }

  setCapabilityReporter(reporter: (error: Error) => void): void {
    this.setErrorReporter(reporter)
  }

  private ensureWatch(): void {
    const existing = this.watchRun
    if (existing === null) {
      this.startWatch()
      return
    }
    if (existing.phase === 'starting' || existing.phase === 'active') {
      return
    }
    if (existing.phase === 'stopping') {
      this.continueAfterStop(existing)
      return
    }
    this.retryFailedCleanup(existing)
  }

  private startWatch(): void {
    this.watchGeneration += 1
    const generation = this.watchGeneration
    const creation = Promise.resolve().then(() => this.managerInstance.adapter.watchState())
    const run: WatchRun = {
      phase: 'starting',
      generation,
      creation,
      watch: null,
      consumption: null,
      stopAttempt: null,
      cleanupFailure: null
    }
    this.watchRun = run
    creation.then(
      watch => {
        if (this.watchRun !== run || run.generation !== generation) {
          this.releaseOrphanedWatch(watch)
          return
        }
        run.watch = watch
        if (run.phase === 'stopping' || run.phase === 'cleanup-failed' || this.listeners.size === 0) {
          this.stopRun(run).then(undefined, error => this.errorReporter(toError(error)))
          return
        }
        run.phase = 'active'
        this.applyState(watch.initial)
        const consumption = this.consumeWatch(run, watch)
        run.consumption = consumption
        consumption.catch(error => {
          if (this.isActive(run)) this.applyError(toError(error))
        })
      },
      error => {
        if (this.watchRun !== run || run.generation !== generation) {
          return
        }
        if (this.listeners.size > 0) {
          this.applyError(toError(error))
        }
        if (this.watchRun === run) {
          this.watchRun = null
        }
      }
    )
  }

  private stopWatch(): void {
    const run = this.watchRun
    if (run === null) {
      return
    }
    if (run.phase === 'stopping' || run.phase === 'cleanup-failed') {
      return
    }
    this.stopRun(run).then(undefined, error => this.errorReporter(toError(error)))
  }

  private continueAfterStop(run: WatchRun): void {
    const attempt = run.stopAttempt ?? this.stopRun(run)
    attempt.then(
      () => {
        if (this.listeners.size === 0) {
          return
        }
        if (this.watchRun === run && run.phase === 'cleanup-failed') {
          this.retryFailedCleanup(run)
          return
        }
        if (this.watchRun === null) {
          this.ensureWatch()
        }
      },
      error => this.errorReporter(toError(error))
    )
  }

  private retryFailedCleanup(run: WatchRun): void {
    if (run.phase !== 'cleanup-failed') {
      return
    }
    this.stopRun(run).then(
      cleanup => {
        if (cleanup.state !== 'released' || this.listeners.size === 0) {
          return
        }
        if (this.watchRun === null) {
          this.ensureWatch()
        }
      },
      error => this.errorReporter(toError(error))
    )
  }

  private stopRun(run: WatchRun): Promise<CleanupRecord> {
    if (run.stopAttempt !== null) {
      return run.stopAttempt
    }
    run.phase = 'stopping'
    const attempt = this.performStop(run).then(cleanup => {
      if (this.watchRun !== run) {
        return cleanup
      }
      if (cleanup.state === 'released') {
        this.watchRun = null
        return cleanup
      }
      run.phase = 'cleanup-failed'
      run.cleanupFailure = cleanup
      run.stopAttempt = null
      reportCleanupFailure(cleanup, this.errorReporter, 'adapter state watch')
      return cleanup
    })
    run.stopAttempt = attempt
    return attempt
  }

  private async performStop(run: WatchRun): Promise<CleanupRecord> {
    let watch = run.watch
    if (watch === null) {
      try {
        watch = await run.creation
        run.watch = watch
      } catch {
        return { state: 'released', failures: [] }
      }
    }
    try {
      return await watch.stop()
    } catch (error) {
      this.errorReporter(toError(error))
      return {
        state: 'release-failed',
        failures: []
      }
    }
  }

  private releaseOrphanedWatch(watch: BleAdapterStateWatch): void {
    watch.stop().then(
      cleanup => {
        if (cleanup.state === 'release-failed') {
          reportCleanupFailure(cleanup, this.errorReporter, 'adapter state watch')
        }
      },
      error => this.errorReporter(toError(error))
    )
  }

  private isActive(run: WatchRun): boolean {
    return this.watchRun === run && (run.phase === 'starting' || run.phase === 'active') && this.listeners.size > 0
  }

  private async consumeWatch(run: WatchRun, watch: BleAdapterStateWatch): Promise<void> {
    try {
      for await (const item of watch.values) {
        if (!this.isActive(run)) {
          return
        }
        if (item.kind === 'terminal') {
          break
        }
        if (item.kind === 'overflow') {
          this.applyError(streamOverflowError('react.adapterStateWatch'))
          continue
        }
        this.applyState(item.value)
      }
    } catch (error) {
      if (this.isActive(run)) {
        this.applyError(toError(error))
      }
    }
    if (this.watchRun !== run) {
      return
    }
    if (run.phase !== 'starting' && run.phase !== 'active') {
      return
    }
    const cleanup = await this.stopRun(run)
    if (cleanup.state === 'released' && this.listeners.size > 0 && this.watchRun === null) {
      this.ensureWatch()
    }
  }

  private applyState(state: BleAdapterState): void {
    this.adapterState = state
    this.adapterSnapshot = { state, loading: false, error: null }
    this.readinessRequest += 1
    this.readinessSnapshot = hasReadiness(this.managerInstance)
      ? { readiness: null, loading: true, error: null }
      : { readiness: null, loading: false, error: expoReadinessUnavailableError }
    this.capabilityStores.forEach(store => store.onManagerStateChanged())
    this.notify()
    this.refreshReadiness(state, this.readinessRequest)
  }

  private applyError(error: Error): void {
    this.readinessRequest += 1
    this.adapterSnapshot = { state: this.adapterState, loading: false, error }
    this.readinessSnapshot = {
      readiness: null,
      loading: false,
      error: hasReadiness(this.managerInstance) ? error : expoReadinessUnavailableError
    }
    this.notify()
  }

  private refreshReadiness(state: BleAdapterState, request: number): void {
    if (!hasReadiness(this.managerInstance)) return
    this.managerInstance.readiness().then(
      readiness => {
        if (!this.isCurrentReadiness(state, request)) return
        this.readinessSnapshot = { readiness, loading: false, error: null }
        this.notify()
      },
      error => {
        if (!this.isCurrentReadiness(state, request)) return
        this.readinessSnapshot = { readiness: null, loading: false, error: toError(error) }
        this.notify()
      }
    )
  }

  private isCurrentReadiness(state: BleAdapterState, request: number): boolean {
    return this.listeners.size > 0 && this.adapterState === state && this.readinessRequest === request
  }

  private notify(): void {
    this.listeners.forEach(listener => listener())
  }
}

class CapabilityStore {
  private readonly listeners = new Set<StoreListener>()
  private ownerUnsubscribe: (() => void) | null = null
  private snapshot: CapabilityDescriptor | undefined
  private initialized = false
  private backendGeneration: string | null = null

  readonly subscribe = (listener: StoreListener): (() => void) => {
    this.listeners.add(listener)
    if (this.listeners.size === 1) this.ownerUnsubscribe = this.owner.subscribe(this.onManagerStateChanged)
    let active = true
    return () => {
      if (!active) return
      active = false
      this.listeners.delete(listener)
      if (this.listeners.size === 0) {
        this.ownerUnsubscribe?.()
        this.ownerUnsubscribe = null
      }
    }
  }

  readonly getSnapshot = (): CapabilityDescriptor | undefined => {
    if (!this.initialized) {
      this.snapshot = getBleCapability(this.owner.manager(), this.id)
      this.backendGeneration = this.owner.currentState()?.backendGeneration ?? null
      this.initialized = true
    }
    return this.snapshot
  }

  constructor(
    private readonly owner: ManagerStore,
    private readonly id: FeatureId
  ) {}

  setErrorReporter(reporter: (error: Error) => void): void {
    this.owner.setCapabilityReporter(reporter)
  }

  onManagerStateChanged = (): void => {
    const state = this.owner.currentState()
    if (state === null) return
    if (this.backendGeneration === null) {
      this.backendGeneration = state.backendGeneration
      return
    }
    if (this.backendGeneration === state.backendGeneration) return
    this.backendGeneration = state.backendGeneration
    if (!this.initialized) return
    this.snapshot = getBleCapability(this.owner.manager(), this.id)
    this.listeners.forEach(listener => listener())
  }
}

export function useDiscoveredPeers(options: ScanOptions = {}): UseDiscoveredPeersResult {
  const { manager } = useBle()
  const reportError = useReactErrorReporter()
  const queryDigest = normalizeScanQuery(options.query).digest
  const signal = options.signal
  const optionsKey = JSON.stringify({
    queryDigest,
    timeoutMs: options.timeoutMs,
    duplicates: options.duplicates,
    delivery: options.delivery,
    observation: options.observation,
    platform: options.platform
  })
  // The normalized digest and primitive controls are semantic dependencies; object identity must not restart a scan.
  // AbortSignal identity is compared directly so mutable signal state does not restart an active scan.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const stableOptions = React.useMemo(() => options, [optionsKey, signal])
  const [result, setResult] = React.useState<UseDiscoveredPeersResult>({
    peers: [],
    state: 'idle',
    error: null
  })

  React.useEffect(() => {
    let active = true
    let session: ScanSession | null = null
    if (manager === null) {
      return () => undefined
    }
    const peers = new Map<string, BlePeer>()
    let overflowError: Error | null = null
    const run = async (): Promise<void> => {
      try {
        session = await manager.scan(stableOptions)
        const current = session
        if (!active) {
          await settleCleanup(() => current.stop(), reportError, 'scan session stop')
          return
        }
        setResult({ peers: [], state: 'active', error: null })
        for await (const item of session.observations) {
          if (!active) return
          if (item.kind === 'terminal') break
          if (item.kind === 'overflow') {
            overflowError = streamOverflowError('react.useDiscoveredPeers.observations')
            setResult({ peers: [...peers.values()], state: 'active', error: overflowError })
            continue
          }
          if (item.kind === 'value') {
            peers.set(item.value.peer.id, item.value.peer)
            setResult({ peers: [...peers.values()], state: 'active', error: overflowError })
          }
        }
        if (active) setResult({ peers: [...peers.values()], state: 'stopped', error: overflowError })
      } catch (reason) {
        if (active) {
          setResult({ peers: [...peers.values()], state: 'failed', error: overflowError ?? toError(reason) })
        }
      }
    }
    run().catch(() => undefined)
    return () => {
      active = false
      const current = session
      if (current !== null) observeCleanup(() => current.stop(), reportError, 'scan session stop')
    }
  }, [manager, optionsKey, signal, stableOptions, reportError])

  return result
}

export function useConnectionState(connection: BleConnection | null): UseConnectionStateResult {
  const reportError = useReactErrorReporter()
  const [result, setResult] = React.useState<UseConnectionStateResult>({
    state: null,
    loading: connection !== null,
    error: null
  })
  const [previousConnection, setPreviousConnection] = React.useState(connection)
  const connectionChanged = previousConnection !== connection
  if (connectionChanged) {
    setPreviousConnection(connection)
    setResult({ state: null, loading: connection !== null, error: null })
  }
  React.useEffect(() => {
    let active = true
    let iterator: AsyncIterator<BleConnectionEvent> | null = null
    if (connection === null) return () => undefined
    const observe = async (): Promise<void> => {
      try {
        const current = connection.lifecycleEvents[Symbol.asyncIterator]()
        iterator = current
        while (true) {
          const next = await current.next()
          if (next.done) break
          const event = next.value
          if (active) setResult({ state: event.current, loading: false, error: null })
        }
      } catch (reason) {
        if (active) setResult({ state: null, loading: false, error: toError(reason) })
      }
    }
    observe().catch(() => undefined)
    return () => {
      active = false
      const current = iterator
      if (current !== null && current.return !== undefined) {
        const close = current.return
        observeRejected(() => close.call(current), reportError)
      }
    }
  }, [connection, connectionChanged, reportError])
  return connection === null
    ? { state: null, loading: false, error: null }
    : connectionChanged
      ? { state: null, loading: true, error: null }
      : result
}

export function useCharacteristicValue(
  characteristic: GattCharacteristic | null,
  options: GattSubscribeOptions = {}
): UseCharacteristicValueResult {
  const reportError = useReactErrorReporter()
  const [result, setResult] = React.useState<UseCharacteristicValueResult>({
    value: null,
    loading: characteristic !== null,
    error: null
  })
  const [previousCharacteristic, setPreviousCharacteristic] = React.useState(characteristic)
  const characteristicChanged = previousCharacteristic !== characteristic
  if (characteristicChanged) {
    setPreviousCharacteristic(characteristic)
    setResult({ value: null, loading: characteristic !== null, error: null })
  }
  const signal = options.signal
  const optionsKey = JSON.stringify({
    timeoutMs: options.timeoutMs,
    delivery: options.delivery,
    stream: options.stream
  })
  // The serialized subscription policy is the semantic dependency; object identity must not restart it.
  // AbortSignal identity is compared directly so mutable signal state does not restart an active subscription.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const stableOptions = React.useMemo(() => options, [optionsKey, signal])
  React.useEffect(() => {
    let active = true
    let subscription: Awaited<ReturnType<GattCharacteristic['subscribe']>> | null = null
    let overflowError: Error | null = null
    let latestValue: GattValueEvent | null = null
    if (characteristic === null) return () => undefined
    const observe = async (): Promise<void> => {
      try {
        subscription = await characteristic.subscribe({
          ...stableOptions,
          stream: stableOptions.stream ?? 'balanced'
        })
        const current = subscription
        if (!active) {
          await settleCleanup(() => current.remove(), reportError, 'characteristic subscription remove')
          return
        }
        for await (const item of subscription.values) {
          if (!active) return
          if (item.kind === 'terminal') break
          if (item.kind === 'overflow') {
            overflowError = streamOverflowError('react.useCharacteristicValue.values')
            setResult({ value: latestValue, loading: false, error: overflowError })
            continue
          }
          if (item.kind === 'value') {
            latestValue = item.value
            setResult({ value: latestValue, loading: false, error: overflowError })
          }
        }
      } catch (reason) {
        if (active) setResult({ value: latestValue, loading: false, error: overflowError ?? toError(reason) })
      }
    }
    observe().catch(() => undefined)
    return () => {
      active = false
      const current = subscription
      if (current !== null) observeCleanup(() => current.remove(), reportError, 'characteristic subscription remove')
    }
  }, [characteristic, characteristicChanged, optionsKey, signal, stableOptions, reportError])
  return characteristic === null
    ? { value: null, loading: false, error: null }
    : characteristicChanged
      ? { value: null, loading: true, error: null }
      : result
}

type CleanupResult = Pick<CleanupRecord, 'state'> & { readonly failures: readonly unknown[] }

function useReactErrorReporter(): (error: Error) => void {
  const callback = React.useContext(BleErrorContext)
  return React.useMemo(
    () => error => {
      if (callback !== null) {
        callback(error)
        return
      }
      if (process.env.NODE_ENV !== 'production') {
        queueMicrotask(() => {
          throw error
        })
      }
    },
    [callback]
  )
}

async function settleCleanup(
  operation: () => PromiseLike<CleanupResult>,
  report: (error: Error) => void,
  resource: string
): Promise<void> {
  let cleanup: CleanupResult
  try {
    cleanup = await operation()
  } catch (error) {
    report(toError(error))
    return
  }
  if (cleanup.state === 'release-failed') {
    reportCleanupFailure(cleanup, report, resource)
  }
}

function observeCleanup(
  operation: () => PromiseLike<CleanupResult>,
  report: (error: Error) => void,
  resource: string
): void {
  settleCleanup(operation, report, resource).then(undefined, error => report(toError(error)))
}

function observeRejected(operation: () => PromiseLike<unknown>, report: (error: Error) => void): void {
  let pending: PromiseLike<unknown>
  try {
    pending = operation()
  } catch (error) {
    report(toError(error))
    return
  }
  Promise.resolve(pending).catch(error => report(toError(error)))
}

function hasReadiness(manager: BleManager): manager is BleManager & Pick<ExpoBleManager, 'readiness'> {
  return 'readiness' in manager && typeof manager.readiness === 'function'
}

class ManagerLease {
  private managerPromise: Promise<BleManager> | null = null
  private releaseScheduled = false
  private releaseInFlight = false
  private releaseAttempted = false
  private released = false
  private releaseBarrier: Promise<void> | null = null
  private resolveReleaseBarrier: (() => void) | null = null
  private createFailureReported = false
  private destroyFailureReported = false

  constructor(private readonly createManager: BleManagerFactory) {}

  create(): Promise<BleManager> {
    if (this.managerPromise === null) {
      const pendingRelease = pendingManagerRelease
      pendingManagerReleaseRetry?.()
      this.managerPromise =
        pendingRelease === null ? this.createManager() : pendingRelease.then(() => this.createManager())
    }
    return this.managerPromise
  }

  isActive(): boolean {
    return !this.released && !this.releaseScheduled
  }

  cancelScheduledRelease(): void {
    if (!this.released && !this.releaseInFlight) this.releaseScheduled = false
  }

  scheduleRelease(report: (error: Error) => void): void {
    if (this.released || this.releaseScheduled || this.releaseInFlight) return
    this.releaseScheduled = true

    if (this.releaseBarrier === null) {
      const releaseBarrier = new Promise<void>(resolve => {
        this.resolveReleaseBarrier = resolve
      })
      this.releaseBarrier = releaseBarrier
      const previousRelease = pendingManagerRelease ?? Promise.resolve()
      const scheduledRelease = previousRelease.then(() => releaseBarrier)
      pendingManagerRelease = scheduledRelease
      scheduledRelease.then(() => {
        if (pendingManagerRelease === scheduledRelease) pendingManagerRelease = null
        if (pendingManagerRelease === null) pendingManagerReleaseRetry = null
        this.releaseBarrier = null
        this.resolveReleaseBarrier = null
      })
    }

    queueMicrotask(() => {
      if (!this.releaseScheduled || this.released) {
        if (!this.releaseAttempted) this.resolveReleaseBarrier?.()
        return
      }
      this.releaseScheduled = false
      this.releaseInFlight = true
      this.releaseAttempted = true
      this.release(report).then(
        succeeded => {
          this.releaseInFlight = false
          if (succeeded) {
            this.released = true
            pendingManagerReleaseRetry = null
            this.resolveReleaseBarrier?.()
          } else {
            pendingManagerReleaseRetry = () => this.scheduleRelease(report)
          }
        },
        () => {
          this.releaseInFlight = false
          pendingManagerReleaseRetry = () => this.scheduleRelease(report)
        }
      )
    })
  }

  reportCreateFailure(error: Error, callback: ((error: Error) => void) | undefined): void {
    if (this.createFailureReported) return
    this.createFailureReported = true
    callback?.(error)
  }

  reportDestroyFailure(error: Error, callback: ((error: Error) => void) | undefined): void {
    if (this.destroyFailureReported) return
    this.destroyFailureReported = true
    callback?.(error)
  }

  private async release(report: (error: Error) => void): Promise<boolean> {
    let manager: BleManager
    try {
      manager = await this.create()
    } catch {
      return true
    }

    try {
      const cleanup = await manager.destroy()
      if (cleanup.state === 'release-failed') {
        reportCleanupFailure(cleanup, report, 'manager destroy')
        return false
      }
      return true
    } catch (error) {
      report(toError(error))
      return false
    }
  }
}

function reportCleanupFailure(cleanup: CleanupResult, report: (error: Error) => void, resource: string): void {
  const error = new Error(`BLE ${resource} reported release-failed`)
  Object.defineProperty(error, 'cleanup', { value: cleanup, enumerable: true })
  report(error)
}

function toError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error))
}

function streamOverflowError(operation: string): Error {
  return contractError('stream.overflow', 'stream', operation)
}

export type { BleAdapterState, BleCapabilities, CapabilityDescriptor, FeatureId }
