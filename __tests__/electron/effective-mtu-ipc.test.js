// __tests__/electron/effective-mtu-ipc.test.js
//
// W7: the Electron main router answers `connection.effective-mtu` like the
// Tauri route, so the renderer `controls.effectiveMtu()` observes a measured
// ATT MTU instead of `argument.invalid`.

const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const { BackendContractError, contractError } = require('../../src/backend-contract/errors')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../../src/ipc/protocol')
const { monotonicTimestamp, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { BUILT_IN_FEATURE_CATALOG } = require('../../src/backend-contract/capabilities')

function negotiated(axis) {
  const selected = version(axis, axis === 'ipc-protocol' ? 4 : 1)
  const range = versionRange(selected, selected)
  return { axis, selected, localRange: range, remoteRange: range }
}

function attachment() {
  const backendGeneration = opaqueId('electron-mtu-generation', 'backend-generation', 'electron')
  return {
    attachmentId: opaqueId('electron-mtu-attachment', 'attachment', 'electron'),
    backendInstanceId: opaqueId('electron-mtu-backend', 'backend-instance', 'electron'),
    backendGeneration,
    adapter: {
      adapterId: opaqueId('electron-mtu-adapter', 'adapter', 'electron'),
      displayName: null,
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration,
        updatedAt: monotonicTimestamp(1),
        safeReason: null
      },
      adapterGeneration: opaqueId('electron-mtu-adapter-generation', 'adapter-generation', 'electron'),
      limitations: []
    }
  }
}

function versions() {
  return {
    backendContract: negotiated('backend-contract'),
    capabilitySchema: negotiated('capability-schema'),
    eventSchema: negotiated('event-schema'),
    traceFormat: negotiated('trace-format')
  }
}

function capabilityDescriptors() {
  const schema = versionRange(version('capability-schema', 1), version('capability-schema', 1))
  const limitation = {
    code: 'not-implemented',
    explanation: 'mtu fixture capability is not implemented',
    affectedGuarantee: 'support'
  }
  const limited = new Set(['connection:direct', 'connection:rssi', 'connection:effective-mtu'])
  return BUILT_IN_FEATURE_CATALOG.map(entry => ({
    id: entry.id,
    state: limited.has(entry.id) ? 'limited' : 'unsupported',
    selectedSchemaRange: schema,
    implementationOrigin: 'backend-native',
    tck: {
      suiteId: entry.requiredTckSuiteId,
      requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
      contractRange: schema
    },
    evidence: {
      receiptId: `electron-mtu-test-${entry.id}`,
      evidenceLevel: limited.has(entry.id) ? 'deterministic' : 'blocked',
      implementationVersion: 'test',
      sourceDigest: `electron-mtu-test-${entry.id}`,
      scenarioIds: ['capability.truth-limits-evidence-and-binding'],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }))
}

function createSender(client, windowScope, sessionScope) {
  const mainFrame = Object.freeze({ processId: 10, routingId: 20 })
  return {
    mainFrame,
    sent: [],
    trusted: {
      authenticatedClientId: opaqueId(client, 'client', `electron:${client}`),
      authenticatedWindowScope: windowScope,
      authenticatedSessionScope: sessionScope
    },
    isDestroyed: () => false,
    once: () => undefined,
    on: () => undefined,
    removeListener: () => undefined,
    send(channel, event) {
      this.sent.push({ channel, event })
    }
  }
}

function createMainFixture(managerOverrides = {}) {
  const currentAttachment = attachment()
  const manager = {
    attachedBackend: { attachment: { attachment: currentAttachment } },
    identity: { versions: versions() },
    capabilities: () => capabilityDescriptors(),
    planScan: jest.fn(),
    onAttachmentAdvanced: () => () => undefined,
    destroy: jest.fn(async () => ({ state: 'released', failures: [] })),
    ...managerOverrides
  }
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 4096,
    maximumOutstandingOperations: 2,
    maximumRetainedBytes: 8192,
    publish: async () => 'terminalized'
  })
  const port = {
    handler: null,
    rawHandler: null,
    handle(channel, handler) {
      expect(channel).toBe('unified-ble-manager:v2')
      this.rawHandler = handler
      this.handler = (event, request) =>
        handler(
          {
            ...event,
            frameId: event.frameId ?? event.sender.mainFrame.routingId,
            processId: event.processId ?? event.sender.mainFrame.processId
          },
          request
        )
    },
    removeHandler: jest.fn()
  }
  const authenticate = jest.fn(event => event.sender.trusted)
  const binding = new ElectronMainBleBinding({ router, port, authenticate })
  binding.install()
  return { authenticate, binding, currentAttachment, manager, port, router, versions: manager.identity.versions }
}

function routeEnvelope(current, bootstrapValue, ordinal, command, payload) {
  return {
    kind: 'route',
    envelope: {
      versions: { ...current.versions, ipcProtocol: negotiated('ipc-protocol') },
      attachment: current.currentAttachment,
      attachmentId: current.currentAttachment.attachmentId,
      renderer: bootstrapValue.renderer,
      rendererLease: bootstrapValue.rendererLease,
      correlation: opaqueId(`mtu-operation-${ordinal}`, 'ipc-operation', `electron:mtu-${ordinal}`),
      dispatchEpoch: opaqueId(`mtu-dispatch-${ordinal}`, 'ipc-dispatch-epoch', `electron:mtu-${ordinal}`),
      command,
      payload,
      binaryPayload: null
    }
  }
}

async function bootstrap(current, sender) {
  const response = await current.port.handler({ sender }, { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
  if (response.kind === 'failure') throw new BackendContractError(response.error)
  expect(response.kind).toBe('bootstrap')
  return response.bootstrap
}

test('renderer observes the main-side effective MTU like the Tauri route', async () => {
  const effectiveMtu = jest.fn(async () => ({ attMtu: 185, payloadBytes: 182, platformPduBytes: null }))
  const connection = {
    peerId: 'peer-mtu',
    connectionId: 'connection-peer-mtu',
    connectionGeneration: 'connection-generation-peer-mtu',
    ownerLeaseId: 'owner-lease-peer-mtu',
    discover: jest.fn(),
    disconnect: jest.fn(async () => ({ state: 'released', failures: [] })),
    // The lifecycle stream stays open for the test: the pump forwards only
    // after real lifecycle items, and an ended stream would log
    // "ended without a terminal item".
    events: {
      [Symbol.asyncIterator]: () => ({
        next: () => new Promise(() => {}),
        return: async () => ({ done: true, value: undefined })
      })
    },
    readRssi: jest.fn(async () => ({ rssi: -42 })),
    effectiveMtu
  }
  const current = createMainFixture({ connect: jest.fn(async () => connection) })
  const sender = createSender('client-mtu', 'window-mtu', 'session-mtu')
  const rendererTransport = {
    invoke: request => current.port.handler({ sender }, request),
    subscribe: () => () => undefined,
    acknowledge: () => current.port.handler({ sender }, { kind: 'event.ack', rendererLease: 'x', eventId: 'y' })
  }
  const manager = await createElectronRendererBleManager({ transport: rendererTransport })
  const publicConnection = await manager.connect('peer-mtu')
  await expect(publicConnection.controls.effectiveMtu()).resolves.toMatchObject({
    state: 'measured',
    attMtu: 185,
    payloadBytes: 182
  })
  expect(effectiveMtu).toHaveBeenCalledTimes(1)
})

function mtuConnection(peer, effectiveMtu) {
  return {
    peerId: peer,
    connectionId: `connection-${peer}`,
    connectionGeneration: `connection-generation-${peer}`,
    ownerLeaseId: `owner-lease-${peer}`,
    discover: jest.fn(),
    disconnect: jest.fn(async () => ({ state: 'released', failures: [] })),
    events: {
      [Symbol.asyncIterator]: () => ({
        next: () => new Promise(() => {}),
        return: async () => ({ done: true, value: undefined })
      })
    },
    readRssi: jest.fn(async () => ({ rssi: -42 })),
    effectiveMtu
  }
}

async function connectPeer(current, sender, bootstrapValue, ordinal, peer) {
  const connected = await current.port.handler(
    { sender },
    routeEnvelope(current, bootstrapValue, ordinal, 'connection.connect', { peerId: peer, deadline: null })
  )
  expect(connected.kind).toBe('route')
  return connected.payload.handle
}

test('effective-mtu forwards the renderer deadline and abort signal', async () => {
  const seen = []
  const effectiveMtu = jest.fn(async options => {
    seen.push(options)
    return { attMtu: 185, payloadBytes: 182, platformPduBytes: null }
  })
  const current = createMainFixture({
    connect: jest.fn(async () => mtuConnection('peer-mtu-deadline', effectiveMtu)),
    monotonicNow: () => 1000
  })
  const sender = createSender('client-mtu-deadline', 'window-mtu-deadline', 'session-mtu-deadline')
  const bootstrapValue = await bootstrap(current, sender)
  const connectionHandle = await connectPeer(current, sender, bootstrapValue, 1, 'peer-mtu-deadline')
  // The renderer speaks a relative budget on its own clock; main admits it
  // against the main clock and forwards the resulting absolute deadline.
  const response = await current.port.handler(
    { sender },
    routeEnvelope(current, bootstrapValue, 2, 'connection.effective-mtu', {
      connectionHandle,
      budgetMs: 500
    })
  )
  expect(response.kind).toBe('route')
  expect(effectiveMtu).toHaveBeenCalledTimes(1)
  expect(seen[0].deadline).toBe(1500)
  expect(seen[0].signal).toBeInstanceOf(AbortSignal)
})

test('effective-mtu propagates renderer abort to the connection', async () => {
  let captured
  const effectiveMtu = jest.fn(
    options =>
      new Promise((resolve, reject) => {
        captured = options
        // What a real connection reports when its operation signal fires.
        options.signal.addEventListener('abort', () =>
          reject(
            new BackendContractError(
              contractError('operation.aborted', 'connection', 'test-effective-mtu-abort').normalized
            )
          )
        )
      })
  )
  const current = createMainFixture({
    connect: jest.fn(async () => mtuConnection('peer-mtu-abort', effectiveMtu))
  })
  const sender = createSender('client-mtu-abort', 'window-mtu-abort', 'session-mtu-abort')
  const bootstrapValue = await bootstrap(current, sender)
  const connectionHandle = await connectPeer(current, sender, bootstrapValue, 1, 'peer-mtu-abort')
  const mtuRequest = routeEnvelope(current, bootstrapValue, 2, 'connection.effective-mtu', {
    connectionHandle,
    deadline: null
  })
  const pending = current.port.handler({ sender }, mtuRequest)
  for (let flush = 0; flush < 5; flush += 1) {
    // eslint-disable-next-line no-await-in-loop
    await new Promise(resolve => setImmediate(resolve))
  }
  const cancelled = await current.port.handler(
    { sender },
    routeEnvelope(current, bootstrapValue, 3, 'operation.cancel', {
      targetCorrelation: String(mtuRequest.envelope.correlation)
    })
  )
  expect(cancelled).toMatchObject({ kind: 'route', payload: { state: 'cancellation-requested' } })
  await expect(pending).resolves.toMatchObject({ kind: 'failure', error: { code: 'operation.aborted' } })
  expect(captured.signal.aborted).toBe(true)
})

test('effective-mtu on an unknown handle fails closed instead of answering', async () => {
  const current = createMainFixture({ connect: jest.fn() })
  const sender = createSender('client-mtu-stale', 'window-mtu-stale', 'session-mtu-stale')
  const bootstrapValue = await bootstrap(current, sender)
  const response = await current.port.handler(
    { sender },
    routeEnvelope(current, bootstrapValue, 1, 'connection.effective-mtu', {
      connectionHandle: 'connection-no-such-handle',
      deadline: null
    })
  )
  expect(response.kind).toBe('failure')
})
