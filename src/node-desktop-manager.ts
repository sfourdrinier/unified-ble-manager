// src/node-desktop-manager.ts
//
// The one-call desktop manager every Node desktop factory shares: the
// shared Rust core provider for one platform, admitted through the Node host
// manager. Platform guard first (before anything loads), then the provider.

import { createNodeBleManagerFromProvider, type NodeBleManagerAppOptions } from './node-host-manager'
import { createPublicBleManager } from './public/ble-manager'
import type { BleManager } from './public/ble-manager'
import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import { contractError } from './backend-contract/errors'
import { desktopRustCoreOperation } from './backends/desktop/desktop-rust-core-binding'
import {
  DESKTOP_RUST_CORE_PROFILES,
  createDesktopRustCoreBackendProvider
} from './backends/desktop/desktop-rust-core-provider'
import type {
  DesktopRustCoreBinding,
  DesktopRustCoreBluezBus,
  DesktopRustCoreGenerationController,
  DesktopRustCorePlatform
} from './backends/desktop/desktop-rust-core-binding'

/** Options every desktop factory accepts beyond the Node manager options. */
export interface DesktopCoreManagerOptions extends NodeBleManagerAppOptions {
  /** Owner label for admitted core centrals (host identity). */
  readonly owner?: string
  /**
   * Injected core entry (tests and embedding hosts). Absent, the factory
   * loads the packaged addon and verifies its build identity before any
   * radio call; there is no TypeScript fallback.
   */
  readonly binding?: DesktopRustCoreBinding
}

export interface DesktopCoreProviderOptions {
  readonly now: () => number
  readonly owner?: string
  readonly binding?: DesktopRustCoreBinding
}

/** BlueZ-only provider facts (the legacy dbus-next options). */
export interface BluezCoreProviderExtras {
  readonly bluezBus?: DesktopRustCoreBluezBus
  readonly pairingGeneration?: DesktopRustCoreGenerationController
}

/** Admit the BlueZ bus before anything loads: `system` or `session`; anything else is invalid. */
export function admitBluezBusKind(busKind: unknown): DesktopRustCoreBluezBus {
  if (busKind === 'system' || busKind === 'session') return busKind
  throw contractError(
    'argument.invalid',
    'core',
    desktopRustCoreOperation(DESKTOP_RUST_CORE_PROFILES.bluez.operationPrefix, 'bus-kind')
  )
}

export function createDesktopCoreProvider(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreProviderOptions,
  hostKind: 'node' | 'desktop-native',
  bluez: BluezCoreProviderExtras = {}
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  if (options.owner !== undefined && options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner'))
  }
  return createDesktopRustCoreBackendProvider({
    platform,
    owner: options.owner ?? profile.defaultOwner,
    now: options.now,
    hostKind,
    radio: 'production',
    ...(options.binding === undefined ? {} : { binding: options.binding }),
    ...(bluez.bluezBus === undefined ? {} : { bluezBus: bluez.bluezBus }),
    ...(bluez.pairingGeneration === undefined ? {} : { pairingGeneration: bluez.pairingGeneration })
  })
}

export async function createDesktopCoreBleManager(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreManagerOptions,
  bluez: BluezCoreProviderExtras = {}
): Promise<BleManager> {
  const now = options.now ?? (() => performance.now())
  const { owner, binding, ...managerOptions } = options
  const provider = createDesktopCoreProvider(
    platform,
    { now, ...(owner === undefined ? {} : { owner }), ...(binding === undefined ? {} : { binding }) },
    'node',
    bluez
  )
  const internal = await createNodeBleManagerFromProvider(
    provider,
    DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
    managerOptions
  )
  return createPublicBleManager(internal, now)
}
