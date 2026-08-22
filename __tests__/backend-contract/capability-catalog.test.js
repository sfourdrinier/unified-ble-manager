const {
  BUILT_IN_FEATURE_CATALOG,
  BUILT_IN_FEATURE_IDS,
  snapshotCapabilityDescriptors
} = require('../../src/backend-contract/capabilities')
const fs = require('fs')
const path = require('path')

describe('built-in capability catalog', () => {
  test('has one metadata entry for every canonical built-in ID', () => {
    const ids = Object.values(BUILT_IN_FEATURE_IDS)
    expect(BUILT_IN_FEATURE_CATALOG).toHaveLength(ids.length)
    expect(new Set(BUILT_IN_FEATURE_CATALOG.map(entry => entry.id)).size).toBe(ids.length)
    expect(BUILT_IN_FEATURE_CATALOG.map(entry => entry.id).sort()).toEqual([...ids].sort())
    for (const entry of BUILT_IN_FEATURE_CATALOG) {
      expect(entry.catalogVersion).toBe(2)
      expect(entry.documentationAnchor).toContain('capabilities.')
      expect(entry.requiredTckSuiteId).toBe('capability.catalog-v2')
    }
  })

  test('keeps the Tauri host manifest complete with the canonical ID set', () => {
    const rust = fs.readFileSync(path.join(__dirname, '../../native/tauri/src/capabilities.rs'), 'utf8')
    for (const id of Object.values(BUILT_IN_FEATURE_IDS)) {
      expect(rust).toContain(`"${id}"`)
    }
  })

  test('completes boundary snapshots with explicit blocked unsupported built-ins', () => {
    const snapshot = snapshotCapabilityDescriptors([], 'backend-generation-test')
    expect(snapshot.descriptors).toHaveLength(BUILT_IN_FEATURE_CATALOG.length)
    expect(snapshot.descriptors.every(descriptor => descriptor.state === 'unsupported')).toBe(true)
    expect(snapshot.descriptors.every(descriptor => descriptor.evidence.evidenceLevel === 'blocked')).toBe(true)
  })
})
