const { createPublicBleCapabilities } = require('../../src/public/capabilities')
const { projectRemoteCapabilities } = require('../../src/ipc/manager')

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
  test('projects renderer-only control seams to unsupported until IPC routes them', () => {
    const source = descriptor('gatt:write-without-response-readiness', 'connection-controls')
    const projected = projectRemoteCapabilities({
      schemaVersion: 2,
      backendGeneration: 'backend-generation-1',
      descriptors: [source]
    })

    expect(projected.descriptors[0]).toMatchObject({
      id: source.id,
      state: 'unsupported',
      evidence: { sourceDigest: 'ipc-renderer-control-projection-v1' }
    })
  })

  test('finding 217 follow-up: the projection routes the effective MTU, so the native descriptor passes through', () => {
    const refused = descriptor('connection:effective-mtu', 'connection-controls')
    refused.state = 'unsupported'
    const nativeLimitation = {
      code: 'effective-mtu-boundary-unavailable',
      explanation: 'The dispatcher exposes no authoritative current ATT MTU observation.',
      affectedGuarantee: 'current effective ATT MTU observation'
    }
    refused.limitations = [nativeLimitation]
    const measured = descriptor('connection:effective-mtu', 'connection-controls')
    const projected = projectRemoteCapabilities({
      schemaVersion: 2,
      backendGeneration: 'backend-generation-1',
      descriptors: [refused, measured]
    })

    // A platform that genuinely cannot answer keeps `unsupported` with its
    // own precise reason — no renderer note appended.
    expect(projected.descriptors[0]).toMatchObject({
      id: refused.id,
      state: 'unsupported',
      limitations: [expect.objectContaining({ code: 'effective-mtu-boundary-unavailable' })]
    })
    expect(projected.descriptors[0].limitations).toHaveLength(1)
    // A platform that answers keeps `limited`: the renderer routes it now.
    expect(projected.descriptors[1]).toMatchObject({ id: measured.id, state: 'limited' })
  })

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
