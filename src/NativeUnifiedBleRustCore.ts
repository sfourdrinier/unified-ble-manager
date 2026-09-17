// src/NativeUnifiedBleRustCore.ts
//
// R01/D3(a) Codegen spec for the production React Native session facade
// over the shared Rust core. Implements exactly the F01 seam
// (src/backends/reactnative/react-native-rust-core.ts): openSession(owner)
// admits one native session; invoke routes one op verbatim; close releases.
// Args cross as a JSON string and results return the frozen C-UBM wire form
// (ok/value/code/domain/operation), so every rejection keeps its exact
// contract identity across the bridge. No TS scheduling, subscription, or
// timeout state lives behind this spec.

import type { TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface RustCoreSessionHandle {
  sessionId: string
}

export interface RustCoreInvokeResult {
  ok: boolean
  value: string
  code: string
  domain: string
  operation: string
}

export interface Spec extends TurboModule {
  openSession(owner: string): Promise<RustCoreSessionHandle>
  invoke(sessionId: string, op: string, argsJson: string): Promise<RustCoreInvokeResult>
  close(sessionId: string): Promise<void>
  contractRevision(): string
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleRustCore')
