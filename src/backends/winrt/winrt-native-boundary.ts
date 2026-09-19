// src/backends/winrt/winrt-native-boundary.ts
//
// LEGACY (5.0): the WinRT node-gyp addon loader for the TypeScript WinRT
// backend. No public entrypoint reaches it: the Node and Electron WinRT
// factories execute the shared Rust core (src/backends/desktop). Kept,
// unexported, until the Rust path is verified end to end; Phase 4 deletes it
// (see CHANGELOG, "legacy desktop backends").

import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { WinRtBoundary } from './winrt-boundary'
import { assertLegacyRequireAvailable } from '../legacy-native-require'

interface WinRtNativeModule {
  readonly boundaryVersion: 2
  createContractBoundary(): WinRtBoundary
}

const requiredNativeBoundaryMethods = [
  'listAdapters',
  'selectAdapter',
  'adapterSnapshot',
  'startScan',
  'stopScan',
  'connect',
  'disconnect',
  'discover',
  'read',
  'write',
  'readDescriptor',
  'writeDescriptor',
  'startNotify',
  'stopNotify',
  'onConnectionLost',
  'onDatabaseChanged',
  'onAdapterState',
  'onSecurityState',
  'onScanTerminal',
  'securityState',
  'pair',
  'cancelPairing',
  'unpair',
  'ingressTelemetry',
  'destroy'
]

function missingNativeBoundaryMethod(boundary: WinRtBoundary): string | null {
  for (const method of requiredNativeBoundaryMethods) {
    if (typeof Reflect.get(boundary, method) !== 'function') {
      return method
    }
  }
  return null
}

function nativeArtifactUnavailable(operation: string, code: string, safeMessage: string): BackendContractError {
  return contractError('capability.unavailable', 'platform', operation, {
    domain: 'winrt',
    code,
    safeMessage,
    metadata: Object.freeze({})
  })
}

/** Loads only the package-controlled Windows Node-API artifact and never substitutes a test radio. */
export function createNativeWinRtBoundary(): WinRtBoundary {
  if (process.platform !== 'win32') {
    throw contractError('capability.unavailable', 'platform', 'winrt.native-boundary.load', {
      domain: 'winrt',
      code: 'windows-required',
      safeMessage: 'The WinRT backend is available only on Windows',
      metadata: Object.freeze({})
    })
  }
  assertLegacyRequireAvailable('winrt.native-boundary.load', 'winrt')
  let nativeModule: WinRtNativeModule
  try {
    nativeModule = require('../../../../native/electron/winrt')
  } catch (error) {
    if (error instanceof BackendContractError) {
      throw error
    }
    throw nativeArtifactUnavailable(
      'winrt.native-boundary.load',
      'native-artifact-unavailable',
      'The packaged WinRT native artifact could not be loaded for this Node or Electron runtime'
    )
  }
  if (nativeModule.boundaryVersion !== 2 || typeof nativeModule.createContractBoundary !== 'function') {
    throw contractError('protocol.incompatible', 'boundary', 'winrt.native-boundary.version', {
      domain: 'winrt',
      code: 'native-protocol-version',
      safeMessage: 'The packaged WinRT native artifact does not implement boundary protocol v2',
      metadata: Object.freeze({})
    })
  }
  try {
    const boundary = nativeModule.createContractBoundary()
    const missingMethod = missingNativeBoundaryMethod(boundary)
    if (missingMethod !== null) {
      throw contractError('protocol.incompatible', 'boundary', 'winrt.native-boundary.surface', {
        domain: 'winrt',
        code: 'native-boundary-surface',
        safeMessage: `The packaged WinRT native boundary is missing required protocol v2 method ${missingMethod}`,
        metadata: Object.freeze({ missingMethod })
      })
    }
    return boundary
  } catch (error) {
    if (error instanceof BackendContractError) {
      throw error
    }
    throw nativeArtifactUnavailable(
      'winrt.native-boundary.create',
      'native-boundary-unavailable',
      'The WinRT native boundary could not be created for this Windows process'
    )
  }
}
