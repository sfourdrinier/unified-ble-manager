const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../src/public/scan-query')
const { snapshotScanPlan } = require('../../src/backend-contract/scan-planning')
const vectors = require('./fixtures/scan-query-pr9-planner.golden.json')

function hydrate(value, key) {
  if (value !== null && typeof value === 'object' && value.$bytes !== undefined) {
    return new Uint8Array(value.$bytes)
  }
  if (Array.isArray(value)) return value.map(item => hydrate(item, key))
  if (value !== null && typeof value === 'object') {
    const result = {}
    for (const [entryKey, entryValue] of Object.entries(value)) {
      result[entryKey] = hydrate(entryValue, entryKey)
    }
    return result
  }
  return value
}

describe('PR9 scan planner contract and canonical oracle corpus', () => {
  test('retains PR4 normalization, digest, and matcher expectations as immutable oracle vectors', () => {
    expect(vectors.length).toBeGreaterThanOrEqual(4)
    expect(new Set(vectors.map(vector => vector.id)).size).toBe(vectors.length)

    for (const vector of vectors) {
      const query = hydrate(vector.query)
      const observation = hydrate(vector.observation)
      const normalizedQuery = normalizeScanQuery(query)
      const normalizedObservation = normalizeScanObservation(observation)

      process.stdout.write(`${vector.id}=${normalizedQuery.digest}\n`)

      if (vector.normalizedQueryDigest !== '') {
        expect(normalizedQuery.digest).toBe(vector.normalizedQueryDigest)
      }
      expect(observationMatchesScanQuery(normalizedQuery, normalizedObservation)).toBe(vector.expectedMatch)
    }
  })

  test('uses the canonical matcher as the residual oracle for a safe-superset plan', () => {
    for (const vector of vectors) {
      const normalizedQuery = normalizeScanQuery(hydrate(vector.query))
      const normalizedObservation = normalizeScanObservation(hydrate(vector.observation))
      const plan = snapshotScanPlan({
        queryDigest: normalizedQuery.digest,
        nativeGuarantee: 'safe-superset',
        native: { predicates: [], complete: false },
        residual: { query: normalizedQuery, predicates: [], complete: true },
        unavailable: [],
        limitations: [],
        estimatedCost: 'high'
      })

      const optimizedMatch =
        plan.nativeGuarantee === 'safe-superset' &&
        observationMatchesScanQuery(plan.residual.query, normalizedObservation)
      expect(optimizedMatch).toBe(vector.expectedMatch)
    }
  })

  test('keeps the canonical residual query and snapshots bounded diagnostic projections', () => {
    const residualQuery = normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Heart'] } }] })
    const plan = snapshotScanPlan({
      queryDigest: residualQuery.digest,
      nativeGuarantee: 'safe-superset',
      native: {
        predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'names', operator: 'prefixes' }],
        complete: false
      },
      residual: {
        query: residualQuery,
        predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'names', operator: 'prefixes' }],
        complete: true
      },
      unavailable: [],
      limitations: [
        {
          code: 'native-name-filter-unavailable',
          predicate: 'anyOf[0].names.prefixes',
          explanation: 'name prefix remains in the canonical residual matcher',
          effect: 'performance-only'
        }
      ],
      estimatedCost: 'moderate'
    })

    expect(plan.residual.query).toBe(residualQuery)
    expect(plan.nativeGuarantee).toBe('safe-superset')
    expect(plan.native.complete).toBe(false)
    expect(plan.residual.complete).toBe(true)
    expect(Object.isFrozen(plan)).toBe(true)
    expect(Object.isFrozen(plan.native)).toBe(true)
    expect(Object.isFrozen(plan.residual)).toBe(true)
    expect(Object.isFrozen(plan.limitations)).toBe(true)
    expect(JSON.stringify(plan.limitations)).not.toContain('manufacturerData')
    expect(() => snapshotScanPlan({ ...plan, queryDigest: 'scan-query-v1:0000000000000000' })).toThrow(
      'residual query digest'
    )
  })

  test('rejects malformed predicate diagnostics before they can cross a host boundary', () => {
    const residualQuery = normalizeScanQuery()
    expect(() =>
      snapshotScanPlan({
        queryDigest: residualQuery.digest,
        nativeGuarantee: 'safe-superset',
        native: {
          predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'not-a-field', operator: 'exact' }],
          complete: false
        },
        residual: { query: residualQuery, predicates: [], complete: true },
        unavailable: [],
        limitations: [],
        estimatedCost: 'low'
      })
    ).toThrow('invalid predicate')
  })
})
