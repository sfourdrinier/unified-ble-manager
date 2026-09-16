// __tests__/tck/rust-parity/rust-parity.test.js
//
// U7 parity slice: EQUAL contract-level observations ref-vs-Rust for every
// frozen base TCK scenario, driven against the REAL napi `.node` build of
// the transition-driving core (no mocks, no fallback backend).
//
// Columns per scenario: ref (frozen contract text + the independent BigInt
// oracle transcribing it) vs Rust (fresh native session per scenario).
// BLE transitions beyond the driven slice disagree by design: the Rust side
// loudly reports the frozen capability.unsupported|capability identity, and
// each disagreement is pinned as an approved-correction candidate
// (RUST_PARITY_GAP_CANDIDATES) resolving to the actual contract — never a
// silent equality, never a weakened expectation.
//
// The real build must exist: a missing `.node` fails loudly with the exact
// remediation (run bindings/napi/run_napi_roundtrip.sh first).

const path = require('node:path')
const { baseTckScenarios } = require('../../../src/tck/scenarios')
const {
  gapTransitionFor,
  observeRustScenario,
  referenceCounter,
  runRustCorpus,
  rustParityBaseScenarios,
  RUST_PARITY_COUNTER_VECTORS,
  RUST_PARITY_FRESH_STATUS,
  RUST_PARITY_GAP_CANDIDATES,
  RUST_PARITY_MAX_BYTES,
  RUST_PARITY_REVISION,
  RUST_PARITY_UNWIRED_WIRE
} = require('../../../src/tck/rust-driver')

const ADDON_PATH =
  process.env.UBM_NAPI_ADDON ||
  path.join(__dirname, '..', '..', '..', 'bindings', 'napi', 'ubm_echo.linux-x64.node')

function loadRustAddon() {
  let addon
  try {
    addon = require(ADDON_PATH)
  } catch (error) {
    throw new Error(
      `U7 parity needs the real napi build at ${ADDON_PATH} ` +
        '(run bindings/napi/run_napi_roundtrip.sh first): ' +
        (error && error.message)
    )
  }
  if (typeof addon.echoRevision !== 'function' || typeof addon.EchoSession !== 'function') {
    throw new Error(`U7 parity loaded an unexpected artifact at ${ADDON_PATH}: missing EchoSession surface`)
  }
  return addon
}

function wireOf(identity) {
  return `${identity.code}|${identity.domain}|${identity.operation}|${identity.detail}`
}

const BASE_DEFINITIONS = baseTckScenarios.filter(definition => definition.execution === 'base')

describe('U7 rust parity (ref-vs-Rust per base TCK scenario)', () => {
  test('the rust driver runs the same frozen base corpus as the reference runner', () => {
    expect(BASE_DEFINITIONS.length).toBeGreaterThan(0)
    expect(rustParityBaseScenarios().map(definition => definition.id)).toEqual(
      BASE_DEFINITIONS.map(definition => definition.id)
    )
  })

  test.each(BASE_DEFINITIONS.map(definition => [definition.id, definition]))(
    '%s: contract-seam observations are EQUAL ref-vs-Rust',
    (_scenarioId, definition) => {
      const addon = loadRustAddon()
      const row = observeRustScenario(addon, definition)

      // Same frozen corpus on both columns (identity, facts, actions).
      expect(row.scenarioId).toBe(definition.id)
      expect(row.execution).toBe('base')
      expect(row.requiredFacts).toEqual(definition.requiredFacts)
      expect(row.requiredControllerActions).toEqual(definition.requiredControllerActions)

      // Frozen seam identity: revision + byte ceiling.
      expect(row.addonRevision).toBe(RUST_PARITY_REVISION)
      expect(row.addonMaxBytes).toBe(RUST_PARITY_MAX_BYTES)

      // Fresh transition core: real Kernel+Central, nothing live.
      expect(row.statusJson).toBe(RUST_PARITY_FRESH_STATUS)
      expect(JSON.parse(row.statusJson)).toEqual({
        revision: RUST_PARITY_REVISION,
        live_operations: 0,
        retained_cleanup: 0
      })

      // Real kernel sweeps settle nothing on a fresh central (twice).
      expect(row.sweepZero).toBe('0')
      expect(row.sweepMax).toBe('0')

      // Real shutdown transition: clean release, idempotent.
      expect(row.destroyFirst).toBe('released')
      expect(row.destroySecond).toBe('released')

      // Lossless u64 vectors: ref oracle and pinned expectation agree, and
      // the Rust observation equals both.
      expect(row.counters.length).toBe(RUST_PARITY_COUNTER_VECTORS.length)
      for (const probe of row.counters) {
        const reference = referenceCounter(probe.decimal)
        if (probe.expected.canonical !== null) {
          expect(reference).toEqual({ canonical: probe.expected.canonical })
          expect(probe.actual.ok).toBe(true)
          if (probe.actual.ok) {
            expect(probe.actual.value).toBe(probe.expected.canonical)
          }
        } else {
          expect(reference).toEqual({ rejectDetail: probe.expected.rejectDetail })
          expect(probe.actual.ok).toBe(false)
          if (!probe.actual.ok) {
            expect(probe.actual.error.code).toBe('bytes.invalid')
            expect(probe.actual.error.domain).toBe('core')
            expect(probe.actual.error.operation).toBe('echo-counter')
            expect(probe.actual.error.detail).toBe(probe.expected.rejectDetail)
          }
        }
      }

      // Uniform post-close lifetime on the Rust side (contract rule).
      expect(row.postCloseCounter.ok).toBe(false)
      if (!row.postCloseCounter.ok) {
        expect(wireOf(row.postCloseCounter.error)).toBe(
          'lifecycle.destroyed|core|echo-counter|session-closed'
        )
      }
      expect(row.postCloseStatus.ok).toBe(false)
      if (!row.postCloseStatus.ok) {
        expect(wireOf(row.postCloseStatus.error)).toBe(
          'lifecycle.destroyed|core|central-status|session-closed'
        )
      }
      expect(row.postCloseTransition.ok).toBe(false)
      if (!row.postCloseTransition.ok) {
        expect(wireOf(row.postCloseTransition.error)).toBe(
          'lifecycle.destroyed|core|request-ble-transition|session-closed'
        )
      }

      // Gap probe: scenario vocabulary, loud contract identity, no equality
      // with the reference backend (which performs the transition).
      expect(row.gapTransition).toBe(gapTransitionFor(definition))
      expect(row.gap.ok).toBe(false)
      if (!row.gap.ok) {
        expect(wireOf(row.gap.error)).toBe(RUST_PARITY_UNWIRED_WIRE)
      }
    }
  )

  test('ble gaps match their approved-correction candidates exactly', () => {
    const addon = loadRustAddon()
    const rows = runRustCorpus(addon)
    expect(rows.map(row => row.scenarioId)).toEqual(
      BASE_DEFINITIONS.map(definition => definition.id)
    )
    for (const candidate of RUST_PARITY_GAP_CANDIDATES) {
      const row = rows.find(entry => entry.scenarioId === candidate.scenarioId)
      if (row === undefined) {
        throw new Error(`parity row missing for candidate scenario ${candidate.scenarioId}`)
      }
      expect(row.gapTransition).toBe(candidate.transition)
      expect(row.gap.ok).toBe(false)
      if (!row.gap.ok) {
        expect(wireOf(row.gap.error)).toBe(candidate.observed)
      }
      expect(candidate.observed).toBe(RUST_PARITY_UNWIRED_WIRE)
    }
  })

  test('correction candidates cover exactly the frozen base corpus', () => {
    const candidateIds = RUST_PARITY_GAP_CANDIDATES.map(candidate => candidate.scenarioId).sort()
    const baseIds = BASE_DEFINITIONS.map(definition => definition.id).sort()
    expect(candidateIds).toEqual(baseIds)
    const correctionIds = RUST_PARITY_GAP_CANDIDATES.map(candidate => candidate.correctionId)
    expect(new Set(correctionIds).size).toBe(correctionIds.length)
    for (const candidate of RUST_PARITY_GAP_CANDIDATES) {
      const definition = BASE_DEFINITIONS.find(entry => entry.id === candidate.scenarioId)
      if (definition === undefined) {
        throw new Error(`candidate scenario is not registered: ${candidate.scenarioId}`)
      }
      expect(candidate.transition).toBe(gapTransitionFor(definition))
      expect(candidate.observed).toBe(RUST_PARITY_UNWIRED_WIRE)
      expect(candidate.contractResolution).toMatch('capability.unsupported|capability')
      expect(candidate.enforcedBy.length).toBeGreaterThan(0)
      expect(candidate.recordedAt.length).toBeGreaterThan(0)
    }
  })

  test('the reference oracle agrees with every pinned counter vector', () => {
    expect(RUST_PARITY_COUNTER_VECTORS.length).toBeGreaterThan(0)
    for (const vector of RUST_PARITY_COUNTER_VECTORS) {
      if (vector.canonical !== null) {
        expect(referenceCounter(vector.decimal)).toEqual({ canonical: vector.canonical })
      } else {
        expect(referenceCounter(vector.decimal)).toEqual({ rejectDetail: vector.rejectDetail })
      }
    }
  })
})
