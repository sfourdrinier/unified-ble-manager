// src/tck/rust-driver/index.ts
//
// U7 parity slice: contract-level observations of the Rust backend through
// the real napi binding, compared per frozen base TCK scenario.

export type {
  RustNativeAddon,
  RustNativeSession,
  RustStringOutcome,
  RustVoidOutcome,
  RustWireError
} from './rust-driver'
export { captureString, captureVoid, parseRustWireError, RustBackendDriver } from './rust-driver'
export type { RustCounterProbe, RustCounterVector, RustReferenceCounter, RustScenarioRow } from './corpus'
export {
  gapTransitionFor,
  observeRustScenario,
  referenceCounter,
  runRustCorpus,
  rustParityBaseScenarios,
  RUST_PARITY_COUNTER_VECTORS,
  RUST_PARITY_FRESH_STATUS,
  RUST_PARITY_MAX_BYTES,
  RUST_PARITY_REVISION,
  RUST_PARITY_UNWIRED_WIRE
} from './corpus'
export type { RustParityGapCandidate } from './correction-candidates'
export { RUST_PARITY_GAP_CANDIDATES } from './correction-candidates'
