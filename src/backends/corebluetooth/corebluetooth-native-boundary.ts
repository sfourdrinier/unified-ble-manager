// src/backends/corebluetooth/corebluetooth-native-boundary.ts
//
// LEGACY (5.0): the CoreBluetooth node-gyp addon loader for the TypeScript
// CoreBluetooth backend. No public entrypoint reaches it: the Node and
// Electron CoreBluetooth factories execute the shared Rust core
// (src/backends/desktop). Kept, unexported, until the Rust path is verified
// end to end; Phase 4 deletes it (see CHANGELOG, "legacy desktop backends").

import { BackendContractError, contractError } from '../../backend-contract/errors'
import { isAuthorizationBlocking } from '../../backend-contract/identity'
import type { CoreBluetoothBoundary } from './corebluetooth-boundary'
import { assertLegacyRequireAvailable } from '../legacy-native-require'

interface CoreBluetoothNativeModule {
  createContractBoundary(): CoreBluetoothBoundary
}

/**
 * Safety bound on CoreBluetooth's first `centralManagerDidUpdateState` callback.
 *
 * Fixed rather than caller-tunable because it runs during provider construction,
 * before any manager or operation exists to carry a `PublicOperationOptions`
 * deadline: there is no caller deadline to derive from at this point. The value
 * is a liveness guard, not a performance target -- a healthy macOS host reports
 * a usable state in well under a second, and exceeding ten seconds means the
 * native boundary is not going to answer. Widening it would only delay the
 * `capability.unavailable` report; it would not make a stalled boundary start.
 */
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
  assertLegacyRequireAvailable('direct-gatt.native-boundary.load', 'corebluetooth')
  let nativeModule: CoreBluetoothNativeModule
  try {
    nativeModule = require('../../../../native/electron/corebluetooth')
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
