// __tests__/ReactNativeRustCoreProvider.test.js
//
// F01 / R07–R13: the binding-backed provider on its own (no manager), over
// the production binding and a deterministic `UnifiedBleRustCore` module that
// speaks the frozen wire. Every radio effect is an owner op; no TypeScript
// scheduling, fallback or synthesized record exists on this path.

const {
  createReactNativeRustCoreBackendProvider
} = require('../src/backends/reactnative/react-native-rust-core-provider')
const { reactNativeAndroidDefaultAdapterId } = require('../src/backends/reactnative/react-native-platform-identity')
const {
  rustCoreHarness,
  scanOptions,
  subscribeOptions,
  settle
} = require('../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../test-support/react-native/deterministic-rust-core-native')
const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

function coreCompatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function providerFor(harness, overrides = {}) {
  return createReactNativeRustCoreBackendProvider({
    platform: 'android',
    binding: harness.binding,
    owner: 'rn-provider-test',
    now: () => 1000,
    runtime: { androidApiLevel: 34 },
    ...overrides
  })
}

async function openBackend(options = {}) {
  const harness = rustCoreHarness({ platform: 'android', ...options })
  const backend = await providerFor(harness).create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
  return { harness, native: harness.native, backend }
}

let correlationOrdinal = 0

function operation(backend, overrides = {}) {
  correlationOrdinal += 1
  return {
    signal: null,
    deadline: null,
    correlation: opaqueId(`corr-${correlationOrdinal}`, 'core-operation', 'test'),
    ...overrides
  }
}

async function connected(backend) {
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const lease = await backend.connections.connect(peerId, opaqueId('client', 'client', 'test'), NO_OPTIONS)
  const database = await backend.gatt.discover(lease.connection, NO_OPTIONS)
  const snapshot = await database.snapshot()
  return { lease, database, snapshot, path: snapshot.characteristics[0].path }
}

async function rejection(promise) {
  return promise.then(
    () => null,
    error => error
  )
}

describe('React Native Rust core provider (F01 factory routing)', () => {
  test('creation admits a session, reads adapter state and counters, and destroy disposes then closes', async () => {
    const { native, backend } = await openBackend()
    expect(native.opsInvoked('adapter.state')).toHaveLength(1)
    expect(native.opsInvoked('counters.describe')).toHaveLength(1)
    await backend.attach({ coreCompatibility: coreCompatibility() })
    expect(await backend.destroy()).toEqual({ state: 'released', failures: [] })
    const names = native.calls.map(call => (call[0] === 'invoke' ? call[2] : call[0]))
    expect(names.indexOf('session.dispose')).toBeLessThan(names.indexOf('closeSession'))
  })

  test('a foreign build fails creation closed with zero sessions, never a TS fallback', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    harness.native.identity = { ...harness.native.identity, contractRevision: 'C-UBM.9.9.9-DRAFT' }
    const error = await rejection(
      providerFor(harness).create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    )
    expect(error.normalized).toMatchObject({ code: 'protocol.incompatible' })
    expect(harness.native.calls.filter(call => call[0] === 'openSession')).toHaveLength(0)
  })

  test('scan routes through the owner; the observation carries the platform fields', async () => {
    const { native, backend } = await openBackend()
    const lease = await backend.scanner.start(scanOptions({ deadline: 1500 }), opaqueId('client', 'client', 'test'))
    expect(native.opsInvoked('scan.start')[0]).toMatchObject({
      serviceUuids: [],
      duplicatePolicy: 'all',
      budgetMs: 500,
      lifetimeMs: 500
    })
    native.emitAdvertisement(DEFAULT_PEER, { rssi: -60, localName: 'Movesense' })
    const item = await lease.observations[Symbol.asyncIterator]().next()
    expect(item.value.value.rssi).toMatchObject({ state: 'present', value: -60 })
    expect(item.value.value.localName).toMatchObject({ state: 'present', value: 'Movesense' })
    await lease.stop()
    expect(native.opsInvoked('scan.stop')).toEqual([{ operationId: 's1-scan-1' }])
    await backend.destroy()
  })

  test('native expiry ends owner and shared consumers while JS timers are suspended', async () => {
    jest.useFakeTimers({ doNotFake: ['setImmediate', 'clearImmediate', 'nextTick', 'queueMicrotask'] })
    try {
      const { native, backend } = await openBackend()
      const owner = await backend.scanner.start(
        scanOptions({ deadline: 1500, sharing: { mode: 'owner', allowSharing: true } }),
        opaqueId('client', 'client', 'test')
      )
      const shared = await backend.scanner.join(owner.leaseId, owner.shareToken, opaqueId('other', 'client', 'test'))
      native.endScans('operation-timed-out')
      await settle(80)
      native.emitAdvertisement(DEFAULT_PEER)
      for (const lease of [owner, shared]) {
        const item = await lease.observations[Symbol.asyncIterator]().next()
        expect(item.value).toMatchObject({ kind: 'terminal', reason: 'operation-timed-out' })
        expect(await lease.stop()).toEqual({ state: 'released', failures: [] })
      }
      expect(native.opsInvoked('scan.stop')).toHaveLength(0)
      await backend.destroy()
    } finally {
      jest.useRealTimers()
    }
  })

  test('replays an exact native expiry drained before scan.start returns its membership', async () => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.endScans('operation-timed-out')
        await settle(80)
      }
      return result
    })
    const owner = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    let item
    const pending = owner.observations[Symbol.asyncIterator]()
      .next()
      .then(value => {
        item = value
      })
    await settle(80)
    expect(item?.value).toMatchObject({ kind: 'terminal', reason: 'operation-timed-out' })
    await pending
    expect(await owner.stop()).toEqual({ state: 'released', failures: [] })
    expect(native.opsInvoked('scan.stop')).toHaveLength(0)
    await backend.destroy()
  })

  test('retains an authentic advertisement drained before its scan.start response settles', async () => {
    const { native, backend } = await openBackend()
    let releaseStart
    const startGate = new Promise(resolve => {
      releaseStart = resolve
    })
    let observedEarly
    const earlyObservation = new Promise(resolve => {
      observedEarly = resolve
    })
    const onAdvertisement = backend.onAdvertisement.bind(backend)
    jest.spyOn(backend, 'onAdvertisement').mockImplementation(record => {
      onAdvertisement(record)
      observedEarly(record)
    })
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.emitAdvertisement(DEFAULT_PEER, { localName: 'authentic early observation' })
        await startGate
      }
      return result
    })
    const starting = backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    try {
      expect(await earlyObservation).toMatchObject({ operationId: 's1-scan-1' })
      expect(backend.retainedJsBytes()).toBeGreaterThan(0)
      releaseStart()
      const scan = await starting
      const first = scan.observations[Symbol.asyncIterator]().next()
      await scan.stop()
      expect((await first).value).toMatchObject({
        kind: 'value',
        value: { localName: { value: 'authentic early observation' } }
      })
    } finally {
      releaseStart()
      await backend.destroy()
    }
  })

  test.each([
    ['drop-oldest', 'second', 1, 0],
    ['drop-newest', 'first', 1, 0],
    ['latest', 'second', 0, 1],
    ['error', null, 1, 0]
  ])('early advertisements use the actual %s consumer budget', async (policy, name, dropped, replaced) => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.emitAdvertisement(DEFAULT_PEER, { localName: 'first' })
        native.emitAdvertisement(DEFAULT_PEER, { localName: 'second' })
        await settle(80)
      }
      return result
    })
    try {
      const scan = await backend.scanner.start(
        scanOptions({
          delivery: { itemCapacity: 1, byteCapacity: 65536, reservedControlCapacity: 4, overflowPolicy: policy }
        }),
        opaqueId('client', 'client', 'test')
      )
      const iterator = scan.observations[Symbol.asyncIterator]()
      const items = Promise.all(name === null ? [iterator.next()] : [iterator.next(), iterator.next()])
      await scan.stop()
      const values = (await items).map(item => item.value)
      expect(values[0]).toMatchObject({
        kind: name === null ? 'terminal' : 'overflow',
        droppedItems: dropped,
        replacedItems: replaced
      })
      if (name !== null) expect(values[1]).toMatchObject({ kind: 'value', value: { localName: { value: name } } })
      else expect(values[0].reason).toBe('overflow')
    } finally {
      await backend.destroy()
    }
  })

  test('stale request advertisements cannot occupy the provisional consumer budget', async () => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.emitAdvertisement(DEFAULT_PEER, { startOperationId: 'old-request', localName: 'stale' })
        native.emitAdvertisement(DEFAULT_PEER, { localName: 'current' })
        await settle(80)
      }
      return result
    })
    try {
      const scan = await backend.scanner.start(
        scanOptions({
          delivery: { itemCapacity: 1, byteCapacity: 65536, reservedControlCapacity: 4, overflowPolicy: 'error' }
        }),
        opaqueId('client', 'client', 'test')
      )
      const first = scan.observations[Symbol.asyncIterator]().next()
      await scan.stop()
      expect((await first).value).toMatchObject({ kind: 'value', value: { localName: { value: 'current' } } })
    } finally {
      await backend.destroy()
    }
  })

  test('native ingress loss during provisional admission reaches the same consumer accounting', async () => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.push([...native.sessions.values()][0], { t: 'ingress-drop', class: 'advertisement', count: 3 })
        native.emitAdvertisement(DEFAULT_PEER, { localName: 'kept' })
        await settle(80)
      }
      return result
    })
    try {
      const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
      const iterator = scan.observations[Symbol.asyncIterator]()
      const items = Promise.all([iterator.next(), iterator.next()])
      await scan.stop()
      const values = (await items).map(item => item.value)
      expect(values[0]).toMatchObject({ kind: 'overflow', droppedItems: 3 })
      expect(values[1]).toMatchObject({ kind: 'value', value: { localName: { value: 'kept' } } })
    } finally {
      await backend.destroy()
    }
  })

  test('early membership mismatch fails closed and retains refused cleanup of the returned membership', async () => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        native.emitAdvertisement(DEFAULT_PEER, { operationId: 'wrong-native-membership' })
        await settle(80)
        native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
      }
      return result
    })
    await expect(backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))).rejects.toMatchObject({
      normalized: { code: 'protocol.violation' }
    })
    expect(native.opsInvoked('scan.stop')).toEqual([{ operationId: 's1-scan-1' }])
    expect([...native.sessions.values()][0].scans.size).toBe(1)
    expect(await backend.destroy()).toEqual({ state: 'released', failures: [] })
  })

  test('bounds provisional scan admission and admits a new start after its refusal', async () => {
    const { native, backend } = await openBackend()
    const client = opaqueId('client', 'client', 'test')
    native.hold('scan.start')
    const first = backend.scanner.start(scanOptions(), client)
    await settle(40)
    await expect(backend.scanner.start(scanOptions(), client)).rejects.toMatchObject({
      normalized: { code: 'scan.already-active' }
    })
    expect(native.opsInvoked('scan.start')).toHaveLength(1)
    native.release('scan.start')
    const scan = await first
    expect(native.opsInvoked('scan.start')[0]).not.toHaveProperty('lifetimeMs')
    await scan.stop()
    const next = await backend.scanner.start(scanOptions({ deadline: 1000 + 2147483648 }), client)
    expect(native.opsInvoked('scan.start')[1]).toMatchObject({ budgetMs: 2147483647, lifetimeMs: 2147483647 })
    await next.stop()
    await backend.destroy()
  })

  test('drain failure is not native release proof for an in-flight scan stop', async () => {
    const { native, backend } = await openBackend()
    const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    native.hold('scan.stop')
    const stopping = scan.stop()
    await settle(40)
    backend.drainFailed(new Error('drain parsing failed'))
    native.release('scan.stop', {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'scan',
          code: 'platform.failure',
          domain: 'platform',
          operation: 'ubm-mobile.scan.stop',
          detail: 'refused held stop',
          platform: null
        }
      ]
    })
    const receipt = await stopping
    const retained = [...native.sessions.values()][0].scans.size
    await backend.destroy()
    expect(receipt).toMatchObject({ state: 'release-failed', failures: [{ error: { code: 'platform.failure' } }] })
    expect(retained).toBe(1)
  })

  test('scan delivery failure alone never skips its later native cleanup attempt', async () => {
    const { native, backend } = await openBackend()
    const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    backend.drainFailed(new Error('drain parsing failed'))
    native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    const receipt = await scan.stop()
    const calls = native.opsInvoked('scan.stop').length
    await backend.destroy()
    expect(receipt).toMatchObject({ state: 'release-failed', failures: [{ error: { code: 'platform.failure' } }] })
    expect(calls).toBe(1)
  })

  test('old queued advertisements and scan-end cannot leak into a replacement membership', async () => {
    const { native, backend } = await openBackend()
    const client = opaqueId('client', 'client', 'test')
    const old = await backend.scanner.start(scanOptions(), client)
    const drain = native.drain.bind(native)
    let release
    let held = false
    const gate = new Promise(resolve => {
      release = resolve
    })
    jest.spyOn(native, 'drain').mockImplementation(async (...args) => {
      const result = await drain(...args)
      if (!held && JSON.parse(result).records.some(record => record.t === 'adv')) {
        held = true
        await gate
      }
      return result
    })
    native.emitAdvertisement(DEFAULT_PEER, { localName: 'old membership value' })
    await settle(80)
    expect(held).toBe(true)
    native.endScans('operation-timed-out')
    const replacement = await backend.scanner.start(scanOptions(), client)
    release()
    native.emitAdvertisement(DEFAULT_PEER, { localName: 'replacement membership value' })
    const item = await replacement.observations[Symbol.asyncIterator]().next()
    expect(item.value.value.localName.value).toBe('replacement membership value')
    await old.stop()
    await replacement.stop()
    await backend.destroy()
  })

  test('ambiguous early terminals fail closed and keep refused cleanup reachable', async () => {
    const { native, backend } = await openBackend()
    const invoke = native.invoke.bind(native)
    jest.spyOn(native, 'invoke').mockImplementation(async (...args) => {
      const result = await invoke(...args)
      if (args[1] === 'scan.start') {
        const session = [...native.sessions.values()][0]
        native.push(session, { t: 'scan-end', operationId: 'foreign-1', reason: 'operation-timed-out' })
        native.push(session, { t: 'scan-end', operationId: 'foreign-2', reason: 'operation-timed-out' })
        await settle(80)
        native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
      }
      return result
    })
    await expect(backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))).rejects.toMatchObject({
      normalized: { code: 'protocol.violation' }
    })
    expect([...native.sessions.values()][0].scans.size).toBe(1)
    expect(await backend.destroy()).toEqual({ state: 'released', failures: [] })
    expect([...native.sessions.values()][0].scans.size).toBe(0)
  })

  test('connect/discover/read/write/subscribe/unsubscribe/dispose execute the owner with one lease', async () => {
    const { native, backend } = await openBackend()
    const { lease, snapshot, path } = await connected(backend)
    const connectLease = native.opsInvoked('connection.connect')[0].lease
    expect(native.opsInvoked('gatt.discover')[0].lease).toBe(connectLease)
    expect(lease.connection.state).toBe('connected')
    // The heart-rate service, then the owner double's duplicate-UUID battery services.
    expect(snapshot.services).toHaveLength(3)
    expect(snapshot.characteristics).toHaveLength(4)
    expect(snapshot.descriptors).toHaveLength(3)
    const read = await backend.gatt.read(path, { operation: operation(backend) }).completion
    expect([...read.value]).toEqual([0x00, 0x48])
    const write = await backend.gatt.write(path, {
      operation: operation(backend),
      bytes: new Uint8Array([1]),
      mode: 'with-response'
    }).completion
    expect(write.commitState).toBe('confirmed')
    const subscription = await backend.gatt.subscribe(path, {
      operation: operation(backend),
      options: subscribeOptions()
    }).completion
    await backend.gatt.unsubscribe(subscription, operation(backend)).completion
    expect(subscription.notifications.isTerminal()).toBe(true)
    await lease.release()
    await backend.destroy()
  })

  test('a destroyed backend refuses loudly without touching the owner', async () => {
    const { native, backend } = await openBackend()
    await backend.destroy()
    const after = native.calls.length
    const error = await rejection(backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test')))
    expect(error.normalized.code).toBe('lifecycle.destroyed')
    expect(native.calls.length).toBe(after)
  })
})

describe('React Native Rust core provider lifecycle (R07–R13)', () => {
  test('R07: a mid-flight abort cancels by the operationId the op was invoked with', async () => {
    const { native, backend } = await openBackend()
    const { path } = await connected(backend)
    native.hold('gatt.read')
    const controller = new AbortController()
    const op = operation(backend, { signal: controller.signal })
    const dispatch = backend.gatt.read(path, { operation: op })
    await settle()
    controller.abort()
    expect((await rejection(dispatch.completion)).normalized.code).toBe('operation.aborted')
    // The wire operation id is the owner's internal name for this invoke; the
    // caller's correlation stays public (legacy `operation-{n}`) and never
    // crosses the wire. The cancel names exactly the invoked operation.
    const invoked = native.opsInvoked('gatt.read')[0]
    expect(invoked.operationId).not.toBe(String(op.correlation))
    expect(native.opsInvoked('op.cancel')).toEqual([{ operationId: invoked.operationId, admission: invoked.admission }])
    await backend.destroy()
  })

  test('R07: dispatch cancellation reports the owner’s acknowledgement', async () => {
    const { native, backend } = await openBackend()
    const { path } = await connected(backend)
    native.hold('gatt.read')
    const dispatch = backend.gatt.read(path, { operation: operation(backend) })
    await settle()
    expect((await dispatch.requestCancellation()).state).toBe('cancellation-requested')
    await rejection(dispatch.completion)
    expect((await dispatch.requestCancellation()).state).toBe('already-terminal')
    await backend.destroy()
  })

  test('R07: aborting after settle sends no spurious op.cancel (no leaked listener)', async () => {
    const { native, backend } = await openBackend()
    const { path } = await connected(backend)
    const controller = new AbortController()
    await backend.gatt.read(path, { operation: operation(backend, { signal: controller.signal }) }).completion
    controller.abort()
    await settle()
    expect(native.opsInvoked('op.cancel')).toHaveLength(0)
    await backend.destroy()
  })

  test('R08: listAdapters probes and releases the probe session', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    const adapters = await providerFor(harness).listAdapters()
    expect(adapters).toHaveLength(1)
    expect(harness.native.opsInvoked('session.dispose')).toHaveLength(1)
    expect(harness.native.calls.filter(call => call[0] === 'closeSession')).toHaveLength(1)
  })

  test('R08/R11: destroy retires every owned stream (scans, notifications, adapter watch, events)', async () => {
    const { backend } = await openBackend()
    const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    const { path } = await connected(backend)
    const subscription = await backend.gatt.subscribe(path, {
      operation: operation(backend),
      options: subscribeOptions()
    }).completion
    const watch = await backend.adapter.watchState()
    const events = backend.events()
    await backend.destroy()
    expect(scan.observations.isTerminal()).toBe(true)
    expect(subscription.notifications.isTerminal()).toBe(true)
    expect(watch.transitions.isTerminal()).toBe(true)
    expect(events.isTerminal()).toBe(true)
  })

  test('R12 (strict wire): a malformed drain record fails delivery loudly instead of being skipped', async () => {
    const { native, backend } = await openBackend()
    const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    native.emitAdvertisement(DEFAULT_PEER, { rssi: 1.5 })
    await settle(60)
    expect((await scan.observations[Symbol.asyncIterator]().next()).value).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed'
    })
    await backend.destroy()
  })

  test('R13: discover on a released connection fails stale before touching the owner', async () => {
    const { native, backend } = await openBackend()
    const { lease } = await connected(backend)
    await lease.release()
    const before = native.opsInvoked('gatt.discover').length
    const error = await rejection(backend.gatt.discover(lease.connection, NO_OPTIONS))
    expect(error.normalized.code).toBe('connection.stale')
    expect(native.opsInvoked('gatt.discover')).toHaveLength(before)
    await backend.destroy()
  })

  test('R13: out-of-range bytes from the owner fail malformed instead of wrapping', async () => {
    const { native, backend } = await openBackend()
    const { path } = await connected(backend)
    native.hold('gatt.read')
    const dispatch = backend.gatt.read(path, { operation: operation(backend) })
    await settle()
    native.release('gatt.read', { valueB64: 'AQ==', extra: true })
    expect((await rejection(dispatch.completion)).normalized.code).toBe('protocol.malformed')
    await backend.destroy()
  })

  test('R13: peerFromAddress rejects malformed addresses and missing address types', async () => {
    const { native, backend } = await openBackend()
    expect(() => backend.connections.peerFromAddress({ address: 'not-an-address', addressType: 'public' })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
    )
    expect(() => backend.connections.peerFromAddress({ address: 'aa:bb:cc:dd:ee:ff' })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
    )
    const peerId = backend.connections.peerFromAddress({ address: 'a0:9e:1a:00:00:01', addressType: 'random' })
    const lease = await backend.connections.connect(peerId, opaqueId('client', 'client', 'test'), NO_OPTIONS)
    expect(native.opsInvoked('connection.connect')[0]).toMatchObject({ peerId: DEFAULT_PEER })
    await lease.release()
    await backend.destroy()
  })

  test('R10: database snapshots are isolated from later owner-record mutation', async () => {
    const { native, backend } = await openBackend()
    const { database, snapshot } = await connected(backend)
    native.peripherals.get(DEFAULT_PEER).services[0].characteristics.push({
      uuid: '00002a38-0000-1000-8000-00805f9b34fb',
      occurrence: 0,
      properties: 0x02,
      value: new Uint8Array(0),
      descriptors: []
    })
    expect(await database.snapshot()).toEqual(snapshot)
    await backend.destroy()
  })

  test('R10: a dispose failure is reported as release-failed, never as released; a retry disposes again', async () => {
    const { native, backend } = await openBackend()
    native.failNext(
      'session.dispose',
      'platform.failure',
      'platform',
      'ubm-mobile.session.dispose',
      'radio close failed'
    )
    const first = await backend.destroy()
    expect(first).toMatchObject({
      state: 'release-failed',
      failures: [{ resourceKind: 'session', error: { code: 'platform.failure' } }]
    })
    expect(await backend.destroy()).toEqual({ state: 'released', failures: [] })
    expect(native.opsInvoked('session.dispose')).toHaveLength(2)
  })

  test('R08: in-flight operations settle their caller when destroy disposes the session', async () => {
    const { native, backend } = await openBackend()
    const { path } = await connected(backend)
    native.hold('gatt.read')
    const dispatch = backend.gatt.read(path, { operation: operation(backend) })
    await settle()
    await backend.destroy()
    expect((await rejection(dispatch.completion)).normalized.code).toBe('operation.aborted')
  })
})

describe('PR210-76 the trace seam is the public CoreTraceSink interface', () => {
  test('a caller-supplied sink object (not the internal recorder class) receives dispatch and outcome records', async () => {
    const records = []
    const harness = rustCoreHarness({ platform: 'android' })
    const backend = await providerFor(harness, { trace: { record: input => records.push(input) } }).create({
      selectedAdapterId: reactNativeAndroidDefaultAdapterId()
    })
    const { lease } = await connected(backend)
    await lease.release()
    expect(records.length).toBeGreaterThan(0)
    expect(records.map(record => record.transition)).toEqual(expect.arrayContaining(['dispatched', 'succeeded']))
    expect(records.every(record => typeof record.operation === 'string')).toBe(true)
    await backend.destroy()
  })
})
