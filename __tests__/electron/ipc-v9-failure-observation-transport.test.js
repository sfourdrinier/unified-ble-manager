const { ElectronMainBleRouter } = require('../../src/electron/main-router')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../../src/ipc/protocol')
const { OrderedDelivery } = require('../../src/ipc/ordered-delivery')
const { version, versionRange, opaqueId, monotonicTimestamp } = require('../../src/backend-contract/primitives')

const tick = () => new Promise(resolve => setImmediate(resolve))
function fixture() {
  const axis = name => {
    const selected = version(name, 1)
    const range = versionRange(selected, selected)
    return { axis: name, selected, localRange: range, remoteRange: range }
  }
  const generation = opaqueId('generation', 'backend-generation', 'test')
  const attachment = {
    attachmentId: opaqueId('attachment', 'attachment', 'test'),
    backendInstanceId: opaqueId('backend', 'backend-instance', 'test'),
    backendGeneration: generation,
    adapter: {
      adapterId: opaqueId('adapter', 'adapter', 'test'),
      displayName: null,
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration: generation,
        updatedAt: monotonicTimestamp(1),
        safeReason: null
      },
      adapterGeneration: opaqueId('adapter-generation', 'adapter-generation', 'test'),
      limitations: []
    }
  }
  const manager = {
    attachedBackend: { attachment: { attachment } },
    identity: {
      versions: {
        backendContract: axis('backend-contract'),
        capabilitySchema: axis('capability-schema'),
        eventSchema: axis('event-schema'),
        traceFormat: axis('trace-format')
      }
    },
    capabilities: jest.fn(() => []),
    onAttachmentAdvanced: () => () => {},
    planScan: jest.fn()
  }
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 512,
    maximumOutstandingOperations: 8,
    maximumRetainedBytes: 4096,
    publish: async () => 'delivered'
  })
  const sender = {
    authenticatedClientId: opaqueId('client', 'client', 'test'),
    authenticatedWindowScope: 'window',
    authenticatedSessionScope: 'session'
  }
  return { router, manager, sender }
}
function offer(v) {
  const selected = version('ipc-protocol', v)
  return { ...IPC_CLIENT_COMPATIBILITY_OFFER, ipcProtocol: versionRange(selected, selected) }
}
async function publicRoute(router, sender) {
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  let serial = 0
  return async (command, payload) => {
    const id = String(++serial)
    return (await router.dispatch(sender, { kind: 'route', envelope: {
      versions: boot.versions, attachment: boot.attachment, attachmentId: boot.attachmentId,
      renderer: boot.renderer, rendererLease: boot.rendererLease,
      correlation: opaqueId(id, 'ipc-operation', 'test'), dispatchEpoch: opaqueId(id, 'ipc-dispatch-epoch', 'test'),
      command, payload, binaryPayload: null
    } })).payload
  }
}

test('actual Electron router refuses pre-v6 peers and admits ordinary BLE peers through the current revision', async () => {
  const { router, manager, sender } = fixture()
  for (const v of [3, 4, 5]) {
    await expect(router.dispatch(sender, { kind: 'bootstrap', offer: offer(v) })).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible' }
    })
    expect(manager.capabilities).not.toHaveBeenCalled()
    expect(manager.planScan).not.toHaveBeenCalled()
  }
  for (const v of [6, 7, 8, 9, 10]) {
    const { router: revisionRouter, manager: revisionManager, sender: revisionSender } = fixture()
    const result = await revisionRouter.dispatch(revisionSender, { kind: 'bootstrap', offer: offer(v) })
    expect(result.bootstrap.versions.ipcProtocol.selected.value).toBe(v)
    expect(revisionManager.capabilities).toHaveBeenCalledTimes(1)
  }
})

test('oversized failure observations remain retained and do not discard or acknowledge the source', async () => {
  const { router, sender } = fixture()
  const route = await publicRoute(router, sender)
  const failure = {
    kind: 'failure-observation',
    failure: {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'x'.repeat(1500),
      platform: null,
      retryability: 'never'
    },
    sourceRejection: null,
    sourceFailureMember: null,
    rejectedLoss: null,
    tailUncertain: true
  }
  const iterator = (async function* () {
    yield failure
  })()
  const delivery = new OrderedDelivery(iterator)
  const discard = jest.fn(async () => ({
    group: { state: 'released', failures: [] },
    members: [],
    discarded: { values: '0', bytes: '0', controls: '0', uncertainTail: true, terminal: null, losses: [] }
  }))
  const group = {
    delivery,
    setup: { state: 'ready', stream: { discard } },
    pendingSetupObservations: [],
    cleanupReleased: false,
    cleanupResponse: null
  }
  const resources = [...router.resources.values()][0]
  resources.orderedGroups.set('g', group)
  expect(await route('gatt.ordered.next', { orderedHandle: 'g' })).toEqual({ state: 'empty' })
  await tick()
  const retained = delivery.current
  await expect(route('gatt.ordered.next', { orderedHandle: 'g' })).rejects.toMatchObject({
    normalized: { code: 'bytes.too-large' }
  })
  expect(delivery.current).toEqual(retained)
  expect(discard).not.toHaveBeenCalled()
})

test('oversized values do not discard later failure observations', async () => {
  const { router } = fixture()
  const value = { kind: 'value', memberId: 'm', ordinal: '1', value: new Uint8Array(2000) }
  const delivery = new OrderedDelivery(
    (async function* () {
      yield value
    })()
  )
  const discard = jest.fn(async () => ({
    group: { state: 'released', failures: [] },
    members: [],
    discarded: { values: '0', bytes: '0', controls: '0', uncertainTail: true, terminal: null, losses: [] }
  }))
  const resources = {
    orderedGroups: new Map([
      [
        'g',
        {
          delivery,
          setup: { state: 'ready', stream: { discard } },
          pendingSetupObservations: [],
          cleanupReleased: false,
          cleanupResponse: null
        }
      ]
    ])
  }
  await router.nextOrdered(resources, { orderedHandle: 'g' })
  await tick()
  const retained = delivery.current
  await expect(router.nextOrdered(resources, { orderedHandle: 'g' })).rejects.toMatchObject({
    normalized: { code: 'bytes.too-large' }
  })
  expect(delivery.current).toEqual(retained)
  expect(discard).not.toHaveBeenCalled()
})

test('public transport reads and acknowledges separate abandonment observations', async () => {
  const { router, sender } = fixture()
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  const ownerKey = [...router.resources.values()][0].ownerKey
  const failure = operation => ({
    kind: 'failure-observation',
    failure: { code: 'platform.failure', domain: 'gatt', operation, platform: null, retryability: 'never' },
    sourceRejection: null,
    sourceFailureMember: null,
    rejectedLoss: null,
    tailUncertain: true
  })
  const first = failure('local')
  const second = failure('source')
  const delivery = new OrderedDelivery(
    (async function* () {
      yield first
    })()
  )
  delivery.poll()
  await tick()
  const released = { state: 'released', failures: [] }
  const group = {
    markerKey: 'g',
    delivery,
    pendingSetupObservations: [],
    setup: {
      state: 'ready',
      stream: {
        discard: async () => ({
          group: released,
          members: [],
          discarded: {
            values: '0',
            bytes: '0',
            controls: '1',
            uncertainTail: true,
            terminal: null,
            losses: [],
            failureObservations: [second]
          }
        })
      }
    }
  }
  const resources = { ownerKey, orderedGroups: new Map([['g', group]]), releasedHandles: new Set() }
  expect(await router.releaseOrderedGroups(resources, () => true, true)).toEqual(released)
  let serial = 0
  const diagnostics = require('../../src/ipc/ordered-diagnostics').createDesktopOrderedDiagnostics({ request: async ({ command, payload }) => {
    const id = String(++serial)
    return router.dispatch(sender, { kind: 'route', envelope: {
      versions: boot.versions, attachment: boot.attachment, attachmentId: boot.attachmentId,
      renderer: boot.renderer, rendererLease: boot.rendererLease,
      correlation: opaqueId(id, 'ipc-operation', 'test'), dispatchEpoch: opaqueId(id, 'ipc-dispatch-epoch', 'test'),
      command, payload, binaryPayload: null
    } })
  } })
  const [{ receiptId }] = (await diagnostics.inspect()).receipts
  await expect(diagnostics.acknowledge(receiptId)).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  const receipt = await diagnostics.read(receiptId)
  expect(receipt.failureObservations).toEqual([first, second])
  await diagnostics.acknowledge(receiptId)
  expect((await diagnostics.inspect()).receipts).toEqual([])
})

test('ordered admission is unavailable to a pre-v9 peer before malformed ordered payloads can reach the producer', async () => {
  const { router, manager, sender } = fixture()
  const result = await router.dispatch(sender, { kind: 'bootstrap', offer: offer(8) })
  const boot = result.bootstrap
  for (const command of [
    'gatt.ordered.open',
    'gatt.ordered-recording.reserve',
    'gatt.ordered-recording.start',
    'gatt.ordered-recording.reclaim.reserve',
    'gatt.ordered-recording.reclaim.start',
    'gatt.ordered-recording.reclaim.commit',
    'gatt.ordered.marker.reserve',
    'gatt.ordered.marker.start',
    'gatt.ordered-recording.marker.reserve',
    'gatt.ordered-recording.marker.start'
  ]) {
    const request = {
      kind: 'route',
      envelope: {
        versions: boot.versions,
        attachment: boot.attachment,
        attachmentId: boot.attachmentId,
        renderer: boot.renderer,
        rendererLease: boot.rendererLease,
        correlation: opaqueId(`op-${command}`, 'ipc-operation', 'test'),
        dispatchEpoch: opaqueId(`epoch-${command}`, 'ipc-dispatch-epoch', 'test'),
        command,
        payload: {},
        binaryPayload: null
      }
    }
    await expect(router.dispatch(sender, request)).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
  }
  expect(manager.planScan).not.toHaveBeenCalled()
})

test('a cancellation requested before ordered admission remains an aborted outcome', async () => {
  const { router, sender } = fixture()
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  const envelope = (command, correlation, payload) => ({
    kind: 'route',
    envelope: {
      versions: boot.versions,
      attachment: boot.attachment,
      attachmentId: boot.attachmentId,
      renderer: boot.renderer,
      rendererLease: boot.rendererLease,
      correlation: opaqueId(correlation, 'ipc-operation', 'test'),
      dispatchEpoch: opaqueId(`epoch-${correlation}`, 'ipc-dispatch-epoch', 'test'),
      command,
      payload,
      binaryPayload: null
    }
  })
  const target = envelope('gatt.ordered.open', 'target', {})
  expect(
    await router.dispatch(sender, envelope('operation.cancel', 'cancel', { targetCorrelation: 'target' }))
  ).toMatchObject({ payload: { state: 'cancellation-pending' } })
  await expect(router.dispatch(sender, target)).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
})

test('malformed ordered record remains retained without discarding the source', async () => {
  const { router, sender } = fixture()
  const route = await publicRoute(router, sender)
  const malformed = { kind: 'failure-observation', failure: { unsupported: 1n } }
  const delivery = new OrderedDelivery(
    (async function* () {
      yield malformed
    })()
  )
  const discard = jest.fn()
  const resources = [...router.resources.values()][0]
  resources.orderedGroups.set('g', { delivery, setup: { state: 'ready', stream: { discard } } })
  await route('gatt.ordered.next', { orderedHandle: 'g' })
  await tick()
  const retained = delivery.current
  await expect(route('gatt.ordered.next', { orderedHandle: 'g' })).rejects.toMatchObject({
    normalized: { code: 'protocol.malformed' }
  })
  expect(delivery.current).toEqual(retained)
  expect(discard).not.toHaveBeenCalled()
})

test('a route that changes the negotiated version is rejected before effects', async () => {
  const { router, manager, sender } = fixture()
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  const selected = version('ipc-protocol', 8)
  const old = versionRange(selected, selected)
  const request = {
    kind: 'route',
    envelope: {
      versions: {
        ...boot.versions,
        ipcProtocol: { axis: 'ipc-protocol', selected, localRange: old, remoteRange: old }
      },
      attachment: boot.attachment,
      attachmentId: boot.attachmentId,
      renderer: boot.renderer,
      rendererLease: boot.rendererLease,
      correlation: opaqueId('tampered', 'ipc-operation', 'test'),
      dispatchEpoch: opaqueId('tampered-epoch', 'ipc-dispatch-epoch', 'test'),
      command: 'scan.start',
      payload: {},
      binaryPayload: null
    }
  }
  await expect(router.dispatch(sender, request)).rejects.toMatchObject({
    normalized: { code: 'protocol.incompatible' }
  })
  expect(manager.planScan).not.toHaveBeenCalled()
})

test('Electron binding rejects incompatible bootstrap before retrying retired resource cleanup', async () => {
  const { ElectronMainBleBinding } = require('../../src/electron/main-binding')
  const { router, sender } = fixture()
  router.maximumMessageBytes = 4096
  const nativeSender = {
    mainFrame: { processId: 1, routingId: 2 },
    isDestroyed: () => false,
    once() {},
    on() {},
    removeListener() {},
    send() {}
  }
  let handler
  const port = {
    handle(_channel, callback) {
      handler = callback
    },
    removeHandler() {}
  }
  const binding = new ElectronMainBleBinding({ router, port, authenticate: () => sender })
  binding.install()
  const release = jest.spyOn(binding, 'releaseRetiredSenderRenderers').mockResolvedValue(undefined)
  const event = { sender: nativeSender, processId: 1, frameId: 2 }
  const rejected = await handler(event, { kind: 'bootstrap', offer: offer(5) })
  expect(rejected).toMatchObject({ kind: 'failure', error: { code: 'protocol.incompatible' } })
  expect(release).not.toHaveBeenCalled()
  const accepted = await handler(event, { kind: 'bootstrap', offer: offer(9) })
  expect(accepted.kind).toBe('bootstrap')
  expect(release).toHaveBeenCalledTimes(1)
})

test('real orphan transport rejects unknown terminal evidence without acknowledging and retries the same raw receipt', async () => {
  const { router, sender } = fixture()
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  const owner = [...router.resources.values()][0].ownerKey
  const receipt = { format: 'public', receiptVersion: 2, receiptId: 'r', values: '0', bytes: '0', controls: '1', uncertainTail: true, terminal: { kind: 'terminal', reason: 'closed', uncertainTail: true, futureEvidence: { detail: 'retain me' } }, losses: [], failureObservations: [], failureObservationsOmitted: [] }
  const raw = JSON.stringify(receipt)
  router.orphanReceipts.set(owner, new Map([['r', { record: receipt, scalars: Array.from(raw), cursor: 0, nextOffset: null, page: null, complete: false }]]))
  let serial = 0
  let acknowledgements = 0
  const diagnostics = require('../../src/ipc/ordered-diagnostics').createDesktopOrderedDiagnostics({ request: async ({ command, payload }) => {
    if (command === 'gatt.ordered.orphan-ack') acknowledgements++
    const id = String(++serial)
    return router.dispatch(sender, { kind: 'route', envelope: { versions: boot.versions, attachment: boot.attachment, attachmentId: boot.attachmentId, renderer: boot.renderer, rendererLease: boot.rendererLease, correlation: opaqueId(id, 'ipc-operation', 'test'), dispatchEpoch: opaqueId(id, 'ipc-dispatch-epoch', 'test'), command, payload, binaryPayload: null } })
  } })
  for (let i = 0; i < 2; i++) {
    await expect(diagnostics.read('r')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    await expect(diagnostics.acknowledge('r')).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    expect(router.orphanReceipts.get(owner).get('r').scalars.join('')).toBe(raw)
  }
  expect(acknowledgements).toBe(0)
})

test('public orphan transport retains nested ordered failures and omitted observations exactly', async () => {
  const { router, sender } = fixture()
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer(9) })).bootstrap
  const ownerKey = [...router.resources.values()][0].ownerKey
  const nested = { code: 'platform.failure', domain: 'gatt', operation: 'source', detail: 'detail', platformDomain: 'native', platformCode: 'E', platformMessage: null, platformMetadata: {}, platformMetadataEntries: [{ key: '__proto__', value: { kind: 'int', value: '-9223372036854775808' } }], platformCleanup: [{ domain: 'cleanup', code: 'C', message: null, metadata: JSON.parse('{"constructor":{"kind":"bool","value":true}}'), metadataEntries: [{ key: 'constructor', value: { kind: 'bool', value: true } }], cleanupFailures: [{ code: 'platform.failure', domain: 'gatt', operation: 'release', detail: null, platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} }] }] }
  const observation = { kind: 'failure-observation', failure: { code: nested.code, domain: nested.domain, operation: nested.operation, platform: null, retryability: 'never', orderedFailure: nested }, sourceRejection: { kind: 'fence', membership: { member: '7', epoch: '9' } }, sourceFailureMember: null, rejectedLoss: null, tailUncertain: true }
  const omitted = { kind: 'failure-observations-omitted', count: null }
  const delivery = new OrderedDelivery((async function* () { yield observation })())
  delivery.poll()
  await tick()
  const released = { state: 'released', failures: [] }
  const group = { markerKey: 'g', delivery, pendingSetupObservations: [], setup: { state: 'ready', stream: { discard: async () => ({ group: released, members: [], discarded: { values: '0', bytes: '0', controls: '1', uncertainTail: true, terminal: null, losses: [], failureObservationsOmitted: [omitted] } }) } } }
  await expect(router.releaseOrderedGroups({ ownerKey, orderedGroups: new Map([['g', group]]), releasedHandles: new Set() }, () => true, true)).resolves.toEqual(released)
  let serial = 0
  const diagnostics = require('../../src/ipc/ordered-diagnostics').createDesktopOrderedDiagnostics({ request: async ({ command, payload }) => {
    const id = String(++serial)
    return router.dispatch(sender, { kind: 'route', envelope: { versions: boot.versions, attachment: boot.attachment, attachmentId: boot.attachmentId, renderer: boot.renderer, rendererLease: boot.rendererLease, correlation: opaqueId(id, 'ipc-operation', 'test'), dispatchEpoch: opaqueId(id, 'ipc-dispatch-epoch', 'test'), command, payload, binaryPayload: null } })
  } })
  const [{ receiptId }] = (await diagnostics.inspect()).receipts
  const result = await diagnostics.read(receiptId)
  expect(result.failureObservations[0]).toEqual(observation)
  expect(result.failureObservationsOmitted).toEqual([omitted])
  expect((await diagnostics.read(receiptId)).wireReceipt).toBe(result.wireReceipt)
  await diagnostics.acknowledge(receiptId)
  await diagnostics.acknowledge(receiptId)
})
