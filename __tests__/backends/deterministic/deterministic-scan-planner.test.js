const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const { describeScanPredicates } = require('../../../src/backend-contract/scan-planning')
const { DeterministicScanPlanner } = require('../../../src/testing/deterministic/deterministic-scan-planner')
const vectors = require('../../backend-contract/fixtures/scan-query-pr9-planner.golden.json')

function hydrate(value) {
  if (value !== null && typeof value === 'object' && value.$bytes !== undefined) {
    return new Uint8Array(value.$bytes)
  }
  if (Array.isArray(value)) return value.map(item => hydrate(item))
  if (value !== null && typeof value === 'object') {
    const result = {}
    for (const [entryKey, entryValue] of Object.entries(value)) {
      result[entryKey] = hydrate(entryValue)
    }
    return result
  }
  return value
}

const context = {
  backendId: 'deterministic',
  platformId: 'deterministic-test',
  availableObservationFields: [
    'peerReference',
    'localName',
    'rssi',
    'connectable',
    'serviceUuids',
    'manufacturerData',
    'serviceData'
  ]
}

describe('deterministic scan planner', () => {
  test('preserves canonical results with a match-all native filter for every PR9 vector', () => {
    const planner = new DeterministicScanPlanner()

    for (const vector of vectors) {
      const normalizedQuery = normalizeScanQuery(hydrate(vector.query))
      const normalizedObservation = normalizeScanObservation(hydrate(vector.observation))
      const plan = planner.plan(normalizedQuery, context)
      const nativeMatchAll =
        plan.nativeFilter.serviceUuids.length === 0 &&
        plan.nativeFilter.manufacturerData.length === 0 &&
        plan.nativeFilter.localNamePrefix === null
      const optimizedMatch = nativeMatchAll && observationMatchesScanQuery(plan.residual.query, normalizedObservation)

      expect(optimizedMatch).toBe(vector.expectedMatch)
      expect(plan.nativeGuarantee).toBe('safe-superset')
      expect(plan.sourceQuery).toStrictEqual(plan.residual.query)
      expect(plan.queryDigest).toBe(normalizedQuery.digest)
      expect(plan.residualQueryDigest).toBe(normalizedQuery.digest)
      expect(plan.native).toStrictEqual({ predicates: [], complete: false })
      expect(plan.residual.predicates).toStrictEqual(describeScanPredicates(normalizedQuery))
      expect(plan.residual.complete).toBe(true)
      expect(plan.unavailable).toStrictEqual([])
      expect(plan.limitations).toStrictEqual([])

      const diagnostics = JSON.stringify({
        native: plan.native,
        residual: { predicates: plan.residual.predicates, complete: plan.residual.complete },
        unavailable: plan.unavailable,
        limitations: plan.limitations
      })
      expect(diagnostics).not.toContain('Heart Strap')
      expect(diagnostics).not.toContain('010203')
      expect(diagnostics).not.toContain('2,21,1,2')
    }
  })

  test('rejects malformed planning context before producing a plan', () => {
    const query = normalizeScanQuery()
    const planner = new DeterministicScanPlanner()

    expect(() =>
      planner.plan(query, {
        ...context,
        availableObservationFields: ['not-an-observation-field']
      })
    ).toThrow('planning context')
  })
})
