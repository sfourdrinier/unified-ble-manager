// src/backends/reactnative/react-native-rust-core-binding.ts
//
// R01/D3(a) production binding producer: resolves the `UnifiedBleRustCore`
// TurboModule (Kotlin over JNI / Swift over UniFFI) and adapts it to the
// F01 seam (`ReactNativeRustCoreBinding`). Resolution uses
// `TurboModuleRegistry.get` (nullable) — never the spec default's
// `getEnforcing` — so a missing native module fails loud with
// `capability.unsupported` instead of crashing the host.
//
// Value decoding: natives return the frozen C-UBM wire record as JSON
// strings. Successful values decode (JSON document → structured value,
// empty → null for quiet takes, non-JSON → verbatim string); failures
// throw `contractError` with the record's exact code/domain/operation, so
// the identity a provider sees is the identity Rust minted.

import { TurboModuleRegistry } from 'react-native'

import type { BleErrorCode, BleErrorDomain } from '../../backend-contract/errors'
import { contractError } from '../../backend-contract/errors'
import type { Spec } from '../../NativeUnifiedBleRustCore'
import type { ReactNativeRustCoreBinding, ReactNativeRustCoreSession } from './react-native-rust-core'

function decodeValue(value: string): unknown {
  if (value === '') return null
  try {
    return JSON.parse(value) as unknown
  } catch {
    return value
  }
}

/** Create the production binding, or fail loud when the native module is absent. */
export function createReactNativeRustCoreBinding(): ReactNativeRustCoreBinding {
  const native = TurboModuleRegistry.get<Spec>('UnifiedBleRustCore')
  if (native == null) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  return {
    openSession: async (owner: string): Promise<ReactNativeRustCoreSession> => {
      const { sessionId } = await native.openSession(owner)
      const revision = await native.contractRevision()
      return {
        contractRevision: () => revision,
        invoke: async (op: string, args: Record<string, unknown>): Promise<unknown> => {
          const outcome = await native.invoke(sessionId, op, JSON.stringify(args))
          if (!outcome.ok) {
            // Frozen-vocab identities minted by the core cross verbatim.
            throw contractError(outcome.code as BleErrorCode, outcome.domain as BleErrorDomain, outcome.operation)
          }
          return decodeValue(outcome.value)
        },
        close: async (): Promise<void> => {
          await native.close(sessionId)
        }
      }
    }
  }
}
