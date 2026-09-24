const { BUILT_IN_FEATURE_IDS } = require('../../../src/backend-contract/capabilities')

function capabilitySnapshot(backendGeneration) {
  const schema = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  const limitation = {
    code: 'not-implemented',
    explanation: 'fixture capability is not implemented',
    affectedGuarantee: 'support'
  }
  return {
    schemaVersion: 2,
    backendGeneration,
    descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
      id,
      state: id === 'connection:direct' || id === 'security:state' ? 'limited' : 'unsupported',
      selectedSchemaRange: schema,
      implementationOrigin: 'backend-native',
      tck: {
        suiteId: 'capability.catalog-v2',
        requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
        contractRange: schema
      },
      evidence: {
        receiptId: `test-${id}`,
        evidenceLevel: id === 'connection:direct' || id === 'security:state' ? 'deterministic' : 'blocked',
        implementationVersion: 'test',
        sourceDigest: `test-${id}`,
        scenarioIds: ['capability.truth-limits-evidence-and-binding'],
        limitations: [limitation]
      },
      limitations: [limitation],
      limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
    }))
  }
}

function bootstrap() {
  const attachment = {
    attachmentId: 'electron-attachment-1',
    backendInstanceId: 'electron-backend-1',
    backendGeneration: 'electron-generation-1',
    adapter: {
      adapterId: 'electron-adapter-1',
      displayName: 'test',
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration: 'electron-generation-1',
        updatedAt: 1,
        safeReason: null
      },
      adapterGeneration: 'electron-adapter-generation-1',
      limitations: []
    }
  }
  const version = axis => ({
    axis,
    selected: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
    localRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
    },
    remoteRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
    }
  })
  return {
    attachment,
    attachmentId: attachment.attachmentId,
    versions: {
      backendContract: version('backend-contract'),
      capabilitySchema: version('capability-schema'),
      eventSchema: version('event-schema'),
      traceFormat: version('trace-format'),
      ipcProtocol: version('ipc-protocol')
    },
    capabilities: capabilitySnapshot(attachment.backendGeneration),
    renderer: { clientId: 'renderer-client-1', windowScope: 'window-1', sessionScope: 'session-1' },
    rendererLease: { leaseId: 'renderer-lease-1', generation: 'renderer-lease-generation-1' }
  }
}

module.exports = { bootstrap }
