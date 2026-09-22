// src/node-winrt.ts
//
// Windows Node entrypoint. The factories execute the shared Rust core
// (`DesktopCentral` over btleplug's WinRT radio) through the packaged N-API
// addon, identity-checked before any radio call. A missing or mismatched
// core fails loudly; nothing falls back to another backend.

import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import {
  createDesktopCoreBleManager,
  createDesktopCoreProvider,
  type DesktopCoreManagerOptions,
  type DesktopCoreProviderOptions
} from './node-desktop-manager'
import type { BleManager } from './public/ble-manager'

export {
  WINRT_BACKEND_ID,
  WINRT_IMPLEMENTATION_VERSION,
  WINRT_PLATFORM_ID,
  winRtCompatibility
} from './backends/desktop/platform-identity'
export * from './desktop-rust-core-exports'
export type { NodeBleManagerAppOptions } from './node-host-manager'
export type { DesktopCoreManagerOptions, DesktopCoreProviderOptions } from './node-desktop-manager'

export type WinRtBleManagerAppOptions = DesktopCoreManagerOptions
export type NativeWinRtProviderOptions = DesktopCoreProviderOptions

/** One-call Node WinRT manager over the shared Rust core. Does not fall back to another backend. */
export async function createWinRtBleManager(options: WinRtBleManagerAppOptions = {}): Promise<BleManager> {
  return createDesktopCoreBleManager('winrt', options)
}

/** Creates the production Node WinRT provider over the shared Rust core (adapters selectable by id). */
export function createNativeWinRtBackendProvider(
  options: NativeWinRtProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopCoreProvider('winrt', options, 'node')
}
