const { BUILT_IN_FEATURE_CATALOG, BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

describe('built-in capability catalog', () => {
  test('has one metadata entry for every canonical built-in ID', () => {
    const ids = Object.values(BUILT_IN_FEATURE_IDS)
    expect(BUILT_IN_FEATURE_CATALOG).toHaveLength(ids.length)
    expect(new Set(BUILT_IN_FEATURE_CATALOG.map(entry => entry.id)).size).toBe(ids.length)
    expect(BUILT_IN_FEATURE_CATALOG.map(entry => entry.id).sort()).toEqual([...ids].sort())
    for (const entry of BUILT_IN_FEATURE_CATALOG) {
      expect(entry.schemaVersion).toBe(2)
      expect(entry.documentationAnchor).toContain('capabilities.')
      expect(entry.requiredTckSuiteId).toBe('capability.catalog-v2')
    }
  })
})
