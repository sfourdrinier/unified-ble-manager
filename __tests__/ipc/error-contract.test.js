const fs = require('fs')
const path = require('path')
const { IpcBleManager } = require('../../src/ipc/manager')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

function negotiated(axis, value = axis === 'ipc-protocol' ? 2 : 1) {
  const selected = { axis, value }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function bootstrapRecord() {
  const backendGeneration = 'backend-generation-1'
  return {
    attachment: {
      attachmentId: 'attachment-1',
      backendInstanceId: 'backend-1',
      backendGeneration,
      adapter: {
        adapterId: 'adapter-1',
        displayName: 'Bluetooth',
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: null,
          backendGeneration,
          updatedAt: 1,
          safeReason: null
        },
        adapterGeneration: 'adapter-generation-1',
        limitations: []
      }
    },
    attachmentId: 'attachment-1',
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
        id,
        state: 'unsupported',
        selectedSchemaRange: negotiated('capability-schema').localRange,
        implementationOrigin: 'backend-native',
        tck: {
          suiteId: 'capability.catalog-v2',
          requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
          contractRange: negotiated('capability-schema').localRange
        },
        evidence: {
          receiptId: `fixture-${id}`,
          evidenceLevel: 'blocked',
          implementationVersion: 'fixture',
          sourceDigest: `fixture-${id}`,
          scenarioIds: ['capability.truth-limits-evidence-and-binding'],
          limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }]
        },
        limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }],
        limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
      }))
    },
    renderer: { clientId: 'client-1', windowScope: 'window', sessionScope: 'session' },
    rendererLease: { leaseId: 'lease-1', generation: 'lease-generation-1' }
  }
}

function isRecord(value) {
  return typeof value === 'object' && value !== null
}

async function createManager(options = {}) {
  const bootstrap = bootstrapRecord()
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: options.routePayload ?? {} }
    },
    subscribe() {
      return () => undefined
    },
    acknowledge: async () => ({ kind: 'event.ack' })
  }
  return IpcBleManager.create(transport)
}

describe('shared IPC error contract', () => {
  test('framework-neutral IPC sources do not name Tauri or Electron in error text', () => {
    const root = path.join(__dirname, '../../src/ipc')
    const files = fs.readdirSync(root).filter(name => name.endsWith('.ts'))
    for (const name of files) {
      const source = fs.readFileSync(path.join(root, name), 'utf8')
      expect(source).not.toMatch(/Tauri BLE manager/)
      expect(source).not.toMatch(/Electron BLE manager/)
    }
  })

  test('duplicate remote stream handles are protocol.violation not TypeError', async () => {
    const ipc = await createManager()
    ipc.registerStream('stream-1', isRecord)
    try {
      ipc.registerStream('stream-1', isRecord)
      throw new Error('expected duplicate stream handle to fail')
    } catch (error) {
      expect(error).toMatchObject({
        normalized: { code: 'protocol.violation', domain: 'ipc', operation: 'ipc-manager.stream-handle' }
      })
      expect(error).not.toBeInstanceOf(TypeError)
    }
    await ipc.destroy()
  })

  test('malformed route deadlines are protocol.malformed not TypeError', async () => {
    const ipc = await createManager()
    await expect(ipc.route('adapter.state', Object.freeze({ deadline: Number.NaN }))).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', domain: 'ipc', operation: 'ipc-manager.deadline' }
    })
    await expect(ipc.route('adapter.state', Object.freeze({ deadline: Number.NaN }))).rejects.not.toBeInstanceOf(
      TypeError
    )
    await ipc.destroy()
  })
})
