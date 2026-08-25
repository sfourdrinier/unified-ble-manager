// src/node-corebluetooth.ts

import { BackendContractError, contractError } from './backend-contract/errors'
import {
  isAuthorizationBlocking,
  type BackendProvider,
  type HostNeutralBackendIdentity
} from './backend-contract/identity'
import type { CoreBluetoothBoundary } from './backends/corebluetooth/corebluetooth-boundary'
import {
  coreBluetoothCompatibility,
  createCoreBluetoothBackendProvider,
  type CoreBluetoothBackendProviderOptions
} from './backends/corebluetooth/corebluetooth-provider'
import { createNodeBleManagerFromProvider, type NodeBleManagerAppOptions } from './node-host-manager'
import { createPublicBleManager } from './public/ble-manager'
import type { BleManager } from './public/ble-manager'

interface CoreBluetoothNativeModule {
  createContractBoundary(): CoreBluetoothBoundary
}

export {
  COREBLUETOOTH_BACKEND_ID,
  COREBLUETOOTH_IMPLEMENTATION_VERSION,
  COREBLUETOOTH_PLATFORM_ID,
  coreBluetoothCompatibility,
  createCoreBluetoothBackendProvider
} from './backends/corebluetooth/corebluetooth-provider'
export type {
  CoreBluetoothAdapterSnapshot,
  CoreBluetoothAdvertisement,
  CoreBluetoothBoundary,
  CoreBluetoothCharacteristicAddress,
  CoreBluetoothCharacteristicRecord,
  CoreBluetoothDescriptorAddress,
  CoreBluetoothDescriptorRecord,
  CoreBluetoothGattSnapshot,
  CoreBluetoothServiceRecord
} from './backends/corebluetooth/corebluetooth-boundary'
export type { CoreBluetoothBackendProviderOptions } from './backends/corebluetooth/corebluetooth-provider'

export interface NativeCoreBluetoothProviderOptions {
  readonly now: () => number
}

const NATIVE_COREBLUETOOTH_INITIALIZATION_TIMEOUT_MILLISECONDS = 10_000

function isUsableAdapterState(state: ReturnType<CoreBluetoothBoundary['adapterSnapshot']>): boolean {
  return state.availability === 'available' && !isAuthorizationBlocking(state.authorization) && state.power === 'on'
}

/** Waits for CoreBluetooth's asynchronous first central-manager state callback before backend attachment. */
export function prepareNativeCoreBluetoothBoundary(boundary: CoreBluetoothBoundary): Promise<void> {
  return new Promise((resolve, reject) => {
    let settled = false
    let removeListener: (() => void) | null = null
    const timeout = setTimeout(() => {
      if (settled) return
      settled = true
      removeListener?.()
      reject(
        nativeArtifactUnavailable(
          'direct-gatt.native-boundary.initialize',
          'adapter-initialization-timed-out',
          'CoreBluetooth did not report a usable adapter state before the initialization deadline'
        )
      )
    }, NATIVE_COREBLUETOOTH_INITIALIZATION_TIMEOUT_MILLISECONDS)
    const accept = (state: ReturnType<CoreBluetoothBoundary['adapterSnapshot']>): void => {
      if (settled || !isUsableAdapterState(state)) return
      settled = true
      clearTimeout(timeout)
      removeListener?.()
      resolve()
    }
    const release = boundary.onAdapterState(accept)
    removeListener = release
    if (settled) {
      release()
      return
    }
    accept(boundary.adapterSnapshot())
  })
}

function nativeArtifactUnavailable(operation: string, code: string, safeMessage: string): BackendContractError {
  return contractError('capability.unavailable', 'platform', operation, {
    domain: 'corebluetooth',
    code,
    safeMessage,
    metadata: Object.freeze({})
  })
}

/** Loads the macOS-only direct CoreBluetooth addon for the current backend boundary. */
export function createNativeCoreBluetoothBoundary(): CoreBluetoothBoundary {
  if (process.platform !== 'darwin') {
    throw contractError('capability.unavailable', 'platform', 'direct-gatt.native-boundary.load', {
      domain: 'corebluetooth',
      code: 'macos-required',
      safeMessage: 'The CoreBluetooth backend is available only on macOS',
      metadata: Object.freeze({})
    })
  }
  let nativeModule: CoreBluetoothNativeModule
  try {
    nativeModule = require('../../native/electron/corebluetooth')
  } catch (error) {
    if (error instanceof BackendContractError) {
      throw error
    }
    throw nativeArtifactUnavailable(
      'direct-gatt.native-boundary.load',
      'native-artifact-unavailable',
      'The packaged CoreBluetooth native artifact could not be loaded for this Node or Electron runtime'
    )
  }
  try {
    return nativeModule.createContractBoundary()
  } catch (error) {
    if (error instanceof BackendContractError) {
      throw error
    }
    throw nativeArtifactUnavailable(
      'direct-gatt.native-boundary.create',
      'native-boundary-unavailable',
      'The CoreBluetooth native boundary could not be created for this macOS process'
    )
  }
}

export type { NodeBleManagerAppOptions }

/** One-call Node CoreBluetooth manager. Does not fall back to another backend. */
export async function createCoreBluetoothBleManager(options: NodeBleManagerAppOptions = {}): Promise<BleManager> {
  const now = options.now ?? (() => performance.now())
  const internal = await createNodeBleManagerFromProvider(
    createNativeCoreBluetoothBackendProvider({ now }),
    coreBluetoothCompatibility,
    options
  )
  return createPublicBleManager(internal, now)
}

/** Creates the production Node CoreBluetooth provider for the selected default central adapter. */
export function createNativeCoreBluetoothBackendProvider(
  options: NativeCoreBluetoothProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const providerOptions: CoreBluetoothBackendProviderOptions = {
    boundaryFactory: createNativeCoreBluetoothBoundary,
    prepareBoundary: prepareNativeCoreBluetoothBoundary,
    now: options.now,
    hostKind: 'node'
  }
  return createCoreBluetoothBackendProvider(providerOptions)
}
