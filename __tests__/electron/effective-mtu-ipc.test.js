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
  const selected = version(axis, axis === 'ipc-protocol' ? 5 : 1)
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
  const limited = new Set([
    'connection:direct',
    'connection:when-available',
    'connection:rssi',
    'connection:effective-mtu',
    'security:state',
    'security:pair',
    'security:cancel-pairing',
    'security:unpair',
    'security:custom-ceremony',
    'peer:address-targeting',
    'scan:platform-options'
  ])
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

test('peer directories cross authenticated Electron IPC without acquiring a connection lease', async () => {
  const reference = { version: 1, backendId: 'corebluetooth', scope: 'system', opaqueId: 'os-connected' }
  const peer = {
    peerId: 'os-connected',
    reference,
    name: 'OS connected',
    rssi: null,
    source: 'system-connected',
    state: { reachability: 'reachable', connection: 'connected', bond: 'unknown', lastSeenAtMonotonicMs: null }
  }
  const connected = jest.fn(async () => [peer])
  const resolve = jest.fn(async () => peer)
  const connect = jest.fn()
  const current = createMainFixture({ connect, monotonicNow: () => performance.now() })
  current.manager.attachedBackend.backend = { peers: { connected, resolve } }
  const sender = createSender('directory', 'directory-window', 'directory-session')
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(manager.peers.connected({ services: ['180d'], timeoutMs: 1000 })).resolves.toMatchObject([
    { id: 'os-connected', reference, state: { connection: 'connected', lastSeenAtMonotonicMs: null } }
  ])
  expect(connected.mock.calls[0][0]).toMatchObject({
    services: ['0000180d-0000-1000-8000-00805f9b34fb'],
    signal: expect.any(AbortSignal),
    deadline: expect.any(Number)
  })
  await expect(manager.peers.resolve(reference)).resolves.toMatchObject({ id: 'os-connected' })
  expect(resolve.mock.calls[0][0]).toEqual(reference)
  expect(connect).not.toHaveBeenCalled()
  await manager.destroy()
})

test('ordinary renderer security routes reach the scoped native authority', async () => {
  const state = {
    bond: 'bonded',
    encryption: 'unknown',
    authentication: 'unknown',
    secureConnections: 'unknown',
    pairingPossible: true,
    measuredAtMonotonicMs: 1,
    limitations: []
  }
  const security = {
    state: jest.fn(async () => state),
    pair: jest.fn(async () => ({ outcome: 'paired', state })),
    cancelPairing: jest.fn(async () => ({ outcome: 'paired' })),
    unpair: jest.fn(async () => ({ outcome: 'unpaired' }))
  }
  const current = createMainFixture({ monotonicNow: () => performance.now(), securityBackend: () => security })
  const sender = createSender('security', 'security-window', 'security-session')
  sender.trusted.securityPermissions = ['security:state', 'security:pair', 'security:cancel-pairing', 'security:unpair']
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  const peer = { id: 'security-peer', name: null, rssi: null }
  await expect(manager.security.state(peer)).resolves.toEqual(state)
  await expect(manager.security.pair(peer, { transport: 'le', secureConnections: 'require' })).resolves.toEqual({
    outcome: 'paired',
    state
  })
  expect(security.pair.mock.calls[0]).toEqual([
    'security-peer',
    expect.objectContaining({
      transport: 'le',
      secureConnections: 'require',
      ceremony: 'system',
      signal: expect.any(AbortSignal)
    })
  ])
  await expect(manager.security.cancelPairing(peer)).resolves.toEqual({ outcome: 'paired' })
  await expect(manager.security.unpair(peer)).resolves.toEqual({ outcome: 'unpaired' })
  await manager.destroy()
})

test('renderer security is denied without each explicit trusted permission', async () => {
  const security = { pair: jest.fn(), state: jest.fn() }
  const current = createMainFixture({ monotonicNow: () => performance.now(), securityBackend: () => security })
  const sender = createSender('denied-security', 'denied-window', 'denied-session')
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(manager.security.pair({ id: 'peer', name: null, rssi: null })).rejects.toMatchObject({
    code: 'permission.denied'
  })
  await expect(manager.security.state({ id: 'peer', name: null, rssi: null })).rejects.toMatchObject({
    code: 'permission.denied'
  })
  expect(security.pair).not.toHaveBeenCalled()
  expect(security.state).not.toHaveBeenCalled()
  await manager.destroy()
})

test('custom ceremony requires its own permission in addition to pairing', async () => {
  const pair = jest.fn()
  const current = createMainFixture({ monotonicNow: () => performance.now(), securityBackend: () => ({ pair }) })
  const sender = createSender('pair-only', 'pair-only-window', 'pair-only-session')
  sender.trusted.securityPermissions = ['security:pair']
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(
    manager.security.pair({ id: 'peer', name: null, rssi: null }, { ceremony: { onChallenge: jest.fn() } })
  ).rejects.toMatchObject({ code: 'permission.denied' })
  expect(pair).not.toHaveBeenCalled()
  await manager.destroy()
})

test('watch permission is denied before native registration', async () => {
  const watch = jest.fn()
  const current = createMainFixture({ monotonicNow: () => performance.now(), securityBackend: () => ({ watch }) })
  const sender = createSender('watch-denied', 'watch-denied-window', 'watch-denied-session')
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(
    manager.security.watch({ id: 'peer', name: null, rssi: null })[Symbol.asyncIterator]().next()
  ).rejects.toMatchObject({ code: 'permission.denied' })
  expect(watch).not.toHaveBeenCalled()
  await manager.destroy()
})

test('public renderer transports out-of-band address and connection policy unchanged', async () => {
  const current = createMainFixture({ monotonicNow: () => performance.now(), supports: () => true })
  const peerFromAddress = jest.fn(() => 'native-address-peer')
  current.manager.attachedBackend.backend = { connections: { peerFromAddress } }
  current.manager.connect = jest.fn(async () => ({
    peerId: 'native-address-peer',
    connectionId: 'address-connection',
    connectionGeneration: 'address-generation',
    disconnect: async () => ({ state: 'released', failures: [] })
  }))
  const sender = createSender('address', 'address-window', 'address-session')
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  const connection = await manager.connect(
    { address: 'DC:56:7B:D9:E8:A4', addressType: 'public' },
    { intent: 'when-available', transport: 'le', preferredPhy: ['le-1m'] }
  )
  expect(peerFromAddress).toHaveBeenCalledWith({ address: 'DC:56:7B:D9:E8:A4', addressType: 'public' })
  expect(current.manager.connect.mock.calls[0]).toEqual([
    'native-address-peer',
    expect.objectContaining({ intent: 'when-available', transport: 'le', preferredPhy: ['le-1m'] })
  ])
  await connection.disconnect()
  await manager.destroy()
})

test('custom security ceremony crosses the ordinary renderer factory and keeps clocks relative', async () => {
  const state = {
    bond: 'bonded',
    encryption: 'unknown',
    authentication: 'unknown',
    secureConnections: 'unknown',
    pairingPossible: true,
    measuredAtMonotonicMs: 1,
    limitations: []
  }
  const pair = jest.fn(async (peerId, options) => {
    const response = await options.ceremony.agent.onChallenge({
      kind: 'confirm-passkey',
      peerId,
      challengeId: 'challenge-1',
      passkey: 123456,
      deadlineMonotonicMs: 501000
    })
    expect(response).toEqual({ kind: 'confirm-passkey', confirmed: true })
    return { outcome: 'paired', state }
  })
  const current = createMainFixture({ monotonicNow: () => 500000, securityBackend: () => ({ pair }) })
  const sender = createSender('agent-security', 'agent-window', 'agent-session')
  sender.trusted.securityPermissions = ['security:pair', 'security:custom-ceremony']
  let listener
  current.router.setEventPublisher(async (_, event) => {
    listener(event)
    return 'delivered'
  })
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: callback => {
        listener = callback
        return () => {}
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  const onChallenge = jest.fn(async challenge => {
    expect(challenge.peer.id).toBe('peer')
    expect(challenge.deadlineMonotonicMs).toBeLessThan(performance.now() + 1100)
    expect(challenge.deadlineMonotonicMs).toBeGreaterThan(performance.now())
    return { kind: challenge.kind, confirmed: true }
  })
  await expect(
    manager.security.pair({ id: 'peer', name: null, rssi: null }, { ceremony: { onChallenge } })
  ).resolves.toEqual({ outcome: 'paired', state })
  expect(onChallenge).toHaveBeenCalledTimes(1)
  await manager.destroy()
})

test('delivered custom challenge response remains confirmed when its route deadline expires afterward', async () => {
  const state = {
    bond: 'bonded',
    encryption: 'unknown',
    authentication: 'unknown',
    secureConnections: 'unknown',
    pairingPossible: true,
    measuredAtMonotonicMs: 1,
    limitations: []
  }
  let customDispatch = false
  let customClockReads = 0
  const nativeResponses = []
  const customReceipts = []
  const current = createMainFixture({
    monotonicNow: () => (customDispatch ? (++customClockReads === 1 ? 100 : 102) : performance.now()),
    securityBackend: () => ({
      pair: async (peerId, options) => {
        const response = await options.ceremony.agent.onChallenge({
          kind: 'confirm',
          peerId,
          challengeId: 'late-deadline',
          deadlineMonotonicMs: performance.now() + 1000
        })
        nativeResponses.push(response)
        return { outcome: 'paired', state }
      }
    })
  })
  const sender = createSender('late-deadline', 'late-deadline-window', 'late-deadline-session')
  sender.trusted.securityPermissions = ['security:pair', 'security:custom-ceremony']
  let listener
  current.router.setEventPublisher(async (_, event) => {
    listener(event)
    return 'delivered'
  })
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => {
        if (request.envelope?.command === 'security.custom-ceremony') {
          customDispatch = true
          return current.port
            .handler(
              { sender },
              { ...request, envelope: { ...request.envelope, payload: { ...request.envelope.payload, budgetMs: 1 } } }
            )
            .then(receipt => {
              customReceipts.push(receipt)
              return receipt
            })
        }
        return current.port.handler({ sender }, request)
      },
      subscribe: callback => {
        listener = callback
        return () => {}
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(
    manager.security.pair(
      { id: 'peer', name: null, rssi: null },
      { ceremony: { onChallenge: async () => ({ kind: 'confirm', confirmed: true }) } }
    )
  ).resolves.toEqual({ outcome: 'paired', state })
  expect(nativeResponses).toEqual([{ kind: 'confirm', confirmed: true }])
  expect(customReceipts).toEqual([expect.objectContaining({ kind: 'route', payload: { accepted: true } })])
  await manager.destroy()
})

test('held application challenge does not hold a confirmed native pairing result', async () => {
  const state = {
    bond: 'bonded',
    encryption: 'unknown',
    authentication: 'unknown',
    secureConnections: 'unknown',
    pairingPossible: true,
    measuredAtMonotonicMs: 1,
    limitations: []
  }
  const current = createMainFixture({
    monotonicNow: () => performance.now(),
    securityBackend: () => ({
      pair: async (peerId, options) => {
        const challenge = options.ceremony.agent.onChallenge({
          kind: 'confirm',
          peerId,
          challengeId: 'held',
          deadlineMonotonicMs: performance.now() + 1000
        })
        challenge.catch(() => undefined)
        return { outcome: 'paired', state }
      }
    })
  })
  const sender = createSender('held', 'held-window', 'held-session')
  sender.trusted.securityPermissions = ['security:pair', 'security:custom-ceremony']
  let listener
  current.router.setEventPublisher(async (_, event) => {
    listener(event)
    return 'delivered'
  })
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: callback => {
        listener = callback
        return () => {}
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(
    manager.security.pair(
      { id: 'peer', name: null, rssi: null },
      { ceremony: { onChallenge: () => new Promise(() => {}) } }
    )
  ).resolves.toEqual({ outcome: 'paired', state })
  await manager.destroy()
})

test('throwing application agent cancels native challenge and reports its failure', async () => {
  const nativeSettled = jest.fn()
  const current = createMainFixture({
    monotonicNow: () => performance.now(),
    securityBackend: () => ({
      pair: async (peerId, options) => {
        try {
          await options.ceremony.agent.onChallenge({
            kind: 'confirm',
            peerId,
            challengeId: 'throws',
            deadlineMonotonicMs: performance.now() + 1000
          })
        } finally {
          nativeSettled()
        }
        return { outcome: 'cancelled' }
      }
    })
  })
  const sender = createSender('throws', 'throws-window', 'throws-session')
  sender.trusted.securityPermissions = ['security:pair', 'security:custom-ceremony']
  let listener
  current.router.setEventPublisher(async (_, event) => {
    listener(event)
    return 'delivered'
  })
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: callback => {
        listener = callback
        return () => {}
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  await expect(
    manager.security.pair(
      { id: 'peer', name: null, rssi: null },
      {
        ceremony: {
          onChallenge: async () => {
            throw new Error('application agent failure')
          }
        }
      }
    )
  ).rejects.toThrow('application agent failure')
  await new Promise(resolve => setImmediate(resolve))
  expect(nativeSettled).toHaveBeenCalledTimes(1)
  await manager.destroy()
})

test.each(['refused', 'rejected', 'held'])(
  'watch native close %s retains retry ownership without blocking sibling release',
  async kind => {
    let settleClose
    const iterator = {
      next: () => new Promise(() => {}),
      [Symbol.asyncIterator]() {
        return this
      },
      return: jest.fn(async () => ({ done: true, value: undefined }))
    }
    const failure = {
      resourceKind: 'native-security-watch',
      error: contractError('platform.transport', 'cleanup', 'native.watch.close').normalized
    }
    const close = jest
      .fn()
      .mockImplementationOnce(() =>
        kind === 'held'
          ? new Promise(resolve => {
              settleClose = resolve
            })
          : kind === 'rejected'
            ? Promise.reject(new Error('native close refused'))
            : Promise.resolve({ state: 'release-failed', failures: [failure] })
      )
      .mockResolvedValue({ state: 'released', failures: [] })
    const current = createMainFixture({
      monotonicNow: () => performance.now(),
      securityBackend: () => ({ watch: () => ({ [Symbol.asyncIterator]: () => iterator, close }) })
    })
    const sender = createSender(`watch-native-${kind}`, `watch-native-${kind}-window`, `watch-native-${kind}-session`)
    sender.trusted.securityPermissions = ['security:state']
    const admitted = await bootstrap(current, sender)
    const watch = await current.port.handler(
      { sender },
      routeEnvelope(current, admitted, 1, 'security.watch.subscribe', { peerId: 'peer' })
    )
    jest.useFakeTimers()
    try {
      const release = current.port.handler(
        { sender },
        routeEnvelope(current, admitted, 2, 'security.watch.unsubscribe', { handle: watch.payload.handle })
      )
      // Advance the actual bounded drain; a loaded runner cannot change which
      // cleanup outcome wins by running a competing real-time test timer.
      await jest.advanceTimersByTimeAsync(50)
      const first = await release
      if (kind === 'held') {
        settleClose({ state: 'released', failures: [] })
        await jest.advanceTimersByTimeAsync(0)
      }
      expect(first).toMatchObject({ kind: 'route', payload: { state: 'release-failed' } })
      await expect(
        current.port.handler(
          { sender },
          routeEnvelope(current, admitted, 3, 'security.watch.unsubscribe', { handle: watch.payload.handle })
        )
      ).resolves.toMatchObject({ kind: 'route', payload: { state: 'released' } })
      expect(iterator.return).toHaveBeenCalledTimes(1)
      expect(close).toHaveBeenCalledTimes(kind === 'held' ? 1 : 2)
      await current.binding.destroy()
    } finally {
      jest.useRealTimers()
    }
  }
)

test('renderer native security watch has scoped events and single-flight removal', async () => {
  const { CoreBoundedStream } = require('../../src/core/bounded-stream')
  const { capacity } = require('../../src/backend-contract/primitives')
  const stream = new CoreBoundedStream(
    { itemCapacity: capacity(8), byteCapacity: capacity(8192), reservedControlCapacity: capacity(1) },
    'error'
  )
  const close = jest.spyOn(stream, 'close')
  const current = createMainFixture({
    monotonicNow: () => performance.now(),
    securityBackend: () => ({ watch: () => stream })
  })
  const sender = createSender('watch', 'watch-window', 'watch-session')
  sender.trusted.securityPermissions = ['security:state']
  let listener
  current.router.setEventPublisher(async (_, event) => {
    listener(event)
    return 'delivered'
  })
  const manager = await createElectronRendererBleManager({
    transport: {
      invoke: request => current.port.handler({ sender }, request),
      subscribe: callback => {
        listener = callback
        return () => {}
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
  })
  const watch = manager.security.watch({ id: 'peer', name: null, rssi: null })
  const iterator = watch[Symbol.asyncIterator]()
  const pending = iterator.next()
  await new Promise(resolve => setImmediate(resolve))
  const state = {
    bond: 'bonding',
    encryption: 'unknown',
    authentication: 'unknown',
    secureConnections: 'unknown',
    pairingPossible: true,
    measuredAtMonotonicMs: 1,
    limitations: []
  }
  stream.emit({ kind: 'state', peerId: 'peer', sequence: 1, state }, 1)
  await expect(pending).resolves.toMatchObject({ done: false, value: { peerId: 'peer', state } })
  await Promise.all([iterator.return(), iterator.return()])
  expect(close).toHaveBeenCalledTimes(1)
  await manager.destroy()
})

test.each(['rejected', 'held'])(
  'watch %s local return cannot prevent authoritative close; late cleanup remains owned',
  async kind => {
    let settleReturn
    const iterator = {
      next: () => new Promise(() => {}),
      [Symbol.asyncIterator]() {
        return this
      },
      return: jest
        .fn()
        .mockImplementationOnce(() =>
          kind === 'held'
            ? new Promise(resolve => {
                settleReturn = resolve
              })
            : Promise.reject(new Error('local return refused'))
        )
        .mockResolvedValue({ done: true, value: undefined })
    }
    const close = jest.fn(async () => ({ state: 'released', failures: [] }))
    const current = createMainFixture({
      monotonicNow: () => performance.now(),
      securityBackend: () => ({ watch: () => ({ [Symbol.asyncIterator]: () => iterator, close }) })
    })
    const sender = createSender(`watch-${kind}`, `watch-${kind}-window`, `watch-${kind}-session`)
    sender.trusted.securityPermissions = ['security:state']
    const admitted = await bootstrap(current, sender)
    const watch = await current.port.handler(
      { sender },
      routeEnvelope(current, admitted, 1, 'security.watch.subscribe', { peerId: 'peer' })
    )
    const release = current.port.handler(
      { sender },
      routeEnvelope(current, admitted, 2, 'security.watch.unsubscribe', { handle: watch.payload.handle })
    )
    await new Promise(resolve => setImmediate(resolve))
    expect(close).toHaveBeenCalledTimes(1)
    await expect(release).resolves.toMatchObject({
      kind: 'route',
      payload: {
        state: 'release-failed',
        failures: [expect.objectContaining({ resourceKind: 'security-watch-iterator' })]
      }
    })
    if (kind === 'held') {
      settleReturn({ done: true, value: undefined })
      await new Promise(resolve => setImmediate(resolve))
    }
    await expect(
      current.port.handler(
        { sender },
        routeEnvelope(current, admitted, 3, 'security.watch.unsubscribe', { handle: watch.payload.handle })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'released' } })
    expect(close).toHaveBeenCalledTimes(1)
    await current.binding.destroy()
  }
)

test('directory IPC refuses a stolen renderer lease before OS lookup and preserves unsupported', async () => {
  const connected = jest.fn(async () => {
    throw contractError('capability.unsupported', 'connection', 'os.connected.service-filter-required')
  })
  const current = createMainFixture({ monotonicNow: () => 1000 })
  current.manager.attachedBackend.backend = { peers: { connected } }
  const owner = createSender('directory-owner', 'owner-window', 'owner-session')
  const other = createSender('directory-other', 'other-window', 'other-session')
  const admitted = await bootstrap(current, owner)
  const envelope = routeEnvelope(current, admitted, 1, 'peers.connected', { query: {}, budgetMs: 500 })
  const refused = await current.port.handler({ sender: other }, envelope)
  expect(refused.kind).toBe('failure')
  expect(connected).not.toHaveBeenCalled()
  const result = await current.port.handler({ sender: owner }, envelope)
  expect(result).toMatchObject({
    kind: 'failure',
    error: { code: 'capability.unsupported', operation: 'os.connected.service-filter-required' }
  })
})

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
