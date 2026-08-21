const { createPublicBleCapabilities } = require('../../src/public/capabilities')

function descriptor(id, scenario) {
  const range = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  const limitation = {
    code: 'deterministic-only',
    explanation: 'Physical-radio qualification is not claimed.',
    affectedGuarantee: 'support'
  }
  return {
    id,
    state: 'limited',
    selectedSchemaRange: range,
    implementationOrigin: 'backend-native',
    tck: { suiteId: 'capability.catalog-v2', requiredScenarioIds: [scenario], contractRange: range },
    evidence: {
      receiptId: 'trusted-receipt-1',
      evidenceLevel: 'deterministic',
      implementationVersion: 'host-4.0.0',
      sourceDigest: 'trusted-source-digest',
      scenarioIds: [scenario],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }
}

describe('trusted IPC capability bootstrap', () => {
  test('projects host descriptors without changing evidence or TCK data', () => {
    const source = descriptor('gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation')
    const capabilities = createPublicBleCapabilities(
      { schemaVersion: 2, backendGeneration: 'backend-generation-1', descriptors: [source] },
      'backend-generation-1'
    )

    expect(capabilities.get(source.id)).toEqual(source)
    expect(capabilities.get(source.id).evidence).toEqual(source.evidence)
    expect(capabilities.get(source.id).tck).toEqual(source.tck)
  })

  test('rejects a generation mismatch and duplicate descriptor', () => {
    const source = descriptor('gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation')
    expect(() =>
      createPublicBleCapabilities(
        { schemaVersion: 2, backendGeneration: 'other-generation', descriptors: [source] },
        'backend-generation-1'
      )
    ).toThrow('protocol.violation')

    expect(() =>
      createPublicBleCapabilities(
        { schemaVersion: 2, backendGeneration: 'backend-generation-1', descriptors: [source, source] },
        'backend-generation-1'
      )
    ).toThrow('protocol.violation')
  })
})
