'use strict'

const { BUILT_IN_FEATURE_IDS } = require('../src/backend-contract/capabilities')
const { createIpcBootstrapRequest } = require('../src/ipc/protocol')

class FakeChannel {
  constructor() {
    this.onmessage = null
  }
}

function negotiated(axis) {
  const selected = { axis, value: axis === 'ipc-protocol' ? 2 : 1 }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function capabilityDescriptor(id, scenario, state = 'limited') {
  const limitation = {
    code: state === 'limited' ? 'deterministic-only' : 'not-implemented',
    explanation:
      state === 'limited'
        ? 'The fixture exposes deterministic host evidence only.'
        : 'The fixture does not implement this capability.',
    affectedGuarantee: state === 'limited' ? 'Physical-radio qualification is not claimed.' : 'support'
  }
  const schemaRange = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  return {
    id,
    state,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native',
    tck: { suiteId: 'capability.catalog-v2', requiredScenarioIds: [scenario], contractRange: schemaRange },
    evidence: {
      receiptId: `fixture-${id}`,
      evidenceLevel: state === 'limited' ? 'deterministic' : 'blocked',
      implementationVersion: 'fixture-v2',
      sourceDigest: `fixture-${id}`,
      scenarioIds: [scenario],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }
}

function capabilitySnapshot(backendGeneration) {
  const entries = [
    ['discovery:continuous-scan', 'scan.owner-join-authority-and-signature'],
    ['connection:direct', 'connection.lease-joins-borrowing-transfer-and-revocation'],
    ['connection:rssi', 'connection.rssi-and-att-mtu-capability-contract'],
    ['gatt:descriptors', 'gatt.descriptor-discovery-read-write'],
    ['gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation']
  ]
  const metadata = new Map(entries)
  return {
    schemaVersion: 2,
    backendGeneration,
    descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => {
      const scenario = metadata.get(id)
      return capabilityDescriptor(
        id,
        scenario ?? 'capability.truth-limits-evidence-and-binding',
        scenario === undefined ? 'unsupported' : 'limited'
      )
    })
  }
}

function rustUnsampledAdapterState(backendGeneration, heard) {
  return {
    availability: 'unknown',
    authorization: 'unknown',
    power: 'unknown',
    heard,
    backendGeneration,
    updatedAt: 1_700_000_000_000,
    safeReason:
      'This snapshot carries attachment identity only; availability, power, and the heard peer count are not sampled here, so route adapter.state for a live reading.'
  }
}

function rustBootstrapResponse(adapterState) {
  const backendGeneration = 'tauri-backend-generation-1'
  const attachmentId = 'tauri-attachment-1'
  return {
    kind: 'bootstrap',
    bootstrap: {
      attachmentId,
      attachment: {
        attachmentId,
        backendInstanceId: 'tauri-btleplug-1',
        backendGeneration,
        adapter: {
          adapterId: 'adapter',
          displayName: 'Bluetooth',
          adapterGeneration: 'tauri-adapter-generation-1',
          limitations: [
            'This host binds one adapter for the lifetime of the attachment; the adapter is selected when the attachment is created and other adapters are not reachable through it.',
            'This host does not observe adapter-state changes; every adapter.state response is a fresh sample and no adapter-state event is emitted.'
          ],
          state: adapterState ?? rustUnsampledAdapterState(backendGeneration, null)
        }
      },
      versions: {
        backendContract: negotiated('backend-contract'),
        capabilitySchema: negotiated('capability-schema'),
        eventSchema: negotiated('event-schema'),
        traceFormat: negotiated('trace-format'),
        ipcProtocol: negotiated('ipc-protocol')
      },
      capabilities: capabilitySnapshot(backendGeneration),
      discovery: { kind: 'continuous-scan' },
      renderer: {
        clientId: 'com.trackourhealth.tauri-hearts:main',
        windowScope: 'main',
        sessionScope: 'tauri-lease-generation-1'
      },
      rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' }
    }
  }
}

async function invokeBootstrap(response) {
  const invoke = jest.fn(async () => response)
  const { TauriBleIpcTransport } = require('../src/tauri/transport')
  const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })
  return transport.invoke(createIpcBootstrapRequest())
}

describe('Tauri Rust bootstrap adapter-state contract', () => {
  test('decodes the unsampled Rust bootstrap adapter snapshot including heard: null', async () => {
    const response = await invokeBootstrap(rustBootstrapResponse())
    expect(response.kind).toBe('bootstrap')
    expect(response.bootstrap.attachment.adapter.state.heard).toBeNull()
  })

  test('decodes a live adapter.state snapshot including integer heard', async () => {
    const backendGeneration = 'tauri-backend-generation-1'
    const liveState = {
      availability: 'available',
      authorization: 'granted',
      power: 'on',
      heard: 3,
      backendGeneration,
      updatedAt: 1_700_000_000_001,
      safeReason: null
    }
    const response = await invokeBootstrap(rustBootstrapResponse(liveState))
    expect(response.kind).toBe('bootstrap')
    expect(response.bootstrap.attachment.adapter.state.heard).toBe(3)
  })

  test('rejects adapter.state that omits heard', async () => {
    const response = rustBootstrapResponse()
    const { heard, ...rest } = response.bootstrap.attachment.adapter.state
    expect(heard).toBeNull()
    response.bootstrap.attachment.adapter.state = rest
    await expect(invokeBootstrap(response)).rejects.toMatchObject({
      normalized: {
        code: 'protocol.malformed',
        operation: expect.stringMatching(/tauri\.transport\.response/)
      }
    })
  })

  test('names extra adapter.state keys when decode fails', async () => {
    const response = rustBootstrapResponse()
    response.bootstrap.attachment.adapter.state.unexpected = true
    await expect(invokeBootstrap(response)).rejects.toMatchObject({
      normalized: {
        code: 'protocol.malformed',
        operation: expect.stringMatching(/extra=unexpected/)
      }
    })
  })

  test('decodes a live adapter.state route payload that includes heard', async () => {
    const invoke = jest.fn(async () => ({
      kind: 'route',
      payload: {
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: 3,
          backendGeneration: 'tauri-backend-generation-1',
          updatedAt: 1_700_000_000_001,
          safeReason: null
        }
      }
    }))
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })
    const response = await transport.invoke({
      kind: 'route',
      envelope: {
        command: 'adapter.state',
        rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' }
      }
    })
    expect(response.kind).toBe('route')
    expect(response.payload.state.heard).toBe(3)
  })
})
