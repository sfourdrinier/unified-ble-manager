const {
  createNativeContinuationController,
  createNativeContinuationControl,
  createNativeContinuationControlAccess
} = require('../../../src/backends/desktop/native-continuation-controller')

const peer = '9828347e-45df-2eeb-e928-6e443f4065e3'
const selector = {
  serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
  serviceOccurrence: 1,
  characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
  characteristicOccurrence: 1
}
const declaration = { onAppearance: 'native', peerId: peer, resubscribe: [selector] }
const ok = value => JSON.stringify({ ok: true, value })
const claim = () => ({
  claimToken: 'claim-1',
  consumerCount: 1,
  selectors: [selector],
  batches: [
    JSON.stringify({
      more: false,
      controlLost: 0,
      records: [{ t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: 'AEg=', delivery: 'notification' }]
    })
  ],
  disposed: false,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: null
})

class Central {
  constructor() {
    this.calls = []
    this.prepared = claim()
    this.ackError = null
  }
  async continuationExecute(peerId, json) {
    this.calls.push(['execute', peerId, JSON.parse(json)])
    return ok({ event: 'continuation.completed', strategy: 'native', peerAddress: peerId, resubscribed: 1 })
  }
  async continuationPrepareClaim(items, bytes) {
    this.calls.push(['prepare', items, bytes])
    return ok(this.prepared)
  }
  async continuationAcknowledgeClaim(token) {
    this.calls.push(['ack', token])
    if (this.ackError) throw this.ackError
    return ok({ disposed: true, afterCutoffLoss: { items: 2, bytes: 4 }, disposeFailure: null })
  }
  async continuationDescribeBacklog() {
    this.calls.push(['status'])
    return ok({ queuedData: 1, lastError: null, continuationOutcome: null })
  }
}

test('internal raw access preserves envelopes and does not acknowledge a prepared claim', async () => {
  const central = new Central()
  const access = createNativeContinuationControlAccess(central)
  expect(JSON.parse(await access.prepareClaim(7, 4096))).toEqual({ ok: true, value: claim() })
  expect(central.calls).toEqual([['prepare', 7, 4096]])
  expect(JSON.parse(await access.acknowledgeClaim('claim-1')).ok).toBe(true)
  expect(central.calls[1]).toEqual(['ack', 'claim-1'])
})

test('transport-only controller preserves public bytes and receiver without storage path or central methods', async () => {
  const central = new Central()
  const access = {
    central,
    execute(peerId, declarationJson) {
      return this.central.continuationExecute(peerId, declarationJson)
    },
    describeBacklog() {
      return this.central.continuationDescribeBacklog()
    },
    prepareClaim(items, bytes) {
      return this.central.continuationPrepareClaim(items, bytes)
    },
    acknowledgeClaim(token) {
      return this.central.continuationAcknowledgeClaim(token)
    }
  }
  const control = createNativeContinuationControl(access)
  expect(Object.keys(control).sort()).toEqual(['claim', 'execute', 'status'])
  expect(await control.execute(declaration)).toMatchObject({ peerAddress: peer, resubscribed: 1 })
  expect(await control.status()).toMatchObject({ queuedData: 1 })
  central.prepared.batches = ['malformed']
  await expect(control.claim()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(central.calls.filter(call => call[0] === 'ack')).toHaveLength(0)
  central.prepared = claim()
  access.prepareClaim = async () => {
    throw new Error('transport lost before payload')
  }
  await expect(control.claim()).rejects.toMatchObject({ code: 'platform.failure' })
  expect(central.calls.filter(call => call[0] === 'ack')).toHaveLength(0)
  access.prepareClaim = (items, bytes) => central.continuationPrepareClaim(items, bytes)
  central.ackError = new Error('uncertain ACK')
  const retained = await control.claim()
  expect(retained.values[0].value).toBeInstanceOf(Uint8Array)
  expect([...retained.values[0].value]).toEqual([0, 72])
  expect(retained.disposed).toBe(false)
  expect(retained.disposeFailure).toMatch(/uncertain/)
  central.ackError = null
  const replay = await control.claim()
  expect([...replay.values[0].value]).toEqual([0, 72])
  expect(replay.disposed).toBe(true)
  expect(central.calls.filter(call => call[0] === 'ack')).toEqual([
    ['ack', 'claim-1'],
    ['ack', 'claim-1']
  ])
})

test('renderer exports provide codec controls without trusted path configuration', () => {
  for (const entrypoint of ['../../../src/electron-renderer', '../../../src/tauri']) {
    const host = require(entrypoint)
    expect(host.createNativeContinuationControl).toBe(createNativeContinuationControl)
    expect(typeof host.createNativeContinuationRecordingController).toBe('function')
    expect(host.createNativeContinuationController).toBeUndefined()
    expect(host.loadDesktopCoreBinding).toBeUndefined()
  }
})

test('uses the existing receiver-dependent central without opening or closing a radio', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  expect(await controller.execute(declaration)).toEqual({
    event: 'continuation.completed',
    strategy: 'native',
    peerAddress: peer,
    resubscribed: 1
  })
  // Both the declaration and transport preserve the exact OS identity.
  expect(central.calls[0]).toEqual(['execute', peer, declaration])
  expect(await controller.status()).toEqual({ queuedData: 1, lastError: null, continuationOutcome: null })
  const backlog = await controller.claim()
  expect([...backlog.values[0].value]).toEqual([0, 72])
  expect(backlog.disposed).toBe(true)
  expect(backlog.afterCutoffLoss).toEqual({ items: 2, bytes: 4 })
  expect(central.calls).toHaveLength(4)
})

test('configures live recording storage on the same owned native engine', async () => {
  const central = new Central()
  central.continuationConfigureRecordingDirectory = async function (directory) {
    this.calls.push(['recording-directory', directory])
    return ok({ state: 'configured', encrypted: false })
  }
  central.continuationRecordingStore = function () {
    this.calls.push(['recording-store'])
    return { prepare: async () => ok({ token: null, records: [], bytes: 0, more: false }) }
  }
  const controller = createNativeContinuationController(central)
  const recordings = await controller.recordings('/private/app/recordings')
  await recordings.prepare('h10', { maxItems: 10, maxBytes: 4096 })
  expect(central.calls).toEqual([['recording-directory', '/private/app/recordings'], ['recording-store']])
  central.continuationConfigureRecordingDirectory = async () => ok({ state: 'configured', encrypted: true })
  await expect(controller.recordings('/private/app/recordings')).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('the published Node recorder recipe executes, returns positive values and closes its one central', async () => {
  const fs = require('fs')
  const path = require('path')
  const guide = fs.readFileSync(path.join(__dirname, '../../../docs/NODE.md'), 'utf8')
  const section = guide.split('## Native continuation in a trusted process host')[1]
  const snippet = section.match(/```ts\n([\s\S]*?)\n```/)[1]
  const central = new Central()
  central.close = jest.fn(async () => ({ state: 'released' }))
  const openProduction = jest.fn(async () => central)
  const loadDesktopCoreBinding = jest.fn(async () => ({ openProduction }))
  const run = new Function(
    'loadDesktopCoreBinding',
    'createNativeContinuationController',
    'console',
    `return (async () => { ${snippet.replace(/^import .*\n/m, '')}\nconst result = await finishRecording(); await shutdownRecorderHost(); return result; })()`
  )
  const backlog = await run(loadDesktopCoreBinding, createNativeContinuationController, { info: jest.fn() })
  expect([...backlog.values[0].value]).toEqual([0, 72])
  expect(backlog.disposed).toBe(true)
  expect(loadDesktopCoreBinding).toHaveBeenCalledWith({ platform: 'corebluetooth', operationPrefix: 'direct-gatt' })
  expect(openProduction).toHaveBeenCalledTimes(1)
  expect(central.close).toHaveBeenCalledTimes(1)
  expect(central.calls.map(call => call[0])).toEqual(['execute', 'prepare', 'ack'])
})

test('never acknowledges malformed data, preserves decoded values after uncertain acknowledgement', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  central.prepared.batches = ['not-json']
  await expect(controller.claim()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(central.calls.some(call => call[0] === 'ack')).toBe(false)
  central.prepared = claim()
  central.ackError = new Error('transport disappeared')
  const backlog = await controller.claim()
  expect([...backlog.values[0].value]).toEqual([0, 72])
  expect(backlog.disposed).toBe(false)
  expect(backlog.disposeFailure).toMatch(/uncertain/)
})

test('rejects invalid declarations, missing identity, unsupported strategy and invalid bounds before dispatch', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  for (const value of [
    { onAppearance: 'native' },
    { ...declaration, peerId: 'invalid\u0000identity' },
    { onAppearance: 'record-only' }
  ]) {
    await expect(controller.execute(value)).rejects.toMatchObject({ code: 'argument.invalid' })
  }
  await expect(controller.claim({ maxItems: 2 ** 32 })).rejects.toMatchObject({ code: 'argument.invalid' })
  expect(central.calls).toEqual([])
})

test('preserves BlueZ adapter-scoped identities and rejects case-aliased outcomes', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  const bluez = 'hci1/dev_AA_BB_CC_DD_EE_FF'
  expect((await controller.execute({ ...declaration, peerId: bluez })).peerAddress).toBe(bluez)
  expect(central.calls[0]).toEqual(['execute', bluez, { ...declaration, peerId: bluez }])
  central.continuationExecute = async peerId =>
    ok({ event: 'continuation.completed', strategy: 'native', peerAddress: peerId.toUpperCase(), resubscribed: 1 })
  await expect(controller.execute({ ...declaration, peerId: bluez })).rejects.toMatchObject({
    code: 'protocol.malformed'
  })
  for (const peerId of ['', 'x'.repeat(1025), ' hci1/dev_AA_BB_CC_DD_EE_FF', 'hci1/dev_AA\nBB']) {
    await expect(controller.execute({ ...declaration, peerId })).rejects.toMatchObject({ code: 'argument.invalid' })
  }
})

test('requires a link result for exactly the declared negotiation', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  const link = { mtu: { requested: 512, timeoutMs: 10000, onUnsupported: 'continue' } }
  await expect(controller.execute({ ...declaration, link })).rejects.toMatchObject({ code: 'protocol.malformed' })
  central.continuationExecute = async peerAddress =>
    ok({
      event: 'continuation.completed',
      strategy: 'native',
      peerAddress,
      resubscribed: 1,
      link: { mtu: { requested: 247, outcome: 'negotiated', mtu: 247 } }
    })
  await expect(controller.execute({ ...declaration, link })).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(controller.execute(declaration)).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('rejects malformed envelopes and retains the native failure identity and retryability', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  central.continuationExecute = async () =>
    JSON.stringify({
      ok: false,
      error: {
        code: 'connection.failed',
        domain: 'connection',
        operation: 'connection.connect',
        detail: 'out of range',
        platform: { domain: 'corebluetooth', code: '6', message: 'timeout', metadata: {} }
      },
      commit: null,
      retryability: 'caller-decides'
    })
  await expect(controller.execute(declaration)).rejects.toMatchObject({
    code: 'connection.failed',
    retryability: 'caller-decides',
    platform: { domain: 'corebluetooth', code: '6' }
  })
  central.continuationPrepareClaim = async () => '{'
  await expect(controller.claim()).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('reports typed native recovery failures and refuses malformed status or mismatched completion identity', async () => {
  const central = new Central()
  const controller = createNativeContinuationController(central)
  const error = {
    code: 'connection.failed',
    domain: 'connection',
    operation: 'connection.connect',
    detail: 'out of range',
    retryability: 'caller-decides'
  }
  central.continuationDescribeBacklog = async () =>
    ok({
      queuedData: 0,
      lastError: error,
      continuationOutcome: {
        event: 'continuation.failed',
        strategy: 'native',
        error: { code: error.code, domain: error.domain, operation: error.operation, detail: error.detail },
        retryability: 'caller-decides',
        attempt: 2
      }
    })
  const status = await controller.status()
  expect(status.lastError).toMatchObject({
    code: 'connection.failed',
    retryability: 'caller-decides',
    platform: { domain: 'ubm-desktop' }
  })
  expect(status.continuationOutcome).toMatchObject({
    event: 'continuation.failed',
    attempt: 2,
    retryability: 'caller-decides',
    error: { code: 'connection.failed' }
  })
  central.continuationDescribeBacklog = async () => ok({ queuedData: -1, lastError: null, continuationOutcome: null })
  await expect(controller.status()).rejects.toMatchObject({ code: 'protocol.malformed' })
  central.continuationDescribeBacklog = async () => ok(null)
  expect(await controller.status()).toBeNull()
  central.continuationExecute = async () =>
    ok({ event: 'continuation.completed', strategy: 'native', peerAddress: 'AA:BB:CC:DD:EE:FF', resubscribed: 1 })
  await expect(controller.execute(declaration)).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('a backend missing the native owner is explicitly unsupported and all desktop host exports are available', async () => {
  const controller = createNativeContinuationController({})
  await expect(controller.execute(declaration)).rejects.toMatchObject({ code: 'capability.unsupported' })
  await expect(controller.claim()).rejects.toMatchObject({ code: 'capability.unsupported' })
  await expect(controller.status()).rejects.toMatchObject({ code: 'capability.unsupported' })
  for (const entry of ['node-bluez', 'node-corebluetooth', 'node-winrt', 'electron-main']) {
    const host = require(`../../../src/${entry}`)
    expect(host.createNativeContinuationController).toBe(createNativeContinuationController)
    expect(typeof host.loadDesktopCoreBinding).toBe('function')
  }
})
