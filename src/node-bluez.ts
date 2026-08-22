// src/node-bluez.ts

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
}

export type { NodeBleManagerAppOptions }

export interface BluezBleManagerAppOptions extends NodeBleManagerAppOptions {
  readonly busKind?: BluezBusKind
}

/** One-call Node BlueZ manager. Does not fall back to another backend. */
export async function createBluezBleManager(options: BluezBleManagerAppOptions = {}): Promise<BleManager> {
  const now = options.now ?? (() => performance.now())
  const { busKind = 'system', ...managerOptions } = options
  const internal = await createNodeBleManagerFromProvider(
    createDbusNextBluezBackendProvider({ busKind, now }),
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
    now: options.now
  }
  return createBluezBackendProvider(providerOptions)
}
