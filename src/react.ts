import * as React from 'react'
import type { ReactNode } from 'react'
import type { CleanupRecord } from './backend-contract/errors'
import type { BleManager } from './public/ble-manager'
import type { BleAdapterState } from './public/ble-adapter'
import type { BleCapabilities, CapabilityDescriptor, FeatureId } from './public/capabilities'

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

const missingProviderError = new Error('useBle must be used within a BleProvider')
const BleContext = React.createContext<BleContextValue | null>(null)

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

  return React.createElement(BleContext.Provider, { value }, children)
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

class ManagerLease {
  private managerPromise: Promise<BleManager> | null = null
  private releaseScheduled = false
  private released = false
  private createFailureReported = false
  private destroyFailureReported = false

  constructor(private readonly createManager: BleManagerFactory) {}

  create(): Promise<BleManager> {
    if (this.managerPromise === null) {
      this.managerPromise = this.createManager()
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
    queueMicrotask(() => {
      if (!this.releaseScheduled || this.released) return
      this.released = true
      this.release(report).catch(() => undefined)
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
        reportCleanupFailure(cleanup, report)
      }
    } catch (error) {
      report(toError(error))
    }
  }
}

function reportCleanupFailure(cleanup: CleanupRecord, report: (error: Error) => void): void {
  const error = new Error('BLE manager destroy reported release-failed')
  Object.defineProperty(error, 'cleanup', { value: cleanup, enumerable: true })
  report(error)
}

function toError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error))
}

export type { BleAdapterState, BleCapabilities, CapabilityDescriptor, FeatureId }
