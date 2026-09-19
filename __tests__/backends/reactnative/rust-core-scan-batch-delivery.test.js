// __tests__/backends/reactnative/rust-core-scan-batch-delivery.test.js
//
// Two physical regressions (Samsung Android, Rust-core path):
// - `manager.find` with a query rejected `stream.overflow` ~400 ms into the
//   scan. `find` reads a `latest` stream (one item, drop-oldest) and applies
//   the query in the public scan controller, so every advertisement reaches
//   that one slot. Records delivered in one synchronous loop filled the slot
//   before the reader could re-arm.
// - A `setTimeout(0)` boundary between records then held notifications ~26 s
//   with the screen off: React Native timers stop with the host (Android
//   Choreographer frames, iOS CADisplayLink).
// The legacy boundary delivered one native callback — one native→JS task —
// per record, and so does the drain: one record per native drain call, each
// resolved as its own task. Every test runs with JS timers paused (a host in
// the background) and drains resolved as native tasks. Driven through the
// ordinary factory, the REAL binding and codec, and the deterministic owner.

const {
  rustCoreHarness,
  environment,
  settle,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { RustCoreDrainRouter } = require('../../../src/backends/reactnative/react-native-rust-core-drain')

const HEART_RATE_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const BATTERY_SERVICE = '0000180f-0000-1000-8000-00805f9b34fb'
const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })
const LATEST = Object.freeze({
  itemCapacity: 1,
  byteCapacity: 4096,
  reservedControlCapacity: 2,
  overflowPolicy: 'drop-oldest'
})

async function openManager(platform = 'android') {
  const harness = rustCoreHarness({ platform, nativeOptions: { drainResolution: 'native-task' } })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  return { native: harness.native, manager, publicManager, backend: manager.attachedBackend.backend }
}

function onlySession(native) {
  const [session] = native.liveSessions()
  return session
}

/** Host tasks, not JS timers: the timers are paused. */
async function until(condition, what) {
  for (let turn = 0; turn < 2000; turn += 1) {
    if (condition()) return
    await new Promise(resolve => setImmediate(resolve))
  }
  throw new Error(`timed out waiting for ${what}`)
}

async function subscribeLatest(manager, backend) {
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await manager.connect(peerId, NO_OPTIONS)
  const database = await connection.discover(NO_OPTIONS)
  const path = (await database.snapshot()).characteristics[0].path
  return database.subscribe(path, subscribeOptions({ delivery: LATEST }))
}

beforeEach(() => {
  // A paused host: JS timers never fire (Android Choreographer and iOS
  // CADisplayLink stop with the screen off); host tasks still run.
  jest.useFakeTimers({ doNotFake: ['setImmediate', 'clearImmediate', 'nextTick', 'queueMicrotask'] })
})

afterEach(() => {
  jest.useRealTimers()
})

describe('queued records are delivered one per native task, with JS timers paused', () => {
  test.each(['android', 'apple'])(
    '%s: find with a query resolves to the matching peer when non-matching adverts share its batch',
    async platform => {
      const { native, manager, publicManager } = await openManager(platform)
      const session = onlySession(native)
      const found = publicManager.find({
        query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, names: { prefixes: ['Polar H10'] } }] }
      })
      await until(() => session.scans.size > 0, 'the scan to start')
      await settle(60)
      const other =
        platform === 'android'
          ? index => `A0:9E:1A:00:00:1${index}`
          : index => `C0FFEE00-0000-4000-8000-00000000001${index}`
      for (const index of [1, 2, 3]) {
        native.emitAdvertisement(other(index), { localName: `Other ${index}`, serviceUuids: [BATTERY_SERVICE] })
      }
      native.emitAdvertisement(platform === 'android' ? DEFAULT_PEER : other(9), {
        localName: 'Polar H10 X',
        serviceUuids: [HEART_RATE_SERVICE]
      })
      // Every record is queued before the one wake runs.
      expect(session.outbox.map(record => record.t)).toEqual(['adv', 'adv', 'adv', 'adv'])
      const peer = await found
      expect(peer.name).toBe('Polar H10 X')
      await publicManager.destroy()
      await manager.destroy()
    }
  )

  test('a latest notification subscription sees every queued value and no overflow', async () => {
    const { native, manager, backend } = await openManager()
    const subscription = await subscribeLatest(manager, backend)
    const items = []
    ;(async () => {
      for await (const item of subscription.values) items.push(item)
    })()
    await settle(60)
    for (const byte of [1, 2, 3, 4]) native.emitNotification(new Uint8Array([byte]))
    expect(onlySession(native).outbox.map(record => record.t)).toEqual(['value', 'value', 'value', 'value'])
    await until(() => items.length >= 4, 'four values')
    expect(items.map(item => item.kind)).toEqual(['value', 'value', 'value', 'value'])
    expect(items.map(item => item.value.value[0])).toEqual([1, 2, 3, 4])
    await subscription.remove()
    await manager.destroy()
  })

  test('control records keep their drained order around the task boundaries', async () => {
    const { native, manager, backend } = await openManager()
    const subscription = await subscribeLatest(manager, backend)
    const items = []
    ;(async () => {
      for await (const item of subscription.values) items.push(item)
    })()
    await settle(60)
    native.emitNotification(new Uint8Array([1]))
    native.emitNotification(new Uint8Array([2]))
    native.endConsumer(DEFAULT_PEER, 'closed')
    await until(() => items.some(item => item.kind === 'terminal'), 'the stream end')
    expect(items.map(item => (item.kind === 'value' ? item.value.value[0] : item.kind))).toEqual([1, 2, 'terminal'])
    await manager.destroy()
  })

  test('a link loss behind queued values still ends the subscription connection-lost (link and stream-end in one pass)', async () => {
    const { native, manager, backend } = await openManager()
    const subscription = await subscribeLatest(manager, backend)
    const items = []
    ;(async () => {
      for await (const item of subscription.values) items.push(item)
    })()
    await settle(60)
    native.emitNotification(new Uint8Array([1]))
    native.emitNotification(new Uint8Array([2]))
    native.dropLink(DEFAULT_PEER, 'peer')
    await until(() => items.some(item => item.kind === 'terminal'), 'the stream end')
    expect(items.map(item => (item.kind === 'value' ? item.value.value[0] : item.reason))).toEqual([
      1,
      2,
      'connection-lost'
    ])
    await manager.destroy()
  })

  test('a destroy racing queued values delivers an ordered prefix, reports the rest unmatched, never overflows', async () => {
    const { native, manager, backend } = await openManager()
    const warnings = []
    const events = backend.events()
    ;(async () => {
      for await (const item of events) {
        if (item.kind !== 'value') break
        if (item.value.kind === 'diagnostic-warning') warnings.push(item.value.code)
      }
    })()
    const subscription = await subscribeLatest(manager, backend)
    const items = []
    const reading = (async () => {
      for await (const item of subscription.values) items.push(item)
    })()
    await settle(60)
    for (const byte of [1, 2, 3, 4]) native.emitNotification(new Uint8Array([byte]))
    await until(() => items.length >= 1, 'the first value')
    await manager.destroy()
    await reading
    const delivered = items.filter(item => item.kind === 'value').map(item => item.value.value[0])
    expect(delivered).toEqual([1, 2, 3, 4].slice(0, delivered.length))
    // The deterministic owner discards its outbox at dispose; the Rust owner
    // keeps it, and the router's stop flush (below) drains it.
    expect(delivered.length + warnings.filter(code => code === 'unmatched-notification').length).toBeLessThanOrEqual(4)
    expect(items.some(item => item.kind === 'overflow')).toBe(false)
    expect(items[items.length - 1]).toMatchObject({ kind: 'terminal' })
  })

  test('an ECG-rate burst (130 frames of 229 bytes) is delivered in order, one native task per frame', async () => {
    const { native, manager, backend } = await openManager()
    const subscription = await subscribeLatest(manager, backend)
    const items = []
    ;(async () => {
      for await (const item of subscription.values) items.push(item)
    })()
    await settle(60)
    const mark = native.calls.length
    const frames = Array.from({ length: 130 }, (_, index) => Uint8Array.from({ length: 229 }, () => index))
    for (const frame of frames) native.emitNotification(frame)
    await until(() => items.length >= frames.length, 'every frame')
    expect(items.every(item => item.kind === 'value')).toBe(true)
    expect(items.map(item => item.value.value[0])).toEqual(frames.map(frame => frame[0]))
    expect(items.every(item => item.value.value.byteLength === 229)).toBe(true)
    const drains = native.callsSince(mark).filter(call => call[0] === 'drain')
    // One drain takes the burst; each later frame waits for a one-record drain
    // call, the native→JS task boundary (legacy: one native callback per record).
    expect(drains).toHaveLength(frames.length)
    expect(drains.map(call => call[2])).toEqual([256, ...Array(frames.length - 1).fill(1)])
    await subscription.remove()
    await manager.destroy()
  })
})

describe('the drain router at teardown', () => {
  /** An owner outbox that keeps its records after dispose, as the Rust owner does; drains resolve as host tasks. */
  function ownerOutbox(records) {
    const queue = [...records]
    const calls = []
    return {
      calls,
      session: {
        onWake: () => () => undefined,
        drain: async maxItems => {
          calls.push(maxItems)
          const taken = queue.splice(0, maxItems)
          await new Promise(resolve => setImmediate(resolve))
          return { more: queue.length > 0, records: taken }
        }
      }
    }
  }

  test('stop drains every record the owner still holds, in order, then ends', async () => {
    const records = [
      { t: 'value', ordinal: 0 },
      { t: 'value', ordinal: 1 },
      { t: 'value', ordinal: 2 },
      { t: 'stream-end', ordinal: 3 },
      { t: 'adv', ordinal: 4 }
    ]
    const { session, calls } = ownerOutbox(records)
    const delivered = []
    const router = new RustCoreDrainRouter(session, {
      deliver: record => delivered.push(record.ordinal),
      failed: error => {
        throw error
      }
    })
    router.start()
    await until(() => delivered.length >= 2, 'the first records')
    await router.stop()
    expect(delivered).toEqual([0, 1, 2, 3, 4])
    const drainsAfterStop = calls.length
    router.wake()
    await until(() => true, 'nothing')
    expect(calls).toHaveLength(drainsAfterStop)
  })

  test('stop on an idle router flushes what raced the last wake', async () => {
    const { session } = ownerOutbox([{ t: 'value', ordinal: 7 }])
    const delivered = []
    const router = new RustCoreDrainRouter(
      { ...session, onWake: () => () => undefined },
      { deliver: record => delivered.push(record.ordinal), failed: () => undefined }
    )
    await router.stop()
    expect(delivered).toEqual([7])
  })

  test('a throwing sink ends the router, reports failed exactly once, and leaves no unhandled rejection', async () => {
    const { session } = ownerOutbox([
      { t: 'value', ordinal: 0 },
      { t: 'value', ordinal: 1 }
    ])
    const failures = []
    const rejections = []
    const onUnhandled = reason => rejections.push(reason)
    process.on('unhandledRejection', onUnhandled)
    try {
      const router = new RustCoreDrainRouter(session, {
        deliver: () => {
          throw new Error('boom-delivery')
        },
        failed: error => {
          failures.push(error)
        }
      })
      router.start()
      await until(() => failures.length > 0, 'the sink failure report')
      // The wake path owns no await: flushing host tasks must not surface
      // the delivery defect as an unhandled rejection.
      for (let turn = 0; turn < 20; turn += 1) {
        await new Promise(resolve => setImmediate(resolve))
      }
      expect(rejections).toEqual([])
      await router.stop()
      expect(failures).toHaveLength(1)
      expect(failures[0].message).toBe('boom-delivery')
      for (let turn = 0; turn < 20; turn += 1) {
        await new Promise(resolve => setImmediate(resolve))
      }
      expect(rejections).toEqual([])
    } finally {
      process.removeListener('unhandledRejection', onUnhandled)
    }
  })
})
