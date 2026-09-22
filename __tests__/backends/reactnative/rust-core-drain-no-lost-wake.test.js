// __tests__/backends/reactnative/rust-core-drain-no-lost-wake.test.js
//
// G5/W8: the React Native drain pump must never lose a wake. The W6
// throughput benchmark stalled outside jest ("the RN harness wake-to-drain
// stalls outside jest"); triage showed a harness module-resolution artifact,
// not a pump race — the wake-to-drain chain delivers 200/200 in plain node
// once `react-native` is stubbed (the f01 precedent) and src/*.ts resolves
// from lib/. These tests pin the single answer on the real production
// binding, router and deterministic owner: every armed arrival wakes exactly
// once, every wake is drained, and the drain keeps its contract (one native
// boundary per data record after the first in a burst).
//
// Deterministic: synchronous interleavings plus bounded host-task flushes
// that fail loudly. No timers of any kind, no fake timers (a paused host is
// the point: wakes and drains owe nothing to JS timers).

'use strict'

const {
  RustCoreDrainRouter,
  DRAIN_MAX_ITEMS,
  DRAIN_BOUNDARY_ITEMS
} = require('../../../src/backends/reactnative/react-native-rust-core-drain')
const {
  createReactNativeRustCoreBinding
} = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const {
  DeterministicRustCoreNative
} = require('../../../test-support/react-native/deterministic-rust-core-native')
const {
  rustCoreHarness,
  environment,
  scanOptions,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

/** Host tasks, never JS timers: every wait below is bounded and loud. */
async function flushTasks(turns = 30) {
  for (let turn = 0; turn < turns; turn += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

async function untilTrue(condition, what, turns = 500) {
  for (let turn = 0; turn < turns; turn += 1) {
    if (condition()) return
    await new Promise(resolve => setImmediate(resolve))
  }
  throw new Error(`no-lost-wake: timed out waiting for ${what}`)
}

function valueRecord(ordinal, consumer = 'c0') {
  return { t: 'value', ordinal, consumer, valueB64: 'AA==', delivery: 'notification', controlLost: 0 }
}

function microtaskFlush(turns = 20) {
  const chain = []
  for (let turn = 0; turn < turns; turn += 1) chain.push(Promise.resolve())
  return chain.reduce((previous, next) => previous.then(() => next), Promise.resolve())
}

/** A session whose drain answers are stepped by hand: no timing luck. */
function controlledSession() {
  const calls = []
  const pending = []
  const answers = []
  const session = {
    drain: (maxItems, maxBytes) => {
      calls.push([maxItems, maxBytes])
      if (answers.length > 0) return Promise.resolve(answers.shift())
      return new Promise(resolve => pending.push(resolve))
    },
    onWake: () => () => undefined
  }
  return {
    session,
    calls,
    answerNext(batch) {
      const resolve = pending.shift()
      if (resolve === undefined) throw new Error('no-lost-wake: no drain in flight to answer')
      resolve(batch)
    },
    queueAnswer(batch) {
      answers.push(batch)
    },
    inFlight() {
      return pending.length
    }
  }
}

function emptyBatch() {
  return { more: false, records: [], controlLost: 0 }
}

/** Stops the router against a stub session: the stop flush always drains, so a final empty answer is queued first. */
async function stopDrained(router, controlled) {
  controlled.queueAnswer(emptyBatch())
  await router.stop()
}

describe('the deterministic owner wakes every armed arrival (arm, empty, record in between)', () => {
  test('a record arriving during an in-flight drain wakes exactly once and drains next', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android', drainResolution: 'native-task' })
    const admission = JSON.parse(await native.openSession('owner-a', 'ubm-mobile-wire/1'))
    const sessionId = String(admission.sessionId)
    const session = native.sessions.get(sessionId)
    native.push(session, { t: 'value', consumer: 'c0', valueB64: 'AA==', delivery: 'notification' })
    await flushTasks()
    let wakes = 0
    native.onSessionWake(() => {
      wakes += 1
    })
    // The call runs the synchronous splice+arm prefix, then parks on the
    // native-task await. The push below runs before any parked continuation,
    // so the record deterministically arrives inside the window.
    const first = native.drain(sessionId, 256, 65536)
    native.push(session, { t: 'value', consumer: 'c0', valueB64: 'AQ==', delivery: 'notification' })
    const firstBatch = JSON.parse(await first)
    expect(firstBatch.records).toHaveLength(1)
    // The first drain answered before the arrival (stale `more: false`): the
    // wake is what keeps the record from stranding.
    expect(firstBatch.more).toBe(false)
    await flushTasks()
    expect(wakes).toBe(1)
    const secondBatch = JSON.parse(await native.drain(sessionId, 256, 65536))
    expect(secondBatch.records.map(record => record.ordinal)).toEqual([2])
    expect(secondBatch.more).toBe(false)
    await native.closeSession(sessionId)
  })

  test('an empty drain arms; a later push disarms and wakes', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const admission = JSON.parse(await native.openSession('owner-b', 'ubm-mobile-wire/1'))
    const sessionId = String(admission.sessionId)
    const session = native.sessions.get(sessionId)
    const drained = JSON.parse(await native.drain(sessionId, 256, 65536))
    expect(drained).toMatchObject({ more: false, records: [] })
    expect(session.armed).toBe(true)
    let wakes = 0
    native.onSessionWake(() => {
      wakes += 1
    })
    native.push(session, { t: 'value', consumer: 'c0', valueB64: 'AA==', delivery: 'notification' })
    expect(session.armed).toBe(false)
    await microtaskFlush()
    expect(wakes).toBe(1)
    await native.closeSession(sessionId)
  })

  test('a synchronous burst wakes once; every record drains in ordinal order', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const admission = JSON.parse(await native.openSession('owner-c', 'ubm-mobile-wire/1'))
    const sessionId = String(admission.sessionId)
    const session = native.sessions.get(sessionId)
    let wakes = 0
    native.onSessionWake(() => {
      wakes += 1
    })
    const COUNT = 25
    for (let index = 0; index < COUNT; index += 1) {
      native.push(session, { t: 'value', consumer: 'c0', valueB64: 'AA==', delivery: 'notification' })
    }
    await microtaskFlush()
    expect(wakes).toBe(1)
    const ordinals = []
    for (;;) {
      const batch = JSON.parse(await native.drain(sessionId, 256, 65536))
      for (const record of batch.records) ordinals.push(record.ordinal)
      if (!batch.more) break
      if (ordinals.length > COUNT) throw new Error('no-lost-wake: drain never quiesced')
    }
    expect(ordinals).toEqual(Array.from({ length: COUNT }, (_, index) => index + 1))
    await native.closeSession(sessionId)
  })
})

describe('the drain router never strands a woken record', () => {
  test('a wake during an in-flight drain plus the owner per-arrival wake delivers', async () => {
    const controlled = controlledSession()
    const delivered = []
    const router = new RustCoreDrainRouter(controlled.session, {
      deliver: record => delivered.push(record.ordinal),
      failed: error => {
        throw error
      }
    })
    router.start()
    await untilTrue(() => controlled.inFlight() === 1, 'the first drain')
    // The arrival's own wake, coalesced behind the running drain.
    router.wake()
    // The in-flight drain answers stale-empty: nothing new arrived yet. The
    // scheduled pass re-drives the loop once (`let more = true` per pass)
    // and finds nothing: one scheduled call per wake, then quiescence.
    controlled.answerNext(emptyBatch())
    await untilTrue(() => controlled.calls.length === 2, 'the scheduled second pass')
    await flushTasks()
    expect(controlled.calls).toHaveLength(2)
    expect(delivered).toEqual([])
    // The owner's per-arrival wake for the record behind the stale answer
    // re-arms the loop; the pending pass takes it.
    router.wake()
    await untilTrue(() => controlled.inFlight() === 1, 'the pass behind the second wake')
    controlled.answerNext({ more: false, records: [valueRecord(9)], controlLost: 0 })
    await untilTrue(() => delivered.length === 1, 'the record behind the stale drain')
    expect(delivered).toEqual([9])
    await flushTasks()
    // Quiesced: no wake, no call — the pump never spins.
    expect(controlled.calls).toHaveLength(3)
    await untilTrue(() => controlled.inFlight() === 1, 'the quiescent pass drain')
    controlled.answerNext(emptyBatch())
    await flushTasks()
    await stopDrained(router, controlled)
  })

  test('a record arriving after the router idles wakes a new run', async () => {
    const controlled = controlledSession()
    const delivered = []
    const router = new RustCoreDrainRouter(controlled.session, {
      deliver: record => delivered.push(record.ordinal),
      failed: error => {
        throw error
      }
    })
    router.start()
    await untilTrue(() => controlled.inFlight() === 1, 'the first drain')
    controlled.answerNext(emptyBatch())
    await untilTrue(() => controlled.calls.length === 1 && delivered.length === 0, 'the idle router')
    await flushTasks()
    expect(controlled.calls).toHaveLength(1)
    controlled.queueAnswer({ more: false, records: [valueRecord(4)], controlLost: 0 })
    router.wake()
    await untilTrue(() => delivered.length === 1, 'the post-idle record')
    expect(delivered).toEqual([4])
    await stopDrained(router, controlled)
  })

  test('one wake drains a burst in order: 256 first, then one record per native call', async () => {
    const controlled = controlledSession()
    const delivered = []
    const router = new RustCoreDrainRouter(controlled.session, {
      deliver: record => delivered.push(record.ordinal),
      failed: error => {
        throw error
      }
    })
    controlled.queueAnswer({ more: true, records: [valueRecord(0), valueRecord(1)], controlLost: 0 })
    controlled.queueAnswer({ more: true, records: [valueRecord(2)], controlLost: 0 })
    controlled.queueAnswer(emptyBatch())
    router.start()
    await untilTrue(() => delivered.length === 3, 'the whole burst')
    expect(delivered).toEqual([0, 1, 2])
    // The design contract (docs/MOBILE_RUST_WIRE.md Delivery): the first
    // call takes the burst; while a backlog remains each call is the task
    // boundary and takes one record — the same count as legacy's one native
    // event per record.
    expect(controlled.calls.map(call => call[0])).toEqual([
      DRAIN_MAX_ITEMS,
      DRAIN_BOUNDARY_ITEMS,
      DRAIN_BOUNDARY_ITEMS
    ])
    expect(DRAIN_MAX_ITEMS).toBe(256)
    expect(DRAIN_BOUNDARY_ITEMS).toBe(1)
    await stopDrained(router, controlled)
  })
})

describe('the binding keeps a wake that arrives before its listener', () => {
  test('a pre-listen wake fires the listener on listen, then the live path wakes per arrival', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
    const session = await binding.openSession('owner-d')
    await session.invoke('scan.start', {
      serviceUuids: [],
      duplicatePolicy: 'all',
      operationId: 'scan-1',
      admission: 1
    })
    native.emitAdvertisement()
    await microtaskFlush()
    let fired = 0
    const remove = session.onWake(() => {
      fired += 1
    })
    // The wake arrived before the listener: kept, then delivered on listen.
    expect(fired).toBe(1)
    // Draining re-arms the empty outbox, so the next arrival wakes again
    // through the live path (an arrival behind no drain wakes nobody — the
    // outbox is still disarmed — which is exactly the coalescing above).
    await session.drain(256, 65536)
    native.emitAdvertisement()
    await microtaskFlush()
    expect(fired).toBe(2)
    remove()
    await session.close()
  })
})

describe('concurrent notification streams share the drain fairly', () => {
  test('three live readers each receive their full sequence in order', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const backend = manager.attachedBackend.backend
    try {
      const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
      const connection = await manager.connect(peerId, NO_OPTIONS)
      const database = await connection.discover(NO_OPTIONS)
      const target = (await database.snapshot()).characteristics[0].path
      const STREAMS = 3
      const COUNT = 40
      const subscriptions = []
      for (let index = 0; index < STREAMS; index += 1) {
        subscriptions.push(await database.subscribe(target, subscribeOptions()))
      }
      // Warmup proves every stream live before the measured flood. One
      // emit fans out to every consumer, so a single emit proves all three
      // live with nothing left buffered (a per-stream emit loop would leave
      // two 0xff strays in every stream — the W6 [2002,8,8] +2).
      const warmupIterators = subscriptions.map(subscription => subscription.values[Symbol.asyncIterator]())
      harness.native.emitNotification(new Uint8Array([0xff]))
      for (const iterator of warmupIterators) {
        const warmup = await iterator.next()
        expect(warmup.done).toBe(false)
        expect(warmup.value.kind).toBe('value')
      }
      const readers = subscriptions.map(
        subscription =>
          (async () => {
            // iterator.next() reports {done, value}; for-await unwraps to
            // the stream entry {kind, value: {value: bytes, delivery}}.
            const items = []
            const iterator = subscription.values[Symbol.asyncIterator]()
            for (let turn = 0; turn < COUNT * 50 + 500 && items.length < COUNT; turn += 1) {
              const outcome = await iterator.next()
              if (outcome.done) break
              if (outcome.value.kind !== 'value') break
              items.push(outcome.value.value.value[0])
            }
            return items
          })()
      )
      const payload = new Uint8Array(20)
      for (let index = 0; index < COUNT; index += 1) {
        payload[0] = index & 0xff
        harness.native.emitNotification(payload)
      }
      const received = await Promise.all(readers)
      const expected = Array.from({ length: COUNT }, (_, index) => index & 0xff)
      for (const items of received) expect(items).toEqual(expected)
      for (const subscription of subscriptions) await subscription.remove()
      await connection.release()
    } finally {
      await manager.destroy()
    }
  }, 120000)
})
