import type { BleManager } from './ble-manager'

export type ReactAdapterWatchOwnershipSnapshot = {
  readonly runCount: number
  readonly phase: 'idle' | 'starting' | 'active' | 'stopping' | 'cleanup-failed'
  readonly hasWatch: boolean
}

export const adapterWatchOwnershipInspectors = new WeakMap<BleManager, () => ReactAdapterWatchOwnershipSnapshot>()

export function inspectReactAdapterWatchOwnershipForTests(manager: BleManager): ReactAdapterWatchOwnershipSnapshot {
  const inspect = adapterWatchOwnershipInspectors.get(manager)
  if (inspect === undefined) {
    return { runCount: 0, phase: 'idle', hasWatch: false }
  }
  return inspect()
}
