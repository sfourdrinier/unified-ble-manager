// src/electron-main.ts
//
// Electron main owns the radio. The providers here are the same shared Rust
// core the Node entrypoints use (`hostKind: 'desktop-native'`), loaded from
// the packaged N-API addon (ABI-stable across Node and Electron). Renderer
// IPC lives in ./electron/main; renderers never load native code.

import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import type { BluezBusKind } from './backends/desktop/platform-identity'
import type { BluezPairingGenerationController } from './backends/desktop/bluez-pairing-generation'
import { admitBluezBusKind, createDesktopCoreProvider, type DesktopCoreProviderOptions } from './node-desktop-manager'

export * from './node-corebluetooth'
export * from './node-winrt'
export * from './node-bluez'
export * from './electron/main'

/** Creates the Electron-main macOS provider over the shared Rust core. */
export function createElectronMainCoreBluetoothBackendProvider(
  options: DesktopCoreProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopCoreProvider('corebluetooth', options, 'desktop-native')
}

/** Creates the Electron-main Windows provider over the shared Rust core. */
export function createElectronMainWinRtBackendProvider(
  options: DesktopCoreProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopCoreProvider('winrt', options, 'desktop-native')
}

export type { BluezBusKind } from './backends/desktop/platform-identity'
export type {
  BluezPairingGeneration,
  BluezPairingGenerationController
} from './backends/desktop/bluez-pairing-generation'

/** The Node BlueZ factory's options, for the Electron-main BlueZ provider. */
export interface ElectronMainBluezProviderOptions extends DesktopCoreProviderOptions {
  /** The D-Bus bus BlueZ is reached on (`'system'` by default). */
  readonly busKind?: BluezBusKind
  /**
   * Host-supplied privileged operation that selects the adapter's LE pairing
   * generation (CAP_NET_ADMIN), as on the Node BlueZ factory. Without it, a
   * directed `secureConnections` is `capability.unsupported`. The setting is
   * adapter-wide while held (see `BluezPairingGenerationController`).
   */
  readonly pairingGeneration?: BluezPairingGenerationController
}

/** Creates the Electron-main Linux provider over the shared Rust core (BlueZ; system bus by default). */
export function createElectronMainBluezBackendProvider(
  options: ElectronMainBluezProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const { busKind = 'system', pairingGeneration, ...providerOptions } = options
  return createDesktopCoreProvider('bluez', providerOptions, 'desktop-native', {
    bluezBus: admitBluezBusKind(busKind),
    ...(pairingGeneration === undefined ? {} : { pairingGeneration })
  })
}
