const { createPublicBleManager } = require('../src/public/ble-manager')

const counters = {
  activeScanControllers: 0,
  scanConsumers: 0,
  chooserSessions: 1,
  connectionLeases: 0,
  physicalLinks: 0,
  databaseSnapshots: 0,
  physicalCccdEnablements: 0,
  subscriptionConsumers: 0,
  queuedOperations: 0,
  dispatchedOperations: 0,
  retainedByteBuffers: 0,
  restorationRecords: 0,
  orphanedIpcOwners: 0
}

describe('public diagnostics backend authority', () => {
  test('uses instantiated backend counters and trace when backend owns the resources', async () => {
    const backendTrace = {
      format: 'unified-ble-trace-v1',
      truncated: false,
      records: [
        {
          ordinal: 1,
          time: 10,
          kind: 'resource',
          event: 'chooser-active',
          cause: null,
          correlation: null,
          redactedClient: true,
          redactedPeer: true,
          redactedPath: true,
          redactedPayload: true
        }
      ]
    }
    const internal = {
      supports: () => false,
      capability: () => null,
      capabilities: () => [],
      localResourceCounters: () => ({ ...counters, chooserSessions: 0 }),
      traceDocument: () => ({ ...backendTrace, records: [] }),
      attachedBackend: {
        backend: {
          security: undefined,
          resourceCounters: () => counters,
          traceDocument: () => backendTrace
        }
      },
      destroy: jest.fn(),
      scan: jest.fn(),
      connect: jest.fn()
    }

    const manager = await createPublicBleManager(internal, () => 10)
    const snapshot = manager.diagnostics.snapshot()

    expect(snapshot.resourceCounters.chooserSessions).toBe(1)
    expect(snapshot.trace.records).toEqual(backendTrace.records)
  })
})
