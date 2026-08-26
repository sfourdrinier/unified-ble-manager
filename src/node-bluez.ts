// src/node-bluez.ts

import type { BluezPairingGenerationController } from './backends/bluez/bluez-pairing-generation'
import type { BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import {
  bluezCompatibility,
  createBluezBackendProvider,
  type BluezBackendProviderOptions
} from './backends/bluez/bluez-backend-provider'
import { createNodeBleManagerFromProvider, type NodeBleManagerAppOptions } from './node-host-manager'
import type { BluezBusKind } from './backends/bluez/bluez-dbus-contract'
import { DbusNextBluezBoundaryFactory } from './backends/bluez/bluez-dbus-next-boundary'
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

export interface DbusNextBluezProviderOptions {
  readonly busKind: BluezBusKind
  readonly now: () => number
  /** See `BluezBleManagerAppOptions.pairingGeneration`. */
  readonly pairingGeneration?: BluezPairingGenerationController
}

export type { NodeBleManagerAppOptions }
export type {
  BluezPairingGeneration,
  BluezPairingGenerationController
} from './backends/bluez/bluez-pairing-generation'

export interface BluezBleManagerAppOptions extends NodeBleManagerAppOptions {
  readonly busKind?: BluezBusKind
  /**
   * Privileged operation that selects the adapter's LE pairing generation,
   * enabling `PairOptions.secureConnections` `'require'` and `'disallow'`.
   *
   * This package never acquires the privilege itself; supplying this is how a
   * host opts in. The setting is **adapter-wide** and **outlives the process**.
   * Read `docs/BONDING.md` before implementing one. Omit it and those two
   * values keep failing closed, which is the default posture.
   */
  readonly pairingGeneration?: BluezPairingGenerationController
}

/** One-call Node BlueZ manager. Does not fall back to another backend. */
export async function createBluezBleManager(options: BluezBleManagerAppOptions = {}): Promise<BleManager> {
  const now = options.now ?? (() => performance.now())
  const { busKind = 'system', pairingGeneration, ...managerOptions } = options
  const internal = await createNodeBleManagerFromProvider(
    createDbusNextBluezBackendProvider({ busKind, now, pairingGeneration }),
    bluezCompatibility,
    managerOptions
  )
  return createPublicBleManager(internal, now)
}

/** Creates the production Node BlueZ provider for one explicitly selected D-Bus bus. */
export function createDbusNextBluezBackendProvider(
  options: DbusNextBluezProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const providerOptions: BluezBackendProviderOptions = {
    busKind: options.busKind,
    boundaryFactory: new DbusNextBluezBoundaryFactory(),
    now: options.now,
    pairingGeneration: options.pairingGeneration
  }
  return createBluezBackendProvider(providerOptions)
}
