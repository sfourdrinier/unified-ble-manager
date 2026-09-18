// __tests__/backends/reactnative/rust-core-provider.test.js
//
// The React Native Rust route end to end, through the ordinary factory
// (`createReactNativeBleManagerWithEnvironment`), the REAL production binding
// and serializer, and a deterministic `UnifiedBleRustCore` module that speaks
// `ubm-mobile-wire/1` exactly as the Rust owner does. Each describe names the
// review finding it proves.

const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER, defaultPeripheral } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { MAX_OPERATION_BYTES } = require('../../../src/backends/reactnative/rust-core-wire')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(harnessOptions = {}, overrides = {}) {
  const harness = rustCoreHarness(harnessOptions)
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, overrides))
  return { harness, native: harness.native, manager, backend: manager.attachedBackend.backend }
}

async function connectDefault(manager, backend, peer = DEFAULT_PEER) {
  const peerId = backend.connections.peerFromAddress({ address: peer, addressType: 'public' })
  return manager.connect(peerId, NO_OPTIONS)
}

async function discover(connection) {
  const database = await connection.discover(NO_OPTIONS)
  const snapshot = await database.snapshot()
  return { database, path: snapshot.characteristics[0].path, snapshot }
}

async function take(stream) {
  return stream[Symbol.asyncIterator]().next()
}

async function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected a rejection')
    },
    error => error.normalized ?? error
  )
}

function indicateOnlyPeripheral() {
  const peripheral = defaultPeripheral()
  peripheral.services[0].characteristics[0].properties = 0x10
  return peripheral
}

describe('Polar-style journey through the ordinary factory', () => {
  test('scan, connect, discover, read, write, subscribe, notify, unsubscribe, disconnect, destroy', async () => {
    const { native, manager, backend } = await openManager()
    const scan = await manager.scan(scanOptions())
    native.emitAdvertisement()
    const advertisement = await take(scan.observations)
    const observation = advertisement.value.value
    expect(observation.localName).toMatchObject({ state: 'present', value: 'Polar H10 1234' })
    expect([...observation.manufacturerData.value[0].value]).toEqual([0x00, 0x80, 0xff])
    expect(observation.device.address).toEqual({ value: DEFAULT_PEER, type: 'opaque' })
    expect((await scan.stop()).state).toBe('released')

    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    expect([...(await database.read(path, NO_OPTIONS))]).toEqual([0x00, 0x48])
    const receipt = await database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'with-response' })
    expect(receipt.commitState).toBe('confirmed')

    const subscription = await database.subscribe(path, subscribeOptions())
    native.emitNotification(new Uint8Array([0x10, 0x55, 0x20, 0x03]))
    const item = await take(subscription.values)
    expect([...item.value.value.value]).toEqual([0x10, 0x55, 0x20, 0x03])
    expect(item.value.value.delivery).toBe('notification')
    expect((await subscription.remove()).state).toBe('released')
    expect((await connection.disconnect()).state).toBe('released')
    expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
    expect(native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(native.calls.filter(call => call[0] === 'closeSession')).toHaveLength(1)
  })
})

describe('PR210-12 byte forms cross the production serializer byte-exactly without Buffer/atob', () => {
  const removed = {}
  beforeAll(() => {
    for (const name of ['atob', 'btoa']) {
      removed[name] = Object.getOwnPropertyDescriptor(globalThis, name)
      delete globalThis[name]
    }
  })
  afterAll(() => {
    for (const [name, descriptor] of Object.entries(removed)) {
      if (descriptor !== undefined) Object.defineProperty(globalThis, name, descriptor)
    }
  })

  test.each([
    ['0x00', [0x00]],
    ['0x80', [0x80]],
    ['0xff', [0xff]],
    ['empty', []],
    ['mixed', [0x00, 0x80, 0xff, 0x7f, 0x01]]
  ])('write then read %s', async (_name, bytes) => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    await database.write(path, new Uint8Array(bytes), { ...NO_OPTIONS, mode: 'with-response' })
    const sent = native.opsInvoked('gatt.write')[0]
    expect(typeof sent.valueB64).toBe('string')
    expect([...(await database.read(path, NO_OPTIONS))]).toEqual(bytes)
    await manager.destroy()
  })

  test('a subarray view sends only its own bytes; the maximum payload round-trips', async () => {
    const { manager, backend } = await openManager({}, { diagnostics: { maximumValueBytes: MAX_OPERATION_BYTES } })
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const backing = new Uint8Array([9, 9, 1, 2, 3, 9])
    await database.write(path, backing.subarray(2, 5), { ...NO_OPTIONS, mode: 'with-response' })
    expect([...(await database.read(path, NO_OPTIONS))]).toEqual([1, 2, 3])
    const maximum = Uint8Array.from({ length: MAX_OPERATION_BYTES }, (_, index) => index & 0xff)
    await database.write(path, maximum, { ...NO_OPTIONS, mode: 'with-response' })
    const back = await database.read(path, NO_OPTIONS)
    expect(back.byteLength).toBe(MAX_OPERATION_BYTES)
    expect(back[MAX_OPERATION_BYTES - 1]).toBe((MAX_OPERATION_BYTES - 1) & 0xff)
    await manager.destroy()
  })

  test('an oversize value is refused before the native module sees it', async () => {
    const { native, manager, backend } = await openManager(
      {},
      { diagnostics: { maximumValueBytes: MAX_OPERATION_BYTES + 1 } }
    )
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const error = await failure(
      database.write(path, new Uint8Array(MAX_OPERATION_BYTES + 1), { ...NO_OPTIONS, mode: 'with-response' })
    )
    expect(error.code).toBe('bytes.too-large')
    expect(native.opsInvoked('gatt.write')).toHaveLength(0)
    await manager.destroy()
  })

  test('malformed base64 from the owner is protocol.malformed, never a guessed value', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    native.hold('gatt.read')
    const read = database.read(path, NO_OPTIONS)
    await settle()
    native.release('gatt.read', { valueB64: 'AA=A' })
    expect((await failure(read)).code).toBe('protocol.malformed')
    await manager.destroy()
  })
})

describe('PR210-13 receipts and delivery are the owner’s facts', () => {
  test('without-response reports unknown, with-response confirmed; a confirmed without-response receipt is refused', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    expect(
      (await database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'without-response' })).commitState
    ).toBe('unknown')
    expect(
      (await database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'with-response' })).commitState
    ).toBe('confirmed')
    native.hold('gatt.write')
    const lying = database.write(path, new Uint8Array([2]), { ...NO_OPTIONS, mode: 'without-response' })
    await settle()
    native.release('gatt.write', { commitState: 'confirmed' })
    expect((await failure(lying)).code).toBe('protocol.malformed')
    await manager.destroy()
  })

  test('a failed write carries the owner’s commit state and retryability', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    native.failNext('gatt.write', 'gatt.write-failed', 'gatt', 'ubm-mobile.gatt.write', 'status 133', 'uncertain')
    const uncertain = await failure(database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'with-response' }))
    expect(uncertain).toMatchObject({ code: 'gatt.write-failed', commit: 'uncertain', retryability: 'never' })
    native.failNext('gatt.write', 'operation.timed-out', 'core', 'ubm-mobile.gatt.write', null, 'not-dispatched')
    const notDispatched = await failure(
      database.write(path, new Uint8Array([1]), { ...NO_OPTIONS, mode: 'with-response' })
    )
    expect(notDispatched).toMatchObject({ commit: 'not-dispatched', retryability: 'caller-decides' })
    await manager.destroy()
  })

  test('Android reports the CCCD mode written; an indication-only characteristic delivers indications', async () => {
    const { native, manager, backend } = await openManager({
      nativeOptions: { peripherals: [indicateOnlyPeripheral()] }
    })
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    native.emitNotification(new Uint8Array([7]))
    expect((await take(subscription.values)).value.value.delivery).toBe('indication')
    expect(native.opsInvoked('gatt.subscribe')[0]).not.toHaveProperty('deliveryMode')
    await subscription.remove()
    const refused = await failure(database.subscribe(path, subscribeOptions({ deliveryMode: 'require-notification' })))
    expect(refused.code).toBe('gatt.property-not-supported')
    await manager.destroy()
  })

  test('Apple delivery is unknown — never reported as a notification', async () => {
    const { native, manager, backend } = await openManager({ platform: 'apple' }, { androidApiLevel: undefined })
    const peerId = backend.peerIdForNativeId(DEFAULT_PEER)
    const connection = await manager.connect(peerId, NO_OPTIONS)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions({ deliveryMode: 'require-notification' }))
    native.emitNotification(new Uint8Array([7]))
    expect((await take(subscription.values)).value.value.delivery).toBe('unknown')
    await manager.destroy()
  })
})

describe('PR210-14 counters, events, cancellation and dispose come from the owner', () => {
  test('counters are non-zero while resources live and return to baseline after cleanup', async () => {
    const { manager, backend } = await openManager()
    expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 0, physicalLinks: 0 })
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    expect(backend.resourceCounters()).toMatchObject({
      connectionLeases: 1,
      physicalLinks: 1,
      databaseSnapshots: 1,
      physicalCccdEnablements: 1,
      subscriptionConsumers: 1
    })
    await subscription.remove()
    await connection.disconnect()
    expect(backend.resourceCounters()).toMatchObject({
      connectionLeases: 0,
      physicalLinks: 0,
      subscriptionConsumers: 0
    })
    const described = await backend.hostServices.counters()
    expect(described.native).toMatchObject({ pendingRadioRequests: 0, liveOps: 0 })
    await manager.destroy()
  })

  test('PR210-53: two managers count their own resources and return to baseline independently', async () => {
    const first = await openManager({}, { managerId: 'manager-a' })
    const second = await openManager({ native: first.native }, { managerId: 'manager-b' })
    const connection = await connectDefault(first.manager, first.backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    const scan = await second.manager.scan(scanOptions())
    await settle()
    expect(first.backend.resourceCounters()).toMatchObject({
      connectionLeases: 1,
      physicalLinks: 1,
      subscriptionConsumers: 1,
      scanConsumers: 0
    })
    await second.backend.hostServices.counters()
    expect(second.backend.resourceCounters()).toMatchObject({
      connectionLeases: 0,
      physicalLinks: 0,
      subscriptionConsumers: 0,
      scanConsumers: 1
    })
    const described = await second.backend.hostServices.counters()
    expect(described.process.counters).toMatchObject({
      connectionLeases: 1,
      scanConsumers: 1,
      subscriptionConsumers: 1
    })
    await subscription.remove()
    await connection.disconnect()
    await first.manager.destroy()
    expect(first.backend.resourceCounters()).toMatchObject({
      connectionLeases: 0,
      physicalLinks: 0,
      subscriptionConsumers: 0,
      scanConsumers: 0
    })
    expect((await second.backend.hostServices.counters()).counters).toMatchObject({ scanConsumers: 1 })
    expect((await scan.stop()).state).toBe('released')
    expect(second.backend.resourceCounters()).toMatchObject({ scanConsumers: 0, activeScanControllers: 0 })
    await second.manager.destroy()
  })

  test('a release-failed dispose is reported, keeps the lease open, and a retried destroy disposes again', async () => {
    const { native, manager } = await openManager()
    native.disposeRecords.push({
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'connection',
          code: 'operation.timed-out',
          domain: 'core',
          operation: 'ubm-mobile.connection.release',
          detail: 'radio close timed out',
          platform: null
        }
      ]
    })
    const first = await manager.destroy()
    expect(first.state).toBe('release-failed')
    expect(first.failures[0]).toMatchObject({ resourceKind: 'connection', error: { code: 'operation.timed-out' } })
    expect(native.calls.filter(call => call[0] === 'closeSession')).toHaveLength(0)
    const second = await manager.destroy()
    expect(second).toEqual({ state: 'released', failures: [] })
    expect(native.opsInvoked('session.dispose')).toHaveLength(2)
    expect(native.calls.filter(call => call[0] === 'closeSession')).toHaveLength(1)
  })

  test('an idle link loss surfaces as a typed event and ends the subscription with connection-lost', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    const lifecycle = []
    const reader = (async () => {
      for await (const item of connection.events) {
        if (item.kind !== 'value') break
        lifecycle.push(item.value.cause)
      }
    })()
    native.dropLink(DEFAULT_PEER, 'peer')
    await settle(60)
    const end = await take(subscription.values)
    expect(end.value).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
    await reader
    expect(lifecycle).toEqual(['connected', 'peer-link-loss'])
    await manager.destroy()
  })

  test('an abort cancels exactly that operation with op.cancel; nothing else is cancelled', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    native.hold('gatt.read')
    const controller = new AbortController()
    const read = database.read(path, { signal: controller.signal, deadline: null })
    await settle()
    const { operationId: readId, admission } = native.opsInvoked('gatt.read')[0]
    controller.abort()
    expect((await failure(read)).code).toBe('operation.aborted')
    expect(native.opsInvoked('op.cancel')).toEqual([{ operationId: readId, admission }])
    await manager.destroy()
  })

  test('an expired deadline never reaches the owner; a live one travels as budgetMs', async () => {
    let now = 1000
    const { native, manager, backend } = await openManager({}, { now: () => now })
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    await database.read(path, { signal: null, deadline: 1500 })
    expect(native.opsInvoked('gatt.read')[0].budgetMs).toBe(500)
    now = 2000
    const expired = await failure(database.read(path, { signal: null, deadline: 1999 }))
    expect(expired.code).toBe('operation.timed-out')
    expect(native.opsInvoked('gatt.read')).toHaveLength(1)
    await manager.destroy()
  })
})

describe('PR210-16 core generations and typed invalidation', () => {
  test('after a link loss, handles of the old generation fail before any native I/O', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    native.dropLink(DEFAULT_PEER, 'peer')
    await settle(60)
    const reads = native.opsInvoked('gatt.read').length
    const error = await failure(database.read(path, NO_OPTIONS))
    expect(['connection.stale', 'gatt.stale-handle']).toContain(error.code)
    expect(native.opsInvoked('gatt.read')).toHaveLength(reads)
    await manager.destroy()
  })

  test('a database change invalidates the old database handles before native I/O', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const oldGeneration = database.path.databaseGeneration
    native.changeDatabase(DEFAULT_PEER)
    await settle(60)
    const error = await failure(
      (async () => backend.gatt.read(path, { operation: { ...NO_OPTIONS, correlation: 'x-1' } }).completion)()
    )
    expect(error.code).toBe('gatt.stale-handle')
    expect(native.opsInvoked('gatt.read')).toHaveLength(0)
    const rediscovered = await connection.discover(NO_OPTIONS)
    expect(rediscovered.path.databaseGeneration).not.toBe(oldGeneration)
    await manager.destroy()
  })

  test('the attachment carries the owner’s generations; a changed generation restarts the backend', async () => {
    const { native, manager, backend } = await openManager()
    expect(String(backend.identity.attachment.backendGeneration)).toBe('backend-gen-1')
    expect(String(backend.identity.attachment.adapter.adapterGeneration)).toBe('adapter-gen-1')
    const events = backend.events()
    native.setAdapter({ backendGeneration: 'backend-gen-2' })
    await settle(60)
    const kinds = []
    for (;;) {
      const item = await take(events)
      if (item.value.kind !== 'value') break
      kinds.push(item.value.value.kind)
      if (kinds.includes('adapter-state')) break
    }
    expect(kinds).toEqual(['backend-restarted', 'adapter-state'])
    await manager.destroy()
  })

  test.each([
    ['fractional occurrence', discovery => ((discovery.services[0].occurrence = 1.9), discovery)],
    ['negative occurrence', discovery => ((discovery.services[0].occurrence = -1), discovery)],
    ['oversized occurrence', discovery => ((discovery.services[0].occurrence = 2 ** 60), discovery)],
    ['unknown property bit', discovery => ((discovery.services[0].characteristics[0].properties = 0x80), discovery)],
    ['non-canonical uuid', discovery => ((discovery.services[0].uuid = '180D'), discovery)]
  ])('%s is refused and the previous snapshot stays valid', async (_name, corrupt) => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, snapshot } = await discover(connection)
    native.discoveryOverride = corrupt
    const error = await failure(backend.gatt.discover(connection.resource ?? connection, NO_OPTIONS))
    expect(error.code).toBe('protocol.malformed')
    expect(await database.snapshot()).toEqual(snapshot)
    await manager.destroy()
  })
})

describe('PR210-17 wake-driven delivery, byte charges and watcher removal', () => {
  afterEach(() => {
    jest.useRealTimers()
  })

  test('an idle manager with one scan and one subscription makes zero native calls over 10 s', async () => {
    const { native, manager, backend } = await openManager()
    const scan = await manager.scan(scanOptions())
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    await settle(60)
    jest.useFakeTimers()
    const mark = native.calls.length
    jest.advanceTimersByTime(10000)
    await settle(60)
    expect(native.callsSince(mark)).toEqual([])
    jest.useRealTimers()
    native.emitNotification(new Uint8Array([1]))
    await take(subscription.values)
    const drains = native.callsSince(mark).filter(call => call[0] === 'drain')
    expect(drains.length).toBeGreaterThanOrEqual(1)
    expect(drains.length).toBeLessThanOrEqual(2)
    await subscription.remove()
    await scan.stop()
    await manager.destroy()
  })

  test('the source has no polling pump', () => {
    const fs = require('node:fs')
    const path = require('node:path')
    const directory = path.join(__dirname, '../../../src/backends/reactnative')
    for (const file of fs.readdirSync(directory).filter(name => name.startsWith('react-native-rust-core'))) {
      const source = fs.readFileSync(path.join(directory, file), 'utf8')
      expect(source).not.toMatch(/pumpDelay|setTimeout\(resolve/)
      expect(source).not.toMatch(/'(scan|notifications|events)\.take'/)
    }
  })

  test('a notification is charged its actual payload bytes', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    native.emitNotification(new Uint8Array(37))
    await settle(60)
    expect(subscription.values.retainedPayloadBytes()).toBe(37)
    await manager.destroy()
  })

  test('closed adapter watches and event streams deregister; the wake subscription ends with the last session', async () => {
    const { native, manager, backend } = await openManager()
    for (let cycle = 0; cycle < 50; cycle += 1) {
      const watch = await manager.adapterStates()
      await watch.stop()
      const events = backend.events()
      await events.close()
    }
    native.setAdapter({ power: 'off' })
    await settle(60)
    expect(native.wakeListeners.size).toBe(1)
    await manager.destroy()
    expect(native.wakeListeners.size).toBe(0)
  })
})

describe('PR210-70 the diagnostic trace is recorded within the configured bounds', () => {
  test('operations leave a payload-free, ordered trace', async () => {
    const { manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    await database.read(path, NO_OPTIONS)
    await connection.disconnect()
    const document = manager.traceDocument()
    expect(document.format).toBe('unified-ble-trace-v1')
    expect(document.truncated).toBe(false)
    const events = document.records.map(record => record.event)
    expect(events).toEqual(expect.arrayContaining(['dispatched', 'succeeded']))
    expect(document.records.map(record => record.ordinal)).toEqual(document.records.map((_record, index) => index + 1))
    const text = JSON.stringify(document)
    expect(text).not.toContain(DEFAULT_PEER)
    expect(document.records.every(record => record.redactedPeer && record.redactedPayload)).toBe(true)
    expect(manager.traces()).toHaveLength(document.records.length)
    await manager.destroy()
  })

  test('traceMaximumRecords bounds the trace and marks it truncated', async () => {
    const { manager, backend } = await openManager({}, { diagnostics: { traceMaximumRecords: 2 } })
    const connection = await connectDefault(manager, backend)
    await discover(connection)
    const document = manager.traceDocument()
    expect(document.records).toHaveLength(2)
    expect(document.truncated).toBe(true)
    await manager.destroy()
  })

  test('a failed operation records its cause', async () => {
    const { native, manager, backend } = await openManager()
    native.failNext('connection.connect', 'connection.failed', 'connection', 'ubm-mobile.connect', 'gatt 133')
    await failure(connectDefault(manager, backend))
    const failed = manager.traceDocument().records.find(record => record.event === 'failed')
    expect(failed).toMatchObject({ kind: 'operation', cause: 'connection.failed' })
    await manager.destroy()
  })

  test('out-of-range bounds are refused before any session opens', async () => {
    const harness = rustCoreHarness()
    await expect(
      createReactNativeBleManagerWithEnvironment(environment(harness, { diagnostics: { traceMaximumRecords: 0 } }))
    ).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(harness.native.calls.filter(call => call[0] === 'openSession')).toHaveLength(0)
  })
})

describe('PR210-69 Android appearance and raw record reach the observation', () => {
  test('reported values are present byte-exactly; unreported ones are unavailable', async () => {
    const { native, manager } = await openManager()
    const scan = await manager.scan(scanOptions())
    native.emitAdvertisement(DEFAULT_PEER, { appearance: 833, rawRecordB64: 'AgEG' })
    native.emitAdvertisement(DEFAULT_PEER)
    const reported = (await take(scan.observations)).value.value
    expect(reported.appearance).toMatchObject({ state: 'present', value: 833 })
    expect([...reported.rawRecord.value]).toEqual([2, 1, 6])
    const unreported = (await take(scan.observations)).value.value
    expect(unreported.appearance.state).not.toBe('present')
    expect(unreported.rawRecord.state).not.toBe('present')
    await scan.stop()
    await manager.destroy()
  })
})

describe('PR210-54 preferredPhy at connect is honoured or refused, never ignored', () => {
  test('Android establishes the link on the preferred PHYs', async () => {
    const { native, manager, backend } = await openManager()
    const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const connection = await manager.connect(peerId, { ...NO_OPTIONS, preferredPhy: ['le-2m', 'le-coded'] })
    expect(native.opsInvoked('connection.connect')[0].preferredPhy).toEqual(['le-2m', 'le-coded'])
    expect(native.connects).toEqual([
      expect.objectContaining({ peerId: DEFAULT_PEER, preferredPhy: ['le-2m', 'le-coded'] })
    ])
    await connection.disconnect()
    await manager.destroy()
  })

  test('a preference the platform cannot apply is refused before any link', async () => {
    const apple = await openManager({ platform: 'apple' })
    const refused = await failure(
      apple.manager.connect(apple.backend.peerIdForNativeId(DEFAULT_PEER), { ...NO_OPTIONS, preferredPhy: ['le-2m'] })
    )
    expect(refused.code).toBe('capability.unsupported')
    expect(apple.native.connects).toEqual([])
    await apple.manager.destroy()
    const android = await openManager()
    const peerId = android.backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const autoConnect = await failure(
      android.manager.connect(peerId, { ...NO_OPTIONS, intent: 'when-available', preferredPhy: ['le-1m'] })
    )
    expect(autoConnect.code).toBe('capability.unsupported')
    expect(android.native.connects).toEqual([])
    await android.manager.destroy()
  })
})

describe('PR210-09 cleanup keeps its identity until the owner confirms release', () => {
  test('scan stop: first attempt fails, second succeeds against the same membership', async () => {
    const { native, manager } = await openManager()
    const scan = await manager.scan(scanOptions())
    native.failNext('scan.stop', 'scan.stop-failed', 'scan', 'ubm-mobile.scan.stop', 'radio busy')
    const first = await scan.stop()
    expect(first.state).toBe('release-failed')
    expect(first.failures[0].error.code).toBe('scan.stop-failed')
    const second = await scan.stop()
    expect(second.state).toBe('released')
    const stops = native.opsInvoked('scan.stop')
    expect(stops).toHaveLength(2)
    expect(stops[0].operationId).toBe(stops[1].operationId)
    await manager.destroy()
  })

  test('unsubscribe: a failed release keeps the consumer; the retry releases the same consumer', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    const { database, path } = await discover(connection)
    const subscription = await database.subscribe(path, subscribeOptions())
    native.failNext('gatt.unsubscribe', 'gatt.subscribe-failed', 'gatt', 'ubm-mobile.gatt.unsubscribe')
    expect((await subscription.remove()).state).toBe('release-failed')
    expect((await subscription.remove()).state).toBe('released')
    const attempts = native.opsInvoked('gatt.unsubscribe')
    expect(attempts).toHaveLength(2)
    expect(attempts[0].consumer).toBe(attempts[1].consumer)
    await manager.destroy()
  })

  test('disconnect: a failed release keeps the lease; the retry releases the same lease', async () => {
    const { native, manager, backend } = await openManager()
    const connection = await connectDefault(manager, backend)
    native.failNext('connection.disconnect', 'operation.timed-out', 'core', 'ubm-mobile.connection.disconnect')
    expect((await connection.release()).state).toBe('release-failed')
    expect(connection.isReleased()).toBe(false)
    expect((await connection.release()).state).toBe('released')
    const attempts = native.opsInvoked('connection.disconnect')
    expect(attempts).toHaveLength(2)
    expect(attempts[0].lease).toBe(attempts[1].lease)
    await manager.destroy()
  })
})
