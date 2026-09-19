// src/node-bluez.ts
//
// Linux Node entrypoint. The factories execute the shared Rust core
// (`DesktopCentral` over btleplug's BlueZ radio) through the packaged N-API
// addon, identity-checked before any radio call. A missing or mismatched
// core fails loudly; nothing falls back to another backend.
//
// D-Bus bus (PR210-20): `busKind` reaches the core (`CentralProfile.bluez_bus`).
// `'system'` is the default. `'session'` serves a BlueZ exported on the
// session bus; a build that cannot reach it answers `capability.unsupported`
// from the core, never a silent system-bus fallback.

import type { BluezPairingGenerationController } from './backends/desktop/bluez-pairing-generation'
import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import type { BluezBusKind } from './backends/desktop/platform-identity'
import {
  admitBluezBusKind,
  createDesktopCoreBleManager,
  createDesktopCoreProvider,
  type DesktopCoreManagerOptions
} from './node-desktop-manager'
import type { DesktopRustCoreBinding } from './backends/desktop/desktop-rust-core-binding'
import type { BleManager } from './public/ble-manager'

export {
  BLUEZ_BACKEND_ID,
  BLUEZ_IMPLEMENTATION_VERSION,
  BLUEZ_PLATFORM_ID,
  bluezCompatibility
} from './backends/desktop/platform-identity'
export * from './desktop-rust-core-exports'
export type { BluezBusKind } from './backends/desktop/platform-identity'
export type { NodeBleManagerAppOptions } from './node-host-manager'
export type { DesktopCoreManagerOptions, DesktopCoreProviderOptions } from './node-desktop-manager'
export type {
  BluezPairingGeneration,
  BluezPairingGenerationController
} from './backends/desktop/bluez-pairing-generation'

export interface BluezBleManagerAppOptions extends DesktopCoreManagerOptions {
  /** The D-Bus bus BlueZ is reached on (`'system'` by default). */
  readonly busKind?: BluezBusKind
  /**
   * Host-supplied privileged operation that selects the adapter's LE pairing
   * generation (CAP_NET_ADMIN). With it, `pair({ secureConnections })`
   * holds the adapter generation for the ceremony and restores it after;
   * without it, a directed generation is `capability.unsupported`. The
   * setting is adapter-wide while held (see `BluezPairingGenerationController`).
   */
  readonly pairingGeneration?: BluezPairingGenerationController
}

export interface DbusNextBluezProviderOptions {
  readonly busKind: BluezBusKind
  readonly now: () => number
  /** See {@link BluezBleManagerAppOptions.pairingGeneration}. */
  readonly pairingGeneration?: BluezPairingGenerationController
  /** Owner label for admitted core centrals (host identity). */
  readonly owner?: string
  /** Injected core entry (tests and embedding hosts). */
  readonly binding?: DesktopRustCoreBinding
}

/** One-call Node BlueZ manager over the shared Rust core. Does not fall back to another backend. */
export async function createBluezBleManager(options: BluezBleManagerAppOptions = {}): Promise<BleManager> {
  const { busKind = 'system', pairingGeneration, ...managerOptions } = options
  return createDesktopCoreBleManager('bluez', managerOptions, {
    bluezBus: admitBluezBusKind(busKind),
    ...(pairingGeneration === undefined ? {} : { pairingGeneration })
  })
}

/**
 * Creates the production Node BlueZ provider over the shared Rust core for
 * the system bus (the name is historical: no dbus-next transport is used).
 */
export function createDbusNextBluezBackendProvider(
  options: DbusNextBluezProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopCoreProvider(
    'bluez',
    {
      now: options.now,
      ...(options.owner === undefined ? {} : { owner: options.owner }),
      ...(options.binding === undefined ? {} : { binding: options.binding })
    },
    'node',
    {
      bluezBus: admitBluezBusKind(options.busKind),
      ...(options.pairingGeneration === undefined ? {} : { pairingGeneration: options.pairingGeneration })
    }
  )
}
