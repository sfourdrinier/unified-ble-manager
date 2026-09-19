// src/tck/rust-driver/corpus.ts
//
// U7 parity slice: runs the FROZEN TCK corpus (`baseTckScenarios`, the same
// source the reference runner uses) against the Rust backend and records
// contract-level observations per scenario.
//
// Provenance (read-only; nothing here is edited):
// - Corpus: `src/tck/scenarios.ts` (`baseTckScenarios`).
// - Revision pin `C-UBM.0.1.2-DRAFT`: frozen `contracts/src/version.ts`
//   (`CONTRACT_REVISION`) and `ubm_core::contracts::CONTRACT_REVISION`.
// - Byte ceiling `524288`: frozen `contracts/src/bounds.ts`
//   (`MAX_OPERATION_BYTES`) and `ubm_core::contracts::MAX_OPERATION_BYTES`.
// - Decimal-string u64 grammar (`^[0-9]+$`, at most 20 digits, range to
//   `2^64 - 1`; shape violations are `u64.input`, overflow is `u64.range`):
//   frozen `contracts/src/bounds.ts` (`parseU64Decimal`, `MAX_DECIMAL_DIGITS`)
//   and `ubm_core::contracts::parse_u64_decimal`.
// - `capability.unsupported|capability` pairing: frozen
//   `contracts/src/capabilities.ts` and `contracts/src/central.ts`
//   (`contractError('capability.unsupported', 'capability', ...)`).
//
// The reference oracle below transcribes that frozen grammar with an
// independent mechanism (JS `BigInt`) so the parity tests compare two
// implementations of one frozen rule. It is a cross-check only: it never
// admits or rejects production traffic.

import type { TckScenarioDefinition } from '../contracts'
import { baseTckScenarios } from '../scenarios'
import type { RustNativeAddon, RustStringOutcome, RustVoidOutcome } from './rust-driver'
import { RustBackendDriver } from './rust-driver'

/** Frozen contract revision spoken by both backends. */
export const RUST_PARITY_REVISION = 'C-UBM.0.1.2-DRAFT'

/** Frozen maximum byte-batch length observed on both backends. */
export const RUST_PARITY_MAX_BYTES = 524288

/** Exact status document a fresh transition core reports on every binding. */
export const RUST_PARITY_FRESH_STATUS = '{"revision":"C-UBM.0.1.2-DRAFT","live_operations":0,"retained_cleanup":0}'

/** Exact wire identity of the unwired-transition loud rejection. */
export const RUST_PARITY_UNWIRED_WIRE =
  'capability.unsupported|capability|request-ble-transition|transition-not-wired-in-u7-slice'

/** One decimal-string u64 probe with its frozen expectation. */
export interface RustCounterVector {
  readonly decimal: string
  /** Canonical form on success; `null` when the input must reject. */
  readonly canonical: string | null
  /** Rejection detail on failure; `null` when the input must succeed. */
  readonly rejectDetail: 'u64.input' | 'u64.range' | null
}

/**
 * Pinned counter vectors (same battery the binding exchanges prove, plus the
 * 21-digit over-cap input the frozen digit cap rejects before range checks).
 */
export const RUST_PARITY_COUNTER_VECTORS: readonly RustCounterVector[] = Object.freeze([
  { decimal: '0', canonical: '0', rejectDetail: null },
  { decimal: '1', canonical: '1', rejectDetail: null },
  { decimal: '00042', canonical: '42', rejectDetail: null },
  { decimal: '9007199254740993', canonical: '9007199254740993', rejectDetail: null },
  { decimal: '9223372036854775807', canonical: '9223372036854775807', rejectDetail: null },
  { decimal: '18446744073709551615', canonical: '18446744073709551615', rejectDetail: null },
  { decimal: '', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '-1', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '+5', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '12a34', canonical: null, rejectDetail: 'u64.input' },
  { decimal: ' 42', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '4.0', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '0x10', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '123456789012345678901', canonical: null, rejectDetail: 'u64.input' },
  { decimal: '18446744073709551616', canonical: null, rejectDetail: 'u64.range' }
])

/** Independent reference outcome for one decimal input. */
export type RustReferenceCounter = { readonly canonical: string } | { readonly rejectDetail: 'u64.input' | 'u64.range' }

/**
 * Independent reference oracle transcribing the frozen decimal-string u64
 * grammar (`^[0-9]+$`, at most 20 digits, range to `2^64 - 1`) with JS
 * `BigInt` semantics — a different mechanism from the Rust `checked_mul`
 * loop enforcing the identical frozen rule. Cross-check only (see module
 * note); pinned vectors above carry the enforced expectations.
 */
export function referenceCounter(decimal: string): RustReferenceCounter {
  if (/^[0-9]+$/u.test(decimal) === false || decimal.length > 20) {
    return Object.freeze({ rejectDetail: 'u64.input' })
  }
  const parsed = BigInt(decimal)
  if (parsed > 18446744073709551615n) {
    return Object.freeze({ rejectDetail: 'u64.range' })
  }
  return Object.freeze({ canonical: parsed.toString() })
}

/** Base scenarios of the frozen corpus (the runner's default selection). */
export function rustParityBaseScenarios(): readonly TckScenarioDefinition[] {
  return baseTckScenarios.filter(definition => definition.execution === 'base')
}

/**
 * Representative unwired BLE transition probed for one scenario: its first
 * required controller action when it names one (scenario-specific/collective
 * vocabulary), else `scan.start` (the canonical radio transition this
 * boundary cannot perform).
 */
export function gapTransitionFor(definition: TckScenarioDefinition): string {
  const first = definition.requiredControllerActions[0]
  return first === undefined ? 'scan.start' : first
}

/** One counter probe with its frozen expectation and Rust observation. */
export interface RustCounterProbe {
  readonly decimal: string
  readonly expected: RustCounterVector
  readonly actual: RustStringOutcome
}

/** Contract-level observations of the Rust backend for one base scenario. */
export interface RustScenarioRow {
  readonly scenarioId: string
  readonly execution: 'base'
  readonly requiredFacts: readonly string[]
  readonly requiredControllerActions: readonly string[]
  readonly addonRevision: string
  readonly addonMaxBytes: number
  readonly statusJson: string
  readonly sweepZero: string
  readonly sweepMax: string
  readonly destroyFirst: string
  readonly destroySecond: string
  readonly counters: readonly RustCounterProbe[]
  readonly gapTransition: string
  readonly gap: RustVoidOutcome
  readonly postCloseCounter: RustStringOutcome
  readonly postCloseStatus: RustStringOutcome
  readonly postCloseTransition: RustVoidOutcome
}

/**
 * Observes the Rust backend for one frozen base scenario on a FRESH native
 * session (one session per scenario, closed before return). Must-succeed
 * probes return raw values and let native rejections propagate (an
 * unexpected regression fails its scenario loudly); expected-rejection
 * probes are captured for exact-identity assertions.
 */
export function observeRustScenario(addon: RustNativeAddon, definition: TckScenarioDefinition): RustScenarioRow {
  const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
  const statusJson = driver.status()
  const sweepZero = driver.sweep('0')
  const sweepMax = driver.sweep('18446744073709551615')
  const destroyFirst = driver.destroy()
  const destroySecond = driver.destroy()
  const counters: RustCounterProbe[] = RUST_PARITY_COUNTER_VECTORS.map(vector =>
    Object.freeze({ decimal: vector.decimal, expected: vector, actual: driver.counter(vector.decimal) })
  )
  const gapTransition = gapTransitionFor(definition)
  const gap = driver.bleTransition(gapTransition)
  driver.close()
  driver.close()
  const postCloseCounter = driver.postCloseCounter('1')
  const postCloseStatus = driver.postCloseStatus()
  const postCloseTransition = driver.postCloseTransition(gapTransition)
  return Object.freeze({
    scenarioId: definition.id,
    execution: 'base',
    requiredFacts: definition.requiredFacts,
    requiredControllerActions: definition.requiredControllerActions,
    addonRevision: addon.echoRevision(),
    addonMaxBytes: addon.echoMaxBytes(),
    statusJson,
    sweepZero,
    sweepMax,
    destroyFirst,
    destroySecond,
    counters: Object.freeze(counters),
    gapTransition,
    gap,
    postCloseCounter,
    postCloseStatus,
    postCloseTransition
  })
}

/** Observes the Rust backend for every frozen base scenario (corpus order). */
export function runRustCorpus(addon: RustNativeAddon): readonly RustScenarioRow[] {
  return Object.freeze(rustParityBaseScenarios().map(definition => observeRustScenario(addon, definition)))
}
