import * as React from 'react'
import type { ReactNode } from 'react'
import { contractError, type CleanupRecord } from './backend-contract/errors'
import type { StreamItem, StreamTerminalNotice } from './backend-contract/streams'
import {
  connectionEventsEndedExpectedly,
  type BleConnection,
  type BleConnectionEvent,
  type BleManager,
  type BlePeer,
  type DiscoveryEvent,
  type PublicScanObservation,
  type ScanOptions,
  type ScanSession
} from './public/ble-manager'
import {
  MAX_PUBLIC_SCAN_STATE_BYTES,
  MAX_PUBLIC_SCAN_STATE_ENTRIES,
  estimatePublicPeerRetentionBytes
} from './public/scan-state-budget'
import type { BleAdapterState, BleAdapterStateWatch } from './public/ble-adapter'
import type { GattCharacteristic, GattSubscribeOptions, GattSubscription, GattValueEvent } from './public/gatt'
import type { BleCapabilities, CapabilityDescriptor, FeatureId } from './public/capabilities'
import { adapterWatchOwnershipInspectors } from './public/react-adapter-watch-inspect'
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
  const generationRef = React.useRef(0)
  const [result, setResult] = React.useState<UseDiscoveredPeersResult>({
    peers: [],
    state: 'idle',
    error: null
  })

  React.useEffect(() => {
    let active = true
    const runGeneration = generationRef.current + 1
    generationRef.current = runGeneration
    if (manager === null) {
      return () => undefined
    }
    const peers = new Map<string, { peer: BlePeer; bytes: number }>()
    let retainedBytes = 0
    let overflowError: Error | null = null
    let consumeError: Error | null = null
    let session: ScanSession | null = null
    let observationIterator: AsyncIterator<StreamItem<PublicScanObservation>> | null = null
    let eventIterator: AsyncIterator<DiscoveryEvent> | null = null
    let observationReturned = false
    let eventsReturned = false
    let sessionReleased = false
    let stopAttempt: Promise<void> | null = null
    const eventsAuthoritative = (): boolean => eventIterator !== null

    const isCurrentGeneration = (): boolean => generationRef.current === runGeneration

    const snapshotPeers = (): BlePeer[] => {
      const next: BlePeer[] = []
      for (const entry of peers.values()) next.push(entry.peer)
      return next
    }

    const publish = (state: UseDiscoveredPeersResult['state'], error: Error | null): void => {
      if (!isCurrentGeneration()) return
      setResult({ peers: snapshotPeers(), state, error })
    }

    const evict = (): void => {
      while (peers.size > MAX_PUBLIC_SCAN_STATE_ENTRIES || retainedBytes > MAX_PUBLIC_SCAN_STATE_BYTES) {
        const oldest = peers.entries().next()
        if (oldest.done) return
        const [oldestId, entry] = oldest.value
        peers.delete(oldestId)
        retainedBytes -= entry.bytes
        overflowError = streamOverflowError('react.useDiscoveredPeers.cap')
      }
    }

    const upsert = (peer: BlePeer): void => {
      const existing = peers.get(peer.id)
      if (existing !== undefined) {
        peers.delete(peer.id)
        retainedBytes -= existing.bytes
      }
      const bytes = estimatePublicPeerRetentionBytes(peer)
      peers.set(peer.id, { peer, bytes })
      retainedBytes += bytes
      evict()
    }

    const refreshIfPresent = (peer: BlePeer): void => {
      if (!peers.has(peer.id)) return
      upsert(peer)
    }

    const removePeer = (id: string): void => {
      const existing = peers.get(id)
      if (existing === undefined) return
      peers.delete(id)
      retainedBytes -= existing.bytes
    }

    const returnIterator = async (
      iterator: { return?: () => PromiseLike<unknown> | unknown } | null,
      complete: () => boolean,
      markComplete: () => void
    ): Promise<void> => {
      if (complete() || iterator === null) return
      const close = iterator.return
      if (close === undefined) {
        markComplete()
        return
      }
      try {
        await close.call(iterator)
        markComplete()
      } catch (reason) {
        reportError(toError(reason))
      }
    }

    const stopRun = (): Promise<void> => {
      if (stopAttempt !== null) return stopAttempt
      const attempt = (async () => {
        await Promise.all([
          returnIterator(
            observationIterator,
            () => observationReturned,
            () => {
              observationReturned = true
            }
          ),
          returnIterator(
            eventIterator,
            () => eventsReturned,
            () => {
              eventsReturned = true
            }
          )
        ])
        if (!sessionReleased && session !== null) {
          const currentSession = session
          try {
            const cleanup = await currentSession.stop()
            if (cleanup.state === 'release-failed') {
              reportCleanupFailure(cleanup, reportError, 'scan session stop')
            } else {
              sessionReleased = true
            }
          } catch (reason) {
            reportError(toError(reason))
          }
        }
        peers.clear()
        retainedBytes = 0
      })()
      stopAttempt = attempt.finally(() => {
        if (stopAttempt === attempt) stopAttempt = null
      })
      return stopAttempt
    }

    const consumeObservations = async (): Promise<void> => {
      if (observationIterator === null) return
      while (true) {
        const next = await observationIterator.next()
        if (next.done || !active) return
        const item = next.value
        if (item.kind === 'terminal') return
        if (item.kind === 'overflow') {
          overflowError = streamOverflowError('react.useDiscoveredPeers.observations')
          publish('active', overflowError)
          continue
        }
        if (item.kind === 'value') {
          if (eventsAuthoritative()) refreshIfPresent(item.value.peer)
          else upsert(item.value.peer)
          if (active) publish('active', overflowError)
        }
      }
    }

    const consumeEvents = async (): Promise<void> => {
      if (eventIterator === null) {
        eventsReturned = true
        return
      }
      while (true) {
        const next = await eventIterator.next()
        if (next.done || !active) return
        const event = next.value
        if (event.kind === 'observed') upsert(event.peer)
        else if (event.kind === 'lost') removePeer(event.peer.id)
        if (active) publish('active', overflowError)
      }
    }

    const run = async (): Promise<void> => {
      try {
        session = await manager.scan(stableOptions)
        observationIterator = session.observations[Symbol.asyncIterator]()
        eventIterator =
          session.events === undefined || session.events === null ? null : session.events[Symbol.asyncIterator]()
        if (eventIterator === null) eventsReturned = true
        if (!active) {
          await stopRun()
          return
        }
        publish('active', null)
        await Promise.all([consumeObservations(), consumeEvents()])
        await stopRun()
        if (isCurrentGeneration()) {
          setResult({
            peers: [],
            state: consumeError === null ? 'stopped' : 'failed',
            error: overflowError ?? consumeError
          })
        }
      } catch (reason) {
        consumeError = overflowError ?? toError(reason)
        if (isCurrentGeneration()) {
          setResult({ peers: snapshotPeers(), state: 'failed', error: consumeError })
        }
        await stopRun()
      }
    }
    run().catch(() => undefined)
    return () => {
      active = false
      observeRejected(
        () =>
          stopRun().then(() => {
            if (!isCurrentGeneration()) return
            setResult({ peers: [], state: 'idle', error: overflowError ?? consumeError })
          }),
        reportError
      )
    }
  }, [manager, optionsKey, signal, stableOptions, reportError])

  return result
}

export function useConnectionState(connection: BleConnection | null): UseConnectionStateResult {
  const reportError = useReactErrorReporter()
  const generationRef = React.useRef(0)
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
    const runGeneration = generationRef.current + 1
    generationRef.current = runGeneration
    let iterator: AsyncIterator<BleConnectionEvent> | null = null
    if (connection === null) return () => undefined
    const isCurrent = (): boolean => active && generationRef.current === runGeneration
    const observe = async (): Promise<void> => {
      try {
        const current = connection.lifecycleEvents[Symbol.asyncIterator]()
        iterator = current
        while (true) {
          const next = await current.next()
          if (!isCurrent()) return
          if (next.done) {
            const expected = connectionEventsEndedExpectedly(connection.lifecycleEvents)
            setResult(currentResult => ({
              state: currentResult.state,
              loading: false,
              error: expected ? null : contractError('stream.closed', 'stream', 'react.useConnectionState')
            }))
            break
          }
          setResult({ state: next.value.current, loading: false, error: null })
        }
      } catch (reason) {
        if (!isCurrent()) return
        setResult(currentResult => ({
          state: currentResult.state,
          loading: false,
          error: toError(reason)
        }))
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
  const generationRef = React.useRef(0)
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
    const runGeneration = generationRef.current + 1
    generationRef.current = runGeneration
    let subscription: GattSubscription | null = null
    let overflowError: Error | null = null
    let latestValue: GattValueEvent | null = null
    let valueIterator: AsyncIterator<StreamItem<GattValueEvent>> | null = null
    let removeAttempt: Promise<CleanupResult> | null = null
    let removeReleased = false
    if (characteristic === null) return () => undefined
    const isCurrent = (): boolean => generationRef.current === runGeneration

    const publish = (next: UseCharacteristicValueResult): void => {
      if (!isCurrent()) return
      setResult(next)
    }

    const removeSubscription = (): Promise<void> => {
      if (removeReleased) return Promise.resolve()
      if (removeAttempt !== null) {
        return removeAttempt.then(
          () => undefined,
          () => undefined
        )
      }
      const current = subscription
      if (current === null) return Promise.resolve()
      const attempt = (async (): Promise<CleanupResult> => {
        try {
          const cleanup = await current.remove()
          if (cleanup.state === 'release-failed') {
            reportCleanupFailure(cleanup, reportError, 'characteristic subscription remove')
            return cleanup
          }
          removeReleased = true
          return cleanup
        } catch (reason) {
          reportError(toError(reason))
          throw reason
        }
      })()
      removeAttempt = attempt.finally(() => {
        if (!removeReleased) removeAttempt = null
      })
      return attempt.then(
        () => undefined,
        () => undefined
      )
    }

    const observe = async (): Promise<void> => {
      try {
        subscription = await characteristic.subscribe({
          ...stableOptions,
          stream: stableOptions.stream ?? 'balanced'
        })
        if (!active) {
          await removeSubscription()
          return
        }
        valueIterator = subscription.values[Symbol.asyncIterator]()
        while (true) {
          const next = await valueIterator.next()
          if (!active) return
          if (next.done) {
            publish({
              value: latestValue,
              loading: false,
              error: overflowError ?? contractError('stream.closed', 'stream', 'react.useCharacteristicValue')
            })
            await removeSubscription()
            return
          }
          const item = next.value
          if (item.kind === 'terminal') {
            publish({
              value: latestValue,
              loading: false,
              error: overflowError ?? mapCharacteristicTerminal(item.reason)
            })
            await removeSubscription()
            return
          }
          if (item.kind === 'overflow') {
            overflowError = streamOverflowError('react.useCharacteristicValue.values')
            publish({ value: latestValue, loading: false, error: overflowError })
            continue
          }
          latestValue = item.value
          publish({ value: latestValue, loading: false, error: overflowError })
        }
      } catch (reason) {
        publish({ value: latestValue, loading: false, error: overflowError ?? toError(reason) })
        await removeSubscription()
      }
    }
    observe().catch(() => undefined)
    return () => {
      active = false
      const currentIterator = valueIterator
      if (currentIterator !== null && currentIterator.return !== undefined) {
        const close = currentIterator.return
        observeRejected(() => close.call(currentIterator), reportError)
      }
      observeRejected(() => removeSubscription(), reportError)
    }
  }, [characteristic, characteristicChanged, optionsKey, signal, stableOptions, reportError])
  return characteristic === null
    ? { value: null, loading: false, error: null }
    : characteristicChanged
      ? { value: null, loading: true, error: null }
      : result
}

function mapCharacteristicTerminal(reason: StreamTerminalNotice['reason']): Error | null {
  if (reason === 'closed' || reason === 'owner-released') return null
  if (reason === 'overflow') return streamOverflowError('react.useCharacteristicValue.values')
  if (reason === 'connection-lost') {
    return contractError('connection.lost', 'connection', 'react.useCharacteristicValue')
  }
  if (reason === 'service-changed') {
    return contractError('gatt.stale-handle', 'gatt', 'react.useCharacteristicValue')
  }
  if (reason === 'operation-aborted') {
    return contractError('operation.aborted', 'connection', 'react.useCharacteristicValue')
  }
  if (reason === 'operation-timed-out') {
    return contractError('operation.timed-out', 'connection', 'react.useCharacteristicValue')
  }
  return contractError('stream.closed', 'stream', 'react.useCharacteristicValue')
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
