// src/web.ts — zero-plumbing Web Bluetooth factory (PR1 final, no compatibility aliases)

import { assertPublicChooseOptions, snapshotBlePeer } from './public/ble-manager'
import type { BleManager, ChooseFilter, ChooseOptions } from './public/ble-manager'
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
import { byteLimit, opaqueId } from './backend-contract/primitives'
import { createEphemeralHostIdentity } from './public/host-identity'
import { canonicalUuid } from './backend-contract/primitives'
import type { Uuid } from './backend-contract/primitives'
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
  const normalized = normalizeWebCreateOptions(options)
  const env = createDefaultNavigatorWebBluetoothEnvironment()
  return createWebManager(env, normalized)
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
  const normalized = normalizeWebCreateOptions(options.createOptions)
  return createWebManager(env, normalized)
}

async function createWebManager(
  env: NavigatorWebBluetoothEnvironment,
  options: BleManagerCreateOptions
): Promise<BleManager> {
  const provider = createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(env))
  const ephemeral = createEphemeralHostIdentity()
  const instanceSuffix = options.instanceId === undefined ? '' : `-${options.instanceId}`
  const clientId = opaqueId(`web-${ephemeral.managerNonce}${instanceSuffix}`, 'client', 'web-bluetooth:browser')
  const managerId = opaqueId(`web-${ephemeral.attachmentNonce}${instanceSuffix}`, 'manager', 'web-bluetooth:browser')
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  const internal = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    {
      ...DEFAULT_BLE_MANAGER_OPTIONS,
      now: env.now,
      maximumValueBytes:
        options.diagnostics?.maximumValueBytes === undefined
          ? DEFAULT_BLE_MANAGER_OPTIONS.maximumValueBytes
          : byteLimit(options.diagnostics.maximumValueBytes),
      traceMaximumRecords: options.diagnostics?.traceMaximumRecords ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumRecords,
      traceMaximumBytes: options.diagnostics?.traceMaximumBytes ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumBytes
    }
  )
  return createPublicBleManager(internal, env.now, {
    discoveryKind: 'system-chooser',
    choose: optionsValue => chooseWebPeer(backend, optionsValue, env.now)
  })
}

function normalizeWebCreateOptions(options: BleManagerCreateOptions | undefined): BleManagerCreateOptions {
  const normalized = normalizeBleManagerCreateOptions(options)
  if (normalized.adapterId !== undefined && normalized.adapterId !== WEB_BLUETOOTH_ADAPTER_ID) {
    throw contractError('adapter.unavailable', 'adapter', 'web-manager.adapter')
  }
  if (normalized.restoration !== undefined) {
    throw contractError('capability.unsupported', 'restoration', 'web-manager.restoration')
  }
  return normalized
}

async function chooseWebPeer(backend: WebBluetoothBackend, options: ChooseOptions, now: () => number) {
  assertPublicChooseOptions(options)
  const normalized = normalizeOperationOptions(options, now)
  const filters = options.filters ?? []
  const optionalServices = (options.optionalServices ?? []).map(normalizeChooserUuid)
  const browserFilters = filters.map(normalizeChooserFilter)
  const acceptAllDevices = options.acceptAllDevices ?? browserFilters.length === 0
  if ((acceptAllDevices && browserFilters.length > 0) || (!acceptAllDevices && browserFilters.length === 0)) {
    throw contractError('scan.filter-invalid', 'chooser', 'web.choose.selection-mode')
  }
  if (
    browserFilters.some(
      f => f.serviceUuids.length === 0 && f.manufacturerData.length === 0 && f.localNamePrefix === null
    )
  ) {
    throw contractError('scan.filter-invalid', 'chooser', 'web.choose.empty-criterion')
  }
  const selection = await backend.choose(
    {
      filters: browserFilters,
      acceptAllDevices,
      optionalServices
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

function normalizeChooserUuid(value: string | number): Uuid {
  return canonicalUuid(typeof value === 'number' ? value.toString(16) : value)
}

function normalizeChooserFilter(filter: ChooseFilter) {
  return {
    serviceUuids: (filter.serviceUuids ?? []).map(normalizeChooserUuid),
    manufacturerData: (filter.manufacturerData ?? []).map(manufacturer => ({
      companyIdentifier: manufacturer.companyIdentifier,
      dataPrefix: manufacturer.dataPrefix === undefined ? null : new Uint8Array(manufacturer.dataPrefix)
    })),
    localNamePrefix: filter.localNamePrefix ?? null
  }
}
