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

interface WatchRun {
  readonly promise: Promise<BleAdapterStateWatch>
  watch: BleAdapterStateWatch | null
  stopped: boolean
  stopPromise: Promise<void> | null
  restartScheduled: boolean
}

const managerStores = new WeakMap<BleManager, ManagerStore>()

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
  private adapterState: BleAdapterState | null = null
  private errorReporter: (error: Error) => void = () => undefined
  private adapterSnapshot: UseAdapterStateResult = { state: null, loading: true, error: null }
  private readinessSnapshot: UseBleReadinessResult = { readiness: null, loading: true, error: null }

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

  constructor(private readonly managerInstance: BleManager) {}

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
    if (existing !== null) {
      if (!existing.stopped) return
      if (existing.stopPromise !== null) {
        if (!existing.restartScheduled) {
          existing.restartScheduled = true
          existing.stopPromise.then(() => {
            if (this.watchRun !== existing || this.listeners.size === 0) return
            this.watchRun = null
            this.ensureWatch()
          })
        }
        return
      }
      existing.stopped = false
      return
    }

    const promise = Promise.resolve().then(() => this.managerInstance.adapter.watchState())
    const run: WatchRun = {
      promise,
      watch: null,
      stopped: false,
      stopPromise: null,
      restartScheduled: false
    }
    this.watchRun = run
    promise.then(
      watch => {
        run.watch = watch
        if (!this.isActive(run)) {
          this.stopResolvedWatch(run, watch)
          return
        }
        this.applyState(watch.initial)
        this.consumeWatch(run, watch).catch(error => {
          if (this.isActive(run)) this.applyError(toError(error))
        })
      },
      error => {
        if (this.isActive(run)) this.applyError(toError(error))
        else if (this.watchRun === run) this.watchRun = null
      }
    )
  }

  private stopWatch(): void {
    const run = this.watchRun
    if (run === null || run.stopped) return
    run.stopped = true
    if (run.watch !== null) {
      this.stopResolvedWatch(run, run.watch)
      return
    }
    run.promise
      .then(watch => {
        run.watch = watch
        if (run.stopped) this.stopResolvedWatch(run, watch)
      })
      .catch(() => undefined)
  }

  private stopResolvedWatch(run: WatchRun, watch: BleAdapterStateWatch): void {
    if (run.stopPromise !== null) return
    run.stopPromise = settleCleanup(() => watch.stop(), this.errorReporter, 'adapter state watch')
    run.stopPromise.then(() => {
      if (this.watchRun === run && run.stopped && this.listeners.size === 0) this.watchRun = null
    })
  }

  private isActive(run: WatchRun): boolean {
    return this.watchRun === run && !run.stopped && this.listeners.size > 0
  }

  private async consumeWatch(run: WatchRun, watch: BleAdapterStateWatch): Promise<void> {
    for await (const item of watch.values) {
      if (!this.isActive(run)) return
      if (item.kind === 'terminal') return
      if (item.kind === 'overflow') {
        this.applyError(streamOverflowError('react.adapterStateWatch'))
        continue
      }
      this.applyState(item.value)
    }
  }

  private applyState(state: BleAdapterState): void {
    this.adapterState = state
    this.adapterSnapshot = { state, loading: false, error: null }
    this.readinessSnapshot = {
      readiness: projectExpoReadiness(state),
      loading: false,
      error: null
    }
    this.capabilityStores.forEach(store => store.onManagerStateChanged())
    this.notify()
  }

  private applyError(error: Error): void {
    this.adapterSnapshot = { state: this.adapterState, loading: false, error }
    this.readinessSnapshot = {
      readiness: this.adapterState === null ? null : projectExpoReadiness(this.adapterState),
      loading: false,
      error
    }
    this.notify()
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

function projectExpoReadiness(adapter: BleAdapterState): BleReadiness {
  if (
    adapter.availability !== 'available' ||
    adapter.authorization === 'restricted' ||
    adapter.authorization === 'unavailable' ||
    adapter.power === 'unsupported'
  ) {
    return createReadiness(adapter, 'unavailable', [])
  }
  if (adapter.authorization === 'denied') {
    return createReadiness(adapter, 'action-required', [{ kind: 'open-settings', target: 'app' }])
  }
  if (adapter.authorization === 'not-determined') {
    return createReadiness(adapter, 'action-required', [{ kind: 'request-permission', permission: 'bluetooth' }])
  }
  if (adapter.power === 'off') {
    return createReadiness(adapter, 'action-required', [{ kind: 'enable-bluetooth', systemUiOnly: true }])
  }
  if (adapter.power !== 'on' || adapter.authorization !== 'granted') {
    return createReadiness(adapter, 'action-required', [])
  }
  return createReadiness(adapter, 'ready', [])
}

function createReadiness(
  adapter: BleAdapterState,
  state: BleReadiness['state'],
  actions: BleReadiness['actions']
): BleReadiness {
  return Object.freeze({ adapter, state, actions: Object.freeze([...actions]) })
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
