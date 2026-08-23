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
import type { BleAdapterState } from './public/ble-adapter'
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
const BleContext = React.createContext<BleContextValue | null>(null)
const BleErrorContext = React.createContext<((error: Error) => void) | null>(null)
let pendingManagerRelease: Promise<void> | null = null

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
  const [result, setResult] = React.useState<UseAdapterStateResult>({
    state: null,
    loading: true,
    error: null
  })

  React.useEffect(() => {
    let active = true
    if (ble.manager === null) {
      return () => {
        active = false
      }
    }

    getAdapterState(ble.manager)
      .then(
        state => {
          if (active) setResult({ state, loading: false, error: null })
        },
        error => {
          if (active) setResult({ state: null, loading: false, error: toError(error) })
        }
      )
      .catch(() => undefined)

    return () => {
      active = false
    }
  }, [ble.manager, ble.loading, ble.error])

  return ble.manager === null ? { state: null, loading: ble.loading, error: ble.error } : result
}

export function getBleCapability(
  manager: Pick<BleManager, 'capabilities'>,
  id: FeatureId
): CapabilityDescriptor | undefined {
  return manager.capabilities.get(id)
}

export function useBleCapability(id: FeatureId): CapabilityDescriptor | undefined {
  const { manager } = useBle()
  return manager === null ? undefined : getBleCapability(manager, id)
}

export function getBleReadiness(manager: Pick<ExpoBleManager, 'readiness'>): Promise<BleReadiness> {
  return manager.readiness()
}

export function useBleReadiness(): UseBleReadinessResult {
  const { manager, loading, error } = useBle()
  const [result, setResult] = React.useState<UseBleReadinessResult>({
    readiness: null,
    loading: true,
    error: null
  })
  const [previousManager, setPreviousManager] = React.useState(manager)
  const managerChanged = previousManager !== manager
  if (managerChanged) {
    setPreviousManager(manager)
    setResult({ readiness: null, loading: true, error: null })
  }

  React.useEffect(() => {
    let active = true
    if (manager === null) return () => undefined
    if (!hasReadiness(manager)) {
      return () => undefined
    }
    getBleReadiness(manager).then(
      readiness => {
        if (active) setResult({ readiness, loading: false, error: null })
      },
      reason => {
        if (active) setResult({ readiness: null, loading: false, error: toError(reason) })
      }
    )
    return () => {
      active = false
    }
  }, [manager, managerChanged])

  if (manager === null) return { readiness: null, loading, error }
  if (!hasReadiness(manager)) {
    return {
      readiness: null,
      loading: false,
      error: new Error('BLE readiness is available only from an Expo host manager.')
    }
  }
  return managerChanged ? { readiness: null, loading: true, error: null } : result
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
  private released = false
  private createFailureReported = false
  private destroyFailureReported = false

  constructor(private readonly createManager: BleManagerFactory) {}

  create(): Promise<BleManager> {
    if (this.managerPromise === null) {
      const pendingRelease = pendingManagerRelease
      this.managerPromise =
        pendingRelease === null ? this.createManager() : pendingRelease.then(() => this.createManager())
    }
    return this.managerPromise
  }

  isActive(): boolean {
    return !this.released && !this.releaseScheduled
  }

  cancelScheduledRelease(): void {
    if (!this.released) this.releaseScheduled = false
  }

  scheduleRelease(report: (error: Error) => void): void {
    if (this.released || this.releaseScheduled) return
    this.releaseScheduled = true

    let resolveReleaseBarrier: () => void = () => undefined
    const releaseBarrier = new Promise<void>(resolve => {
      resolveReleaseBarrier = resolve
    })
    const previousRelease = pendingManagerRelease ?? Promise.resolve()
    const scheduledRelease = previousRelease.then(() => releaseBarrier)
    pendingManagerRelease = scheduledRelease
    scheduledRelease.then(() => {
      if (pendingManagerRelease === scheduledRelease) pendingManagerRelease = null
    })

    queueMicrotask(() => {
      if (!this.releaseScheduled || this.released) {
        resolveReleaseBarrier()
        return
      }
      this.released = true
      this.release(report).then(resolveReleaseBarrier, resolveReleaseBarrier)
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

  private async release(report: (error: Error) => void): Promise<void> {
    let manager: BleManager
    try {
      manager = await this.create()
    } catch {
      return
    }

    try {
      const cleanup = await manager.destroy()
      if (cleanup.state === 'release-failed') {
        reportCleanupFailure(cleanup, report, 'manager destroy')
      }
    } catch (error) {
      report(toError(error))
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
