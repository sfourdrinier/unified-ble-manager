// src/web.ts — zero-plumbing Web Bluetooth factory (PR1 final, no compatibility aliases)

import { snapshotBlePeer } from './public/ble-manager'
import type { BleManager, ChooseOptions } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import {
  createWebBluetoothProvider,
  WEB_BLUETOOTH_ADAPTER_ID,
  type WebBluetoothBackend
} from './web/web-bluetooth-backend'
import {
  createDefaultNavigatorWebBluetoothEnvironment,
  NavigatorWebBluetoothBoundary,
  type NavigatorWebBluetoothEnvironment
} from './web/navigator-web-bluetooth-boundary'
import { createBleManagerFromBackend, DEFAULT_BLE_MANAGER_OPTIONS } from './manager/ble-manager'
import { opaqueId } from './backend-contract/primitives'
import { createEphemeralHostIdentity } from './public/host-identity'
import { canonicalUuid } from './backend-contract/primitives'
import { normalizeOperationOptions } from './public/operation-options'
import { contractError } from './backend-contract/errors'

export {
  createWebBluetoothProvider,
  WEB_BLUETOOTH_ADAPTER_ID,
  WebBluetoothBackend,
  WebBluetoothProvider
} from './web/web-bluetooth-backend'
export {
  NavigatorWebBluetoothBoundary,
  createDefaultNavigatorWebBluetoothEnvironment
} from './web/navigator-web-bluetooth-boundary'
export function createNavigatorWebBluetoothProvider(environment: NavigatorWebBluetoothEnvironment) {
  return createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(environment))
}
export type { NavigatorWebBluetoothEnvironment } from './web/navigator-web-bluetooth-boundary'
export type { ChooserRequest, ChooserSelection, WebChooser, WebHost } from './backend-contract/host/web'
export type {
  WebBluetoothBoundary,
  WebBluetoothCharacteristicBoundary,
  WebBluetoothCharacteristicProperties,
  WebBluetoothDescriptorBoundary,
  WebBluetoothDeviceBoundary,
  WebBluetoothDeviceSelection,
  WebBluetoothDisconnectListener,
  WebBluetoothGattServerBoundary,
  WebBluetoothNotificationListener,
  WebBluetoothPageLifecycleReason,
  WebBluetoothRequestDeviceOptions,
  WebBluetoothRequestFilter,
  WebBluetoothRequestManufacturerDataFilter,
  WebBluetoothServiceBoundary,
  WebBluetoothTimerHandle
} from './web/web-bluetooth-boundary'

// Zero-plumbing Web factory — returns one BleManager. No provider/clientId tuple.
export async function createWebBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  const env = createDefaultNavigatorWebBluetoothEnvironment()
  const provider = createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(env))
  const ephemeral = createEphemeralHostIdentity()
  const clientId = opaqueId(`web-${ephemeral.managerNonce.slice(0, 8)}`, 'client', 'web-bluetooth:browser')
  const managerId = opaqueId(`web-${ephemeral.attachmentNonce.slice(0, 8)}`, 'manager', 'web-bluetooth:browser')
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const internal = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: env.now }
  )
  return createPublicBleManager(internal, env.now, {
    discoveryKind: 'system-chooser',
    choose: optionsValue => chooseWebPeer(backend, optionsValue, env.now)
  })
}

// Explicit provider injection for tests and unusual hosts.
export interface WebBleManagerWithEnvironmentOptions {
  readonly environment: NavigatorWebBluetoothEnvironment
  readonly createOptions?: BleManagerCreateOptions
}

export async function createWebBleManagerWithEnvironment(
  options: WebBleManagerWithEnvironmentOptions
): Promise<BleManager> {
  const env = options.environment
  const provider = createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(env))
  const ephemeral = createEphemeralHostIdentity()
  const clientId = opaqueId(`web-${ephemeral.managerNonce.slice(0, 8)}`, 'client', 'web-bluetooth:browser')
  const managerId = opaqueId(`web-${ephemeral.attachmentNonce.slice(0, 8)}`, 'manager', 'web-bluetooth:browser')
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const internal = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: env.now }
  )
  return createPublicBleManager(internal, env.now, {
    discoveryKind: 'system-chooser',
    choose: optionsValue => chooseWebPeer(backend, optionsValue, env.now)
  })
}

async function chooseWebPeer(backend: WebBluetoothBackend, options: ChooseOptions, now: () => number) {
  const normalized = normalizeOperationOptions(options, now)
  const services =
    options.services?.map(value => canonicalUuid(typeof value === 'number' ? value.toString(16) : value)) ?? []
  const selection = await backend.choose(
    {
      filters: services.length === 0 ? [] : [{ serviceUuids: services, manufacturerData: [], localNamePrefix: null }],
      acceptAllDevices: services.length === 0,
      optionalServices: services
    },
    normalized
  )
  const selected = backend.peerReferenceFor(String(selection.peerId))
  if (selected === null) throw contractError('protocol.violation', 'connection', 'web.choose.peer-reference')
  return snapshotBlePeer({
    id: String(selection.peerId),
    name: null,
    rssi: null,
    reference: { version: 1, backendId: selected.backendId, scope: 'origin', opaqueId: selected.browserDeviceId },
    sources: ['origin-authorized'],
    lastAdvertisement: null,
    state: {
      reachability: 'unknown',
      connection: 'disconnected',
      bond: 'unsupported',
      lastSeenAtMonotonicMs: null
    }
  })
}
