// __tests__/tck/rust-parity/rust-parity.test.js
//
// U7 parity slice: EQUAL contract-level observations ref-vs-Rust for every
// frozen base TCK scenario, driven against the REAL napi `.node` build of
// the transition-driving core (no mocks, no fallback backend).
//
// Two columns per scenario:
// - Closed (11): the scenario's BLE vocabulary runs through the
//   session-owned staged transition core from deterministic SYNTHETIC host
//   events only (no BLE hardware exists). The suite asserts EQUAL
//   observations: same data bytes, same error wires, same settlement
//   receipts, same effect sequencing, same bounded-batch accounting
//   (`src/tck/rust-driver/staged.ts` pins the frozen-rule expectations;
//   the untouched `ubm-core` is the independent oracle).
// - Stay-open (6): scenarios that genuinely need real radio (adapter
//   enumeration, peer handshakes, adapter state, trace sinks) keep pinning
//   their loud `capability.unsupported|capability` rejection AND a staged
//   probe proving the missing source. Never silently closed, never faked.
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
  RUST_PARITY_UNWIRED_WIRE,
  RUST_PARITY_PROVEN_MARKER,
  observeStagedScenario,
  observeStayOpenProbe,
  stagedProgramFor,
  stayOpenProbeFor,
  STAGED_PROGRAMS,
  STAY_OPEN_STAGED_PROBES
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
        '(run pnpm test:parity first): ' +
        (error && error.message)
    )
  }
  if (typeof addon.echoRevision !== 'function' || typeof addon.EchoSession !== 'function') {
    throw new Error(`U7 parity loaded an unexpected artifact at ${ADDON_PATH}: missing EchoSession surface`)
  }
  const probe = new addon.EchoSession(RUST_PARITY_REVISION)
  for (const method of ['stagedStep', 'stagedDrainLog', 'stagedCounters']) {
    if (typeof probe[method] !== 'function') {
      throw new Error(
        `U7 parity needs the staged drive surface at ${ADDON_PATH} (missing ${method}): ` +
          'rebuild the napi addon from this tree'
      )
    }
  }
  return addon
}

function wireOf(identity) {
  return `${identity.code}|${identity.domain}|${identity.operation}|${identity.detail}`
}

const BASE_DEFINITIONS = baseTckScenarios.filter(definition => definition.execution === 'base')
const PROVEN_IDS = RUST_PARITY_GAP_CANDIDATES.filter(candidate => candidate.status === 'transition-proven').map(
  candidate => candidate.scenarioId
)
const OPEN_IDS = RUST_PARITY_GAP_CANDIDATES.filter(
  candidate => candidate.status === 'stays-open-real-radio'
).map(candidate => candidate.scenarioId)

describe('U7 rust parity (ref-vs-Rust per base TCK scenario)', () => {
  test('the rust driver runs the same frozen base corpus as the reference runner', () => {
    expect(BASE_DEFINITIONS.length).toBe(17)
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

      // Uniform post-close lifetime on the Rust side (contract rule),
      // including the staged drive surface.
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
    }
  )

  test.each(STAGED_PROGRAMS.map(program => [program.scenarioId, program]))(
    '%s: staged transitions prove EQUAL observations ref-vs-Rust',
    (_scenarioId, program) => {
      const addon = loadRustAddon()
      const row = observeStagedScenario(addon, program)

      // Same scripted program on both columns, in order: data bytes, error
      // wires, settlement receipts, effect sequencing all EQUAL.
      expect(row.scenarioId).toBe(program.scenarioId)
      expect(row.observations.length).toBe(program.expected.length)
      for (let index = 0; index < program.expected.length; index += 1) {
        expect(row.observations[index]).toBe(program.expected[index])
      }

      // Bounded batches (<=64) with preserved dropped-not-staged
      // accounting: no scripted program drops anything silently. The
      // capability program is purely observational (no kernel admission),
      // so its staged_total is exactly 0; every transition-driving program
      // stages real kernel effects.
      const counters = JSON.parse(row.counters)
      expect(counters.cap).toBeLessThanOrEqual(64)
      expect(counters.dropped_not_staged).toBe(0)
      expect(counters.truncated_sweeps).toBe(0)
      if (program.scenarioId === 'capability.truth-limits-evidence-and-binding') {
        expect(counters.staged_total).toBe(0)
      } else {
        expect(counters.staged_total).toBeGreaterThan(0)
      }

      // The drained log carries every observation in order (sequencing
      // audit trail, raw kernel labels).
      expect(row.drained.split('\n').length).toBe(program.steps.length)
    }
  )

  test.each(STAY_OPEN_STAGED_PROBES.map(probe => [probe.scenarioId, probe]))(
    '%s: stays open with a loud rejection plus a sharpened staged probe',
    (_scenarioId, probe) => {
      const addon = loadRustAddon()

      // The unwired transition still rejects loudly with the frozen
      // identity (never silently closed, never faked).
      const definition = BASE_DEFINITIONS.find(entry => entry.id === probe.scenarioId)
      if (definition === undefined) {
        throw new Error(`probe scenario is not registered: ${probe.scenarioId}`)
      }
      const row = observeRustScenario(addon, definition)
      expect(row.gapTransition).toBe(gapTransitionFor(definition))
      expect(row.gap.ok).toBe(false)
      if (!row.gap.ok) {
        expect(wireOf(row.gap.error)).toBe(RUST_PARITY_UNWIRED_WIRE)
      }

      // The sharpened probe proves WHY through the staged surface: the
      // missing adapter/host source fails closed here too.
      const observations = observeStayOpenProbe(addon, probe)
      expect(observations.length).toBe(probe.expected.length)
      for (let index = 0; index < probe.expected.length; index += 1) {
        expect(observations[index]).toBe(probe.expected[index])
      }
    }
  )

  test('closed gaps resolve to transition proofs, open gaps stay loud', () => {
    const addon = loadRustAddon()
    const rows = runRustCorpus(addon)
    expect(rows.map(row => row.scenarioId)).toEqual(
      BASE_DEFINITIONS.map(definition => definition.id)
    )
    expect(PROVEN_IDS.length).toBe(STAGED_PROGRAMS.length)
    expect(OPEN_IDS.length).toBe(STAY_OPEN_STAGED_PROBES.length)
    expect(PROVEN_IDS.length + OPEN_IDS.length).toBe(BASE_DEFINITIONS.length)
    for (const candidate of RUST_PARITY_GAP_CANDIDATES) {
      const row = rows.find(entry => entry.scenarioId === candidate.scenarioId)
      if (row === undefined) {
        throw new Error(`parity row missing for candidate scenario ${candidate.scenarioId}`)
      }
      if (candidate.status === 'transition-proven') {
        // Closed: the staged program (not the rejection probe) is the
        // proof; the candidate records the program, never a wire.
        expect(candidate.observed).toBe(RUST_PARITY_PROVEN_MARKER)
        const program = stagedProgramFor(candidate.scenarioId)
        if (program === undefined) {
          throw new Error(`staged program missing for proven scenario ${candidate.scenarioId}`)
        }
        expect(candidate.transition).toBe(candidate.staged)
        const staged = observeStagedScenario(addon, program)
        expect(staged.observations).toEqual(program.expected)
      } else {
        // Open: the rejection pin still holds exactly, with the scenario
        // vocabulary that genuinely needs radio.
        expect(candidate.status).toBe('stays-open-real-radio')
        expect(row.gapTransition).toBe(candidate.transition)
        expect(row.gap.ok).toBe(false)
        if (!row.gap.ok) {
          expect(wireOf(row.gap.error)).toBe(candidate.observed)
        }
        expect(candidate.observed).toBe(RUST_PARITY_UNWIRED_WIRE)
        const probe = stayOpenProbeFor(candidate.scenarioId)
        if (probe === undefined) {
          throw new Error(`stay-open probe missing for open scenario ${candidate.scenarioId}`)
        }
        expect(observeStayOpenProbe(addon, probe)).toEqual(probe.expected)
      }
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
      if (candidate.status === 'transition-proven') {
        expect(candidate.observed).toBe(RUST_PARITY_PROVEN_MARKER)
        expect(candidate.contractResolution).toMatch('CLOSED by the staged-transition slice')
        const program = stagedProgramFor(candidate.scenarioId)
        if (program === undefined) {
          throw new Error(`staged program missing for proven scenario ${candidate.scenarioId}`)
        }
        expect(program.steps.length).toBe(program.expected.length)
        expect(program.steps.length).toBeGreaterThan(0)
      } else {
        expect(candidate.transition).toBe(gapTransitionFor(definition))
        expect(candidate.observed).toBe(RUST_PARITY_UNWIRED_WIRE)
        expect(candidate.contractResolution).toMatch('capability.unsupported|capability')
        expect(candidate.contractResolution).toMatch('Stay-open reason:')
      }
      expect(candidate.enforcedBy.length).toBeGreaterThan(0)
      expect(candidate.recordedAt.length).toBeGreaterThan(0)
      expect(candidate.staged.length).toBeGreaterThan(0)
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
