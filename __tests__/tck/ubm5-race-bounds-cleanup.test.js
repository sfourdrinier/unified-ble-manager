// __tests__/tck/ubm5-race-bounds-cleanup.test.js
//
// UBM 5.0 TCK race/bounds/cleanup vectors (TCK card; enables U2/U3/U7).
// Test-first RED gate for src/tck/race-bounds-cleanup-vectors.ts + approved-corrections.ts.

const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const {
  RACE_BOUNDS_CLEANUP_VECTOR_IDS,
  runRaceBoundsCleanupVector
} = require('../../src/tck/race-bounds-cleanup-vectors')
const { APPROVED_TCK_CORRECTIONS } = require('../../src/tck/approved-corrections')

const EXPECTED_VECTORS = [
  'cleanup.failed-cleanup-retains-and-reports',
  'cleanup.duplicate-destroy-is-idempotent',
  'generation.stale-path-rejects-before-dispatch',
  'completion.duplicate-completion-settles-once',
  'bounds.subscription-overflow-is-bounded-and-terminal',
  'cancel.admission-completion-boundary-settles-once',
  'invalidation.service-change-invalidates-generation'
]

describe('UBM5 TCK race/bounds/cleanup vectors', () => {
  test('registers every required vector id exactly once', () => {
    expect([...RACE_BOUNDS_CLEANUP_VECTOR_IDS].sort()).toEqual([...EXPECTED_VECTORS].sort())
  })

  test.each(EXPECTED_VECTORS)('%s holds against the pinned TS reference', async vectorId => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, vectorId)
    expect(observation.vectorId).toBe(vectorId)
    expect(observation.holds).toBe(true)
    expect(observation.detail).toBeDefined()
  })

  test('duplicate completion does not double-settle (counterexample first)', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'completion.duplicate-completion-settles-once')
    expect(observation.holds).toBe(true)
    expect(observation.detail.settledOnce).toBe(true)
    expect(observation.detail.noDoubleSettlement).toBe(true)
  })

  test('stale generation rejects before dispatch (counterexample first)', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'generation.stale-path-rejects-before-dispatch')
    expect(observation.holds).toBe(true)
    expect(observation.detail.staleRejected).toBe(true)
    expect(observation.detail.currentStillReads).toBe(true)
  })

  test('approved-correction log is a separate store with contract resolutions', () => {
    expect(Array.isArray(APPROVED_TCK_CORRECTIONS)).toBe(true)
    for (const entry of APPROVED_TCK_CORRECTIONS) {
      expect(entry).toEqual(
        expect.objectContaining({
          vectorId: expect.any(String),
          scenarioId: expect.any(String),
          observed: expect.any(String),
          contractResolution: expect.any(String),
          referenceBugNoted: expect.any(Boolean)
        })
      )
      expect(String(entry.contractResolution).length).toBeGreaterThan(0)
    }
  })
})
