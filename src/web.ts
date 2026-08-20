/* eslint-disable @typescript-eslint/no-explicit-any, no-void */
// src/web.ts — zero-plumbing Web Bluetooth factory (PR1)

import type { BleManager } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { createWebBluetoothProvider, WEB_BLUETOOTH_ADAPTER_ID } from './web/web-bluetooth-backend'
import {
  createDefaultNavigatorWebBluetoothEnvironment,
  NavigatorWebBluetoothBoundary,
  type NavigatorWebBluetoothEnvironment
} from './web/navigator-web-bluetooth-boundary'
import { createBleManagerFromBackend, DEFAULT_BLE_MANAGER_OPTIONS } from './manager/ble-manager'
import { opaqueId } from './backend-contract/primitives'
import { createEphemeralHostIdentity } from './public/host-identity'

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

// New zero-plumbing Web factory — returns one BleManager, not a tuple.
// Chooser is a capability on the manager, not a separate return value.
// Overloaded to keep RC1 test compatibility: if called with { provider, clientId } it delegates to legacy tuple.
export async function createWebBleManager(
  options: BleManagerCreateOptions & { provider?: any; clientId?: string; managerId?: string; now?: () => number } = {}
): Promise<BleManager | WebBleManagerSession> {
  // Backward compat: RC1 tests call createWebBleManager({ provider, clientId, managerId, now })
  if ((options as any).provider !== undefined) {
    const legacy = options as unknown as WebBleManagerOptions
    return createWebBleManagerLegacy(legacy) as unknown as BleManager
  }
  const normalized = normalizeBleManagerCreateOptions(options as BleManagerCreateOptions)
  const env = createDefaultNavigatorWebBluetoothEnvironment()
  const provider = createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(env))
  const ephemeral = createEphemeralHostIdentity()
  const clientId = opaqueId(`web-${ephemeral.managerNonce.slice(0, 8)}`, 'client', 'web-bluetooth:browser')
  const managerId = opaqueId(`web-${ephemeral.attachmentNonce.slice(0, 8)}`, 'manager', 'web-bluetooth:browser')
  const internal = await createBleManagerFromBackend(
    await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID }),
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: env.now }
  )
  // Normalized instanceId does not affect Web; adapterId ignored (single adapter)
  void normalized
  return createPublicBleManager(internal as any, env.now)
}

// Legacy Web factory that returns { chooser, manager } tuple — kept for existing tests.
// New code should use createWebBleManager() which returns a single BleManager.
export interface WebBleManagerOptions {
  readonly provider: import('./web/web-bluetooth-backend').WebBluetoothProvider
  readonly clientId: string
  readonly managerId: string
  readonly now: () => number
}
export interface NavigatorWebBleManagerOptions {
  readonly clientId: string
  readonly managerId: string
  readonly environment?: NavigatorWebBluetoothEnvironment
}
export interface WebBleManagerSession {
  readonly chooser: import('./backend-contract/host/web').WebChooser<string>
  readonly manager: import('./manager/ble-manager').BleManager<string, any>
}
export async function createNavigatorWebBleManager(
  options: NavigatorWebBleManagerOptions
): Promise<WebBleManagerSession> {
  const environment = options.environment ?? createDefaultNavigatorWebBluetoothEnvironment()
  const provider = createNavigatorWebBluetoothProvider(environment)
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const manager = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: {
        clientId: opaqueId(options.clientId, 'client', 'web-bluetooth:browser'),
        managerId: opaqueId(options.managerId, 'manager', 'web-bluetooth:browser'),
        ownerMode: 'owning'
      }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: environment.now }
  )
  return { chooser: backend, manager }
}
export async function createWebBleManagerLegacy(options: WebBleManagerOptions): Promise<WebBleManagerSession> {
  const backend = await options.provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const manager = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: options.provider.descriptor.compatibility,
      manager: {
        clientId: opaqueId(options.clientId, 'client', 'web-bluetooth:browser'),
        managerId: opaqueId(options.managerId, 'manager', 'web-bluetooth:browser'),
        ownerMode: 'owning'
      }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: options.now }
  )
  return { chooser: backend, manager }
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
  const internal = await createBleManagerFromBackend(
    await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID }),
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: env.now }
  )
  return createPublicBleManager(internal as any, env.now)
}
