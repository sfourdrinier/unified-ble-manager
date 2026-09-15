// __tests__/tck/ubm5-draft-corrections.test.js
//
// UBM 5.0 TCK draft-correction log (TCK card; review follow-up).
// Test-first RED gate for the draft-corrections rescope of
// src/tck/approved-corrections.ts: the store holds DRAFT-side
// misunderstandings (the pinned reference was right), never approved
// reference-bug corrections, and every entry is enforced by a real assertion.

const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const { RACE_BOUNDS_CLEANUP_VECTOR_IDS, runRaceBoundsCleanupVector } = require('../../src/tck/race-bounds-cleanup-vectors')
const { baseTckScenarios } = require('../../src/tck/scenarios')
const { DRAFT_TCK_CORRECTIONS } = require('../../src/tck/approved-corrections')

describe('UBM5 TCK draft-correction log', () => {
  test('entries are draft-side misunderstandings with enforcing assertions', () => {
    expect(Array.isArray(DRAFT_TCK_CORRECTIONS)).toBe(true)
    expect(DRAFT_TCK_CORRECTIONS.length).toBeGreaterThan(0)
    for (const entry of DRAFT_TCK_CORRECTIONS) {
      expect(entry).toEqual(
        expect.objectContaining({
          correctionId: expect.any(String),
          vectorId: expect.any(String),
          scenarioId: expect.any(String),
          observed: expect.any(String),
          contractResolution: expect.any(String),
          enforcedBy: expect.any(String)
        })
      )
      expect(String(entry.correctionId).length).toBeGreaterThan(0)
      expect(String(entry.contractResolution).length).toBeGreaterThan(0)
      expect(String(entry.enforcedBy).length).toBeGreaterThan(0)
      expect(entry).not.toHaveProperty('referenceBugNoted')
    }
  })

  test('every entry links to a registered vector and a registered scenario', () => {
    const vectorIds = new Set(RACE_BOUNDS_CLEANUP_VECTOR_IDS)
    const scenarioIds = new Set(baseTckScenarios.map(definition => definition.id))
    expect(DRAFT_TCK_CORRECTIONS.length).toBeGreaterThan(0)
    for (const entry of DRAFT_TCK_CORRECTIONS) {
      expect(vectorIds.has(entry.vectorId)).toBe(true)
      expect(scenarioIds.has(entry.scenarioId)).toBe(true)
    }
  })

  test('duplicate-completion draft is enforced: exactly one settlement, one late trace', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'completion.duplicate-completion-settles-once')
    expect(observation.holds).toBe(true)
    expect(observation.detail.settledOnce).toBe(true)
    expect(observation.detail.lateAcknowledgements).toBe(1)
  })

  test('failed-cleanup draft is enforced: reported, retained, then cleanly released', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'cleanup.failed-cleanup-retains-and-reports')
    expect(observation.holds).toBe(true)
    expect(observation.detail.failureReported).toBe(true)
    expect(observation.detail.retained).toBe(true)
    expect(observation.detail.followUpReleased).toBe(true)
  })

  test('no unwired correction lookup survives (findApprovedCorrection is deleted)', () => {
    const correctionsModule = require('../../src/tck/approved-corrections')
    expect(correctionsModule.findApprovedCorrection).toBe(undefined)
    expect(correctionsModule.findDraftCorrection).toBe(undefined)
  })
})
