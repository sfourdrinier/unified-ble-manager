// src/node-host-manager.ts

import { contractError } from './backend-contract/errors'
import type { AdapterDescriptor, BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import { opaqueId, type BackendCompatibilityOffer } from './backend-contract/primitives'
import { createEphemeralHostIdentity, normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS, type BleManager } from './manager/ble-manager'

export interface NodeBleManagerAppOptions extends BleManagerCreateOptions {
  readonly now?: () => number
}

export async function createNodeBleManagerFromProvider(
  provider: BackendProvider<string, HostNeutralBackendIdentity<string>>,
  compatibility: BackendCompatibilityOffer,
  options: NodeBleManagerAppOptions
): Promise<BleManager<string, HostNeutralBackendIdentity<string>>> {
  const { now = () => performance.now(), ...createOptions } = options
  normalizeBleManagerCreateOptions(createOptions)
  const adapters = await provider.listAdapters()
  const selected = selectNodeAdapter(adapters, options.adapterId)
  const ephemeral = createEphemeralHostIdentity()
  const instanceSuffix = options.instanceId === undefined ? '' : `-${options.instanceId}`
  return createBleManagerFromProvider(
    {
      provider,
      selection: { selectedAdapterId: selected.adapterId },
      coreCompatibility: compatibility,
      manager: {
        clientId: opaqueId(`node-${ephemeral.managerNonce}${instanceSuffix}`, 'client', 'node:host'),
        managerId: opaqueId(`node-${ephemeral.attachmentNonce}${instanceSuffix}`, 'manager', 'node:host'),
        ownerMode: 'owning'
      }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
  )
}

function selectNodeAdapter(
  adapters: readonly AdapterDescriptor<string>[],
  selectedAdapterId: string | undefined
): AdapterDescriptor<string> {
  if (selectedAdapterId !== undefined) {
    const match = adapters.find(adapter => String(adapter.adapterId) === selectedAdapterId)
    if (match === undefined) {
      throw contractError('adapter.unavailable', 'adapter', 'node-host-manager.selected-adapter')
    }
    return match
  }
  if (adapters.length === 0) {
    throw contractError('adapter.unavailable', 'adapter', 'node-host-manager.adapter')
  }
  if (adapters.length > 1) {
    throw contractError('adapter.ambiguous', 'adapter', 'node-host-manager.adapter')
  }
  const only = adapters[0]
  if (only === undefined) {
    throw contractError('adapter.unavailable', 'adapter', 'node-host-manager.adapter')
  }
  return only
}
