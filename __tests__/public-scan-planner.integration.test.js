const { normalizeScanQuery } = require('../src/public/scan-query')
const { createDeterministicTestBleManager } = require('../src/testing/deterministic/deterministic-test-manager')

async function settleDeterministic(fixture, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 20 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

describe('public deterministic scan planning', () => {
  test('exposes the backend-owned safe-superset plan without changing scan defaults', async () => {
    const { manager, fixture } = await createDeterministicTestBleManager()
    const query = { anyOf: [{ names: { prefixes: ['Heart'] } }] }
    let scan
    try {
      scan = await settleDeterministic(fixture, manager.scan({ query }))
      const normalizedQuery = normalizeScanQuery(query)

      expect(scan.plan).not.toBeNull()
      expect(scan.plan.nativeGuarantee).toBe('safe-superset')
      expect(scan.plan.queryDigest).toBe(normalizedQuery.digest)
      expect(scan.plan.residualQueryDigest).toBe(normalizedQuery.digest)
      expect(scan.plan.sourceQuery).toStrictEqual(scan.plan.residual.query)
      expect(scan.plan.nativeFilter).toBeUndefined()
    } finally {
      if (scan !== undefined) await settleDeterministic(fixture, scan.stop())
      await settleDeterministic(fixture, manager.destroy())
    }
  })
})
