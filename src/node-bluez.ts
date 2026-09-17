// src/node-bluez.ts
//
// R03 cutover: the public node-bluez factories execute the shared Rust core
// (`DesktopCentral` via the NAPI `UbmCentral` dispatch) through
// `createBluezRustCoreBackendProvider`. The TypeScript D-Bus transport
// (`DbusNextBluezBoundaryFactory` + `BluezBackend`) stays exported for
// package-surface compatibility and unit coverage, but it is no longer on
// the public factory execution path: BLE work executes the core, and a
// missing core fails loudly (`capability.unsupported` /
// `bluez-manager.rust-core-missing`), never by silently running the legacy
// transport.

import type { BluezPairingGenerationController } from './backends/bluez/bluez-pairing-generation'
import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import { bluezCompatibility } from './backends/bluez/bluez-backend-provider'
import { createNodeBleManagerFromProvider, type NodeBleManagerAppOptions } from './node-host-manager'
import type { BluezBusKind } from './backends/bluez/bluez-dbus-contract'
import {
  createBluezRustCoreBackendProvider,
  type BluezRustCoreBinding,
  type BluezRustCoreProviderOptions
} from './backends/bluez/bluez-rust-core-provider'
import { contractError } from './backend-contract/errors'
import { createPublicBleManager } from './public/ble-manager'
import type { BleManager } from './public/ble-manager'

export {
  BLUEZ_BACKEND_ID,
  BLUEZ_IMPLEMENTATION_VERSION,
  BLUEZ_PLATFORM_ID,
  bluezCompatibility,
  createBluezBackendProvider
} from './backends/bluez/bluez-backend-provider'
export { DbusNextBluezBoundaryFactory } from './backends/bluez/bluez-dbus-next-boundary'
export {
  BLUEZ_RUST_CORE_BACKEND_ID,
  BLUEZ_RUST_CORE_IMPLEMENTATION_VERSION,
  BLUEZ_RUST_CORE_MISSING_OPERATION,
  BLUEZ_RUST_CORE_PROVIDER_ID,
  BluezRustCoreBackend,
  createBluezRustCoreBackendProvider,
  resolveBluezRustCoreBinding,
  toBluezRustCoreError
} from './backends/bluez/bluez-rust-core-provider'
export type {
  BluezBusKind,
  BluezDbusBoundary,
  BluezDbusBoundaryFactory,
  BluezDbusErrorDetail,
  BluezInterfacesAdded,
  BluezInterfacesRemoved,
  BluezManagedInterface,
  BluezManagedObject,
  BluezMethodBoundary,
  BluezObjectManagerBoundary,
  BluezProperties,
  BluezPropertiesChanged,
  BluezVariant
} from './backends/bluez/bluez-dbus-contract'
export type { BluezBackendProviderOptions } from './backends/bluez/bluez-backend-provider'
export type {
  BluezRustCoreAdvertisement,
  BluezRustCoreBinding,
  BluezRustCoreCentral,
  BluezRustCorePath,
  BluezRustCoreProviderOptions,
  BluezRustCoreRadio,
  BluezRustCoreSelector
} from './backends/bluez/bluez-rust-core-provider'

export interface DbusNextBluezProviderOptions {
  readonly busKind: BluezBusKind
  readonly now: () => number
  /**
   * Privileged pairing-generation controller. The shared core executes no
   * pairing ceremony, so supplying one fails loudly at the factory: pairing
   * BLE work cannot execute partly in the core and partly in TypeScript.
   */
  readonly pairingGeneration?: BluezPairingGenerationController
  /** Owner label for the admitted core central (host identity). */
  readonly owner?: string
  /**
   * Injected core entry (tests). Absent, the factory resolves the dispatch
   * addon and a missing core fails loudly — never a silent legacy transport.
   */
  readonly binding?: BluezRustCoreBinding
}

export type { NodeBleManagerAppOptions }
export type {
  BluezPairingGeneration,
  BluezPairingGenerationController
} from './backends/bluez/bluez-pairing-generation'

export interface BluezBleManagerAppOptions extends NodeBleManagerAppOptions {
  readonly busKind?: BluezBusKind
  /**
   * Privileged operation that selects the adapter's LE pairing generation.
   * The shared core executes no pairing ceremony: supplying this fails
   * loudly at the factory instead of splitting BLE work across authorities.
   * Omit it (the default posture) for core-executed management.
   */
  readonly pairingGeneration?: BluezPairingGenerationController
  /** Owner label for the admitted core central (host identity). */
  readonly owner?: string
  /**
   * Injected core entry (tests). Absent, the factory resolves the dispatch
   * addon and a missing core fails loudly — never a silent legacy transport.
   */
  readonly binding?: BluezRustCoreBinding
}

function rejectPairingGeneration(pairingGeneration: unknown): void {
  if (pairingGeneration !== undefined) {
    throw contractError('capability.unsupported', 'capability', 'bluez-manager.pairing-generation')
  }
}

function admitBusKind(busKind: BluezBusKind): void {
  if (busKind !== 'system' && busKind !== 'session') {
    throw contractError('argument.invalid', 'core', 'bluez-manager.bus-kind')
  }
}

function coreProviderOptions(
  options: { busKind: BluezBusKind; now: () => number; owner?: string; binding?: BluezRustCoreBinding },
  optionName: string
): BluezRustCoreProviderOptions {
  admitBusKind(options.busKind)
  if (options.owner !== undefined && options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', optionName)
  }
  return {
    owner: options.owner ?? 'node-bluez',
    now: options.now,
    radio: 'production',
    binding: options.binding
  }
}

/** One-call Node BlueZ manager over the shared Rust core. Does not fall back to another backend. */
export async function createBluezBleManager(options: BluezBleManagerAppOptions = {}): Promise<BleManager> {
  const now = options.now ?? (() => performance.now())
  const { busKind = 'system', pairingGeneration, owner, binding, ...managerOptions } = options
  rejectPairingGeneration(pairingGeneration)
  const internal = await createNodeBleManagerFromProvider(
    createBluezRustCoreBackendProvider(coreProviderOptions({ busKind, now, owner, binding }, 'bluez-manager.owner')),
    bluezCompatibility,
    managerOptions
  )
  return createPublicBleManager(internal, now)
}

/**
 * Creates the production Node BlueZ provider over the shared Rust core for
 * one explicitly selected D-Bus bus. The bus keeps selecting the host
 * attachment, but BLE work executes the core: the `DbusNext` boundary
 * factory is no longer constructed here. A missing core fails loudly.
 */
export function createDbusNextBluezBackendProvider(
  options: DbusNextBluezProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  rejectPairingGeneration(options.pairingGeneration)
  return createBluezRustCoreBackendProvider(coreProviderOptions(options, 'bluez-manager.owner'))
}
