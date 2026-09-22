// src/electron/main-binding-inspection.ts
//
// Test-only inspection of an Electron main binding's renderer release retries
// (finding F3). It lives outside the production entrypoint and is exported
// from `unified-ble-manager/testing` only (decision L: no test seams in
// production entrypoints).

import { contractError } from '../backend-contract/errors'
import type { CleanupRecord } from '../backend-contract/errors'

export interface ElectronMainBindingRendererReleaseSnapshot {
  readonly leaseId: string
  readonly releaseRetries: number
  readonly retryExhausted: boolean
  readonly lastReleaseFailure: CleanupRecord | null
}

export interface ElectronMainBindingReleaseInspection {
  readonly lifecycle: 'active' | 'destroying' | 'release-required' | 'destroyed'
  readonly renderers: readonly ElectronMainBindingRendererReleaseSnapshot[]
}

const electronBindingReleaseInspectors = new WeakMap<object, () => ElectronMainBindingReleaseInspection>()

export function registerElectronBindingReleaseInspector(
  binding: object,
  inspect: () => ElectronMainBindingReleaseInspection
): void {
  electronBindingReleaseInspectors.set(binding, inspect)
}

/**
 * Per-renderer release-retry attempts and the terminal `release-failed` record
 * reported when the retry bound is reached.
 */
export function inspectElectronMainBleBindingForTests(binding: object): ElectronMainBindingReleaseInspection {
  const inspect = electronBindingReleaseInspectors.get(binding)
  if (inspect === undefined) {
    throw contractError('argument.invalid', 'ipc', 'electron-main-binding.release-inspect')
  }
  return inspect()
}
