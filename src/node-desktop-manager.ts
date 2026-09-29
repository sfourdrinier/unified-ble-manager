// src/node-desktop-manager.ts
//
// The one-call desktop manager every Node desktop factory shares: the
// shared Rust core provider for one platform, admitted through the Node host
// manager. Platform guard first (before anything loads), then the provider.

import { createNodeBleManagerFromProvider, selectNodeAdapter, type NodeBleManagerAppOptions } from './node-host-manager'
import {
  createDesktopProcessHostFromBackend,
  assertDesktopProcessOptions,
  type DesktopProcessHost
} from './desktop-process-host'
import { createPublicBleManager } from './public/ble-manager'
import type { BleManager } from './public/ble-manager'
import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import { contractError } from './backend-contract/errors'
import { desktopRustCoreOperation } from './backends/desktop/desktop-rust-core-binding'
import {
  DESKTOP_RUST_CORE_PROFILES,
  DesktopRustCoreBackend,
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
  readonly connectionPolicy?: import('./backends/desktop/bluez-connection-policy').BluezConnectionPolicy | undefined
  readonly bluezBus?: DesktopRustCoreBluezBus
  readonly pairingGeneration?: DesktopRustCoreGenerationController
}

function assertNoForeignConnectionPolicy(platform: DesktopRustCorePlatform, options: object): void {
  if (platform !== 'bluez' && Reflect.get(options, 'connectionPolicy') !== undefined) {
    throw contractError('argument.invalid', 'core', 'desktop.bluez-only-option')
  }
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
  assertNoForeignConnectionPolicy(platform, options)
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  if (options.owner !== undefined && options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner'))
  }
  return createDesktopRustCoreBackendProvider({
    platform,
    owner: options.owner ?? profile.defaultOwner,
    now: options.now,
    hostKind,
    ...(options.binding === undefined ? {} : { binding: options.binding }),
    ...(bluez.bluezBus === undefined ? {} : { bluezBus: bluez.bluezBus }),
    ...(bluez.connectionPolicy === undefined ? {} : { connectionPolicy: bluez.connectionPolicy }),
    ...(bluez.pairingGeneration === undefined ? {} : { pairingGeneration: bluez.pairingGeneration })
  })
}

export async function createDesktopCoreBleManager(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreManagerOptions,
  bluez: BluezCoreProviderExtras = {}
): Promise<BleManager> {
  assertNoForeignConnectionPolicy(platform, options)
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

/** Trusted process owner over one selected desktop backend; children borrow its lifetime. */
export async function createDesktopCoreProcessHost(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreManagerOptions = {},
  bluez: BluezCoreProviderExtras = {},
  hostKind: 'node' | 'desktop-native' = 'node'
): Promise<DesktopProcessHost> {
  assertNoForeignConnectionPolicy(platform, options)
  const { owner, binding, now = () => performance.now(), ...createOptions } = options
  assertDesktopProcessOptions(createOptions)
  const provider = createDesktopCoreProvider(
    platform,
    { now, ...(owner === undefined ? {} : { owner }), ...(binding === undefined ? {} : { binding }) },
    hostKind,
    bluez
  )
  const adapter = selectNodeAdapter(await provider.listAdapters(), options.adapterId)
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  if (!(backend instanceof DesktopRustCoreBackend))
    throw contractError('protocol.incompatible', 'core', 'desktop-process-host.backend')
  return createDesktopProcessHostFromBackend(backend, {
    now,
    ...(options.instanceId === undefined ? {} : { instanceId: options.instanceId }),
    ...(options.diagnostics === undefined ? {} : { diagnostics: options.diagnostics }),
    ...(options.randomBytes === undefined ? {} : { randomBytes: options.randomBytes })
  })
}
