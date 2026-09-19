// live-dashboard scenario tests: one tile per Polar H10 in range, driven by a
// scripted manager double (scan + per-peer connections + PMD answers), so the
// same assertions hold on every host. Test-first for the LiveDashboardScenario.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createFakeRuntime } from './fake-runtime.mjs'
import {
  BATTERY_POLL_MS,
  ECG_BUFFER_CAP_SAMPLES,
  ECG_DISPLAY_MAX_POINTS,
  ECG_WINDOW_SAMPLES,
  LiveDashboardScenario,
  downsampleEcg
} from '../scenarios/live-dashboard.ts'
import { createScenarioRegistry } from '../create-driver.ts'

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const BATTERY_LEVEL = '00002a19-0000-1000-8000-00805f9b34fb'
const MODEL_NUMBER = '00002a24-0000-1000-8000-00805f9b34fb'
const SERIAL_NUMBER = '00002a25-0000-1000-8000-00805f9b34fb'
const FIRMWARE_REVISION = '00002a26-0000-1000-8000-00805f9b34fb'
const MANUFACTURER_NAME = '00002a29-0000-1000-8000-00805f9b34fb'
const PMD_CONTROL_POINT = 'FB005C81-02E7-F387-1CAD-8ACD2D8DF0C8'
const PMD_DATA = 'FB005C82-02E7-F387-1CAD-8ACD2D8DF0C8'

function hrBytes(bpm, rrMs = null) {
  if (rrMs === null) return new Uint8Array([0x06, bpm])
  const rr = Math.round((rrMs * 1024) / 1000)
  return new Uint8Array([0x16, bpm, rr & 0xff, (rr >> 8) & 0xff])
}

/** One PMD type-0 frame: header + 3-byte LE µV samples. */
function ecgFrameBytes(timestampNs, samples) {
  const bytes = new Uint8Array(10 + samples.length * 3)
  const view = new DataView(bytes.buffer)
  bytes[0] = 0x00
  view.setBigUint64(1, timestampNs, true)
  bytes[9] = 0x00
  samples.forEach((sample, index) => {
    const unsigned = sample < 0 ? sample + 0x1000000 : sample
    const offset = 10 + index * 3
    bytes[offset] = unsigned & 0xff
    bytes[offset + 1] = (unsigned >> 8) & 0xff
    bytes[offset + 2] = (unsigned >> 16) & 0xff
  })
  return bytes
}

/** Enveloped queue stream (subscription / scan observations): one consumer. */
function makeStream() {
  const items = []
  let wake = null
  let ended = false
  return {
    push(item) {
      items.push({ kind: 'value', value: item })
      wake?.()
    },
    end() {
      ended = true
      wake?.()
    },
    async *[Symbol.asyncIterator]() {
      for (;;) {
        if (items.length > 0) {
          yield items.shift()
          continue
        }
        if (ended) return
        await new Promise(resolve => (wake = resolve))
        wake = null
      }
    }
  }
}

/**
 * Broadcast raw stream (connection lifecycle): every iterator sees every
 * event, and `return()` settles a parked `next()` at once, like a backend
 * whose release ends the lifecycle stream. (An `async function*` suspended at
 * a bare `await` would park `return()` forever and hang supervisor cleanup.)
 */
function makeBroadcast() {
  const readers = new Set()
  return {
    push(event) {
      for (const reader of readers) reader.deliver(event)
    },
    [Symbol.asyncIterator]() {
      const reader = {
        queue: [],
        pending: null,
        done: false,
        deliver(event) {
          if (reader.done) return
          if (reader.pending !== null) {
            const resolve = reader.pending
            reader.pending = null
            resolve({ value: event, done: false })
          } else {
            reader.queue.push(event)
          }
        }
      }
      readers.add(reader)
      return {
        next() {
          if (reader.queue.length > 0) return Promise.resolve({ value: reader.queue.shift(), done: false })
          if (reader.done) return Promise.resolve({ value: undefined, done: true })
          return new Promise(resolve => {
            reader.pending = resolve
          })
        },
        async return() {
          reader.done = true
          readers.delete(reader)
          if (reader.pending !== null) {
            const resolve = reader.pending
            reader.pending = null
            resolve({ value: undefined, done: true })
          }
          return { value: undefined, done: true }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

function textBytes(text) {
  return new TextEncoder().encode(text)
}

/**
 * A scan-capable manager double: pushable scan observations, one connection
 * per peer id with its own lifecycle broadcast, per-subscribe streams, and a
 * PMD control point that answers GET_SETTINGS/START/STOP with SUCCESS.
 */
function makeDashboardHost(overrides = {}) {
  const calls = []
  const runtime = createFakeRuntime('test/test')
  const scanStream = makeStream()
  const scanEvents = makeStream()
  const subscriptions = []
  const peers = overrides.peers ?? [{ id: 'peer-h10-1', name: 'Polar H10 E997042F', rssi: -60, reference: null, sources: ['test'] }]
  const reads = {
    [BATTERY_LEVEL]: new Uint8Array([85]),
    [MANUFACTURER_NAME]: textBytes('Polar Electro Oy'),
    [MODEL_NUMBER]: textBytes('H10'),
    [SERIAL_NUMBER]: textBytes('E997042F'),
    [FIRMWARE_REVISION]: textBytes('3.2.1'),
    [PMD_CONTROL_POINT]: new Uint8Array([0x00, 0x01, 0x00]),
    ...(overrides.reads ?? {})
  }
  const batteryError = overrides.batterySubscribeError ?? null
  const connections = new Map()
  const cpStreams = []

  const characteristic = (service, uuid) => ({
    uuid,
    async read() {
      calls.push(`read ${uuid}`)
      const bytes = reads[uuid]
      if (bytes === undefined) throw Object.assign(new Error(`no ${uuid}`), { code: 'gatt.attribute-not-found' })
      return bytes
    },
    async write(bytes) {
      calls.push(`write ${uuid}`)
      if (uuid === PMD_CONTROL_POINT) {
        const op = bytes[0]
        const ok = op === 0x01 || op === 0x02 || op === 0x03
        const response =
          op === 0x01 && ok
            ? new Uint8Array([0xf0, op, bytes[1] ?? 0, 0x00, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0e, 0x00])
            : new Uint8Array([0xf0, op ?? 0, bytes[1] ?? 0, ok ? 0x00 : 0x01])
        queueMicrotask(() => {
          cpStreams.at(-1)?.push({ value: response, delivery: 'indication', observedAtMonotonicMs: runtime.now(), sequence: 1 })
        })
      }
      return { state: 'written' }
    },
    async subscribe(options) {
      calls.push(`subscribe ${uuid}`)
      if (uuid === BATTERY_LEVEL && batteryError !== null) throw batteryError
      const values = makeStream()
      const subscription = {
        uuid,
        values,
        requestedDelivery: options?.delivery ?? null,
        effectiveDelivery: uuid === PMD_CONTROL_POINT ? 'indication' : 'notification',
        async remove() {
          calls.push(`unsubscribe ${uuid}`)
          values.end()
          return { state: 'released', failures: [] }
        }
      }
      if (uuid === PMD_CONTROL_POINT) cpStreams.push(values)
      subscriptions.push(subscription)
      return subscription
    }
  })
  const gatt = {
    generation: 'db-1',
    services: [{ uuid: '0000180f-0000-1000-8000-00805f9b34fb' }],
    characteristic: (service, uuid) => characteristic(String(service), String(uuid))
  }
  const connectionFor = peerId => {
    let connection = connections.get(peerId)
    if (connection === undefined) {
      connection = {
        connectionGeneration: `gen-${peerId}`,
        lifecycleEvents: makeBroadcast(),
        controls: {
          requestMtu: async mtu => {
            calls.push(`requestMtu ${mtu}`)
            return { kind: 'negotiated', attMtu: 247 }
          },
          effectiveMtu: async () => ({ kind: 'measured', attMtu: 247 })
        },
        async discover() {
          calls.push(`discover ${peerId}`)
          return gatt
        },
        async release() {
          calls.push(`connection.release ${peerId}`)
          return { state: 'released', failures: [] }
        }
      }
      connections.set(peerId, connection)
    }
    return connection
  }
  const discovery = overrides.discovery ?? 'continuous-scan'
  const capabilities = {
    'discovery:continuous-scan': discovery === 'system-chooser' ? 'unsupported' : 'supported',
    'discovery:system-chooser': discovery === 'system-chooser' ? 'supported' : 'unsupported',
    ...(overrides.capabilities ?? {})
  }
  const manager = {
    discovery: { kind: discovery },
    adapter: {
      async state() {
        return { availability: 'available', power: 'on', authorization: 'granted' }
      },
      async waitUntilReady(options) {
        calls.push(`waitUntilReady ${options.operation}`)
        return { availability: 'available', power: 'on', authorization: 'granted' }
      }
    },
    capabilities: {
      supports: id => capabilities[id] === 'supported' || capabilities[id] === 'limited',
      get: id => (capabilities[id] === undefined ? undefined : { id, state: capabilities[id], limitations: [] }),
      list: () => []
    },
    async find(options) {
      calls.push(`find ${JSON.stringify(options.query)}`)
      return peers[0]
    },
    async choose(options) {
      calls.push(`choose ${JSON.stringify({ filters: options.filters })}`)
      return peers[0]
    },
    async scan(options) {
      calls.push(`scan ${JSON.stringify(options.query)}`)
      return {
        plan: null,
        state: makeStream(),
        events: scanEvents,
        observations: scanStream,
        async stop() {
          calls.push('scan.stop')
          scanStream.end()
          scanEvents.end()
          return { state: 'released', failures: [] }
        }
      }
    },
    async connect(target, options) {
      const peerId = typeof target === 'string' ? target : (target.id ?? 'peer-h10-1')
      calls.push(`connect ${peerId} ${options?.intent ?? 'direct'}`)
      return connectionFor(peerId)
    },
    async destroy() {
      calls.push('manager.destroy')
      return { state: 'released', failures: [] }
    }
  }
  const host = {
    identity: { host: 'node', platform: 'linux', backend: 'node/bluez', model: 'fake', osVersion: '0', appBuild: {} },
    runtime,
    appState: null,
    userGesture: null,
    createManager: async () => ({
      manager,
      prepare: async () => {},
      acquireBackgroundLease: async () => ({ state: 'x', detail: null, release: async () => null })
    })
  }
  const observationFor = peer => ({
    peer,
    localName: peer.name,
    rssi: peer.rssi,
    connectable: true,
    serviceUuids: [HR_SERVICE],
    manufacturerData: [],
    serviceData: null,
    observedAtMonotonicMs: runtime.now()
  })
  return { host, calls, runtime, scanStream, scanEvents, subscriptions, connections, connectionFor, observationFor, peers }
}

function lastSubscription(subscriptions, uuid) {
  const matches = subscriptions.filter(subscription => subscription.uuid === uuid)
  return matches.at(-1) ?? null
}

async function pollFor(description, probe, timeoutMs = 8000, intervalMs = 25) {
  const startedAt = Date.now()
  for (;;) {
    const value = probe()
    if (value !== null && value !== undefined && value !== false) return value
    if (Date.now() - startedAt > timeoutMs) throw new Error(`timed out waiting for ${description}`)
    await new Promise(resolve => setTimeout(resolve, intervalMs))
  }
}

function tileOf(scenario, peerId) {
  return scenario.snapshot().tiles[peerId] ?? null
}

test('live-dashboard is registered on every host with start/stop/snapshot commands', () => {
  const { host } = makeDashboardHost()
  const registry = createScenarioRegistry(host)
  const ids = registry.list().map(scenario => scenario.id)
  assert.ok(ids.includes('live-dashboard'), `registry holds live-dashboard (has ${ids.join(', ')})`)
  const scenario = registry.get('live-dashboard')
  const commands = Object.fromEntries(scenario.describe().commands.map(command => [command.name, command]))
  assert.equal(commands.start.acceptsDevice, false)
  assert.deepEqual(Object.keys(commands).sort(), ['snapshot', 'start', 'stop'])
})

test('a tile appears on scan observation and walks discovered -> connecting -> streaming', async () => {
  const { host, scanStream, observationFor, peers, subscriptions, runtime } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: false })
  assert.equal(scenario.snapshot().phase, 'scanning')
  scanStream.push(observationFor(peers[0]))
  const firstSeen = await pollFor('tile discovery', () => tileOf(scenario, peers[0].id))
  assert.ok(
    ['discovered', 'connecting', 'streaming'].includes(firstSeen.status),
    `tile walks discovered -> connecting -> streaming (first seen ${firstSeen.status})`
  )
  assert.equal(firstSeen.name, 'Polar H10 E997042F')
  assert.ok(
    scenario.recentEvents().some(event => event.kind === 'tile-discovered' && event.data.tile === peers[0].id),
    'the discovered state is announced, even when the fake connects before the first poll'
  )
  const streaming = await pollFor('tile streaming', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.status === 'streaming' ? tile : null
  })
  assert.equal(streaming.connectionGeneration, 'gen-peer-h10-1')
  assert.equal(streaming.supervisorState, 'connected')
  const hr = lastSubscription(subscriptions, HR_MEASUREMENT)
  assert.ok(hr !== null, 'heart-rate subscribed')
  hr.values.push({ value: hrBytes(72, 800), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: 0 })
  const withHr = await pollFor('heart-rate value', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.bpm === 72 ? tile : null
  })
  assert.equal(withHr.contact, 'detected')
  assert.deepEqual(withHr.rrIntervalsMs, [800])
  await scenario.dispatch('stop', {})
  assert.equal(tileOf(scenario, peers[0].id).status, 'off')
})

test('link loss reports the supervisor backoff word as reconnecting, then streams again', async () => {
  const { host, scanStream, observationFor, peers, subscriptions, runtime, connectionFor } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: false })
  scanStream.push(observationFor(peers[0]))
  await pollFor('tile streaming', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.status === 'streaming' ? tile : null
  })
  connectionFor(peers[0].id).lifecycleEvents.push({
    sequence: 1,
    previous: 'connected',
    current: 'lost',
    cause: 'connection.lost',
    connectionGeneration: 'gen-peer-h10-1'
  })
  const reconnecting = await pollFor('reconnecting tile', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.status === 'reconnecting' ? tile : null
  })
  assert.ok(['backoff', 'connecting', 'configuring', 'disconnecting'].includes(reconnecting.supervisorState), `library state shown (${reconnecting.supervisorState})`)
  assert.equal(reconnecting.lifecycleCause, 'connection.lost')
  const backoffEvent = await pollFor('supervisor backoff event', () =>
    scenario.recentEvents().find(event => event.kind === 'tile-supervisor' && event.data.state === 'backoff')
  )
  assert.equal(backoffEvent.data.tile, peers[0].id)
  const recovered = await pollFor('tile streaming again', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.status === 'streaming' ? tile : null
  })
  assert.equal(recovered.supervisorState, 'connected')
  const hr = lastSubscription(subscriptions, HR_MEASUREMENT)
  hr.values.push({ value: hrBytes(65), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: 1 })
  await pollFor('post-reconnect value', () => tileOf(scenario, peers[0].id)?.bpm === 65)
  await scenario.dispatch('stop', {})
})

test('battery notifies when supported and polls when the library refuses the subscription', async () => {
  const plain = makeDashboardHost()
  const plainScenario = new LiveDashboardScenario(plain.host)
  await plainScenario.dispatch('start', { ecg: false })
  plain.scanStream.push(plain.observationFor(plain.peers[0]))
  await pollFor('plain tile streaming', () => tileOf(plainScenario, plain.peers[0].id)?.status === 'streaming')
  assert.equal(tileOf(plainScenario, plain.peers[0].id).batteryDelivery, 'notification')
  const battery = lastSubscription(plain.subscriptions, BATTERY_LEVEL)
  battery.values.push({ value: new Uint8Array([85]), delivery: 'notification', observedAtMonotonicMs: plain.runtime.now(), sequence: 0 })
  await pollFor('battery notification', () => tileOf(plainScenario, plain.peers[0].id)?.batteryPercent === 85)
  await plainScenario.dispatch('stop', {})

  const refused = makeDashboardHost({
    batterySubscribeError: Object.assign(new Error('notify not supported'), { code: 'capability.unsupported' })
  })
  const refusedScenario = new LiveDashboardScenario(refused.host)
  await refusedScenario.dispatch('start', { ecg: false })
  refused.scanStream.push(refused.observationFor(refused.peers[0]))
  const polling = await pollFor('polling tile', () => {
    const tile = tileOf(refusedScenario, refused.peers[0].id)
    return tile !== null && tile.batteryDelivery === 'poll' ? tile : null
  })
  assert.equal(polling.batteryPercent, 85, 'the configure-time read still reports a value')
  const readsBefore = refused.calls.filter(call => call === `read ${BATTERY_LEVEL}`).length
  refused.runtime.advance(BATTERY_POLL_MS + 1)
  await pollFor('periodic battery read', () => refused.calls.filter(call => call === `read ${BATTERY_LEVEL}`).length > readsBefore)
  const unsupported = refusedScenario.recentEvents().find(event => event.kind === 'tile-battery-poll')
  assert.ok(unsupported !== undefined, 'the fallback is announced, not silent')
  assert.equal(unsupported.data.reason, 'capability.unsupported')
  await refusedScenario.dispatch('stop', {})
})

test('firmware revision, model and serial are read per tile and failures stay visible', async () => {
  const { host, scanStream, observationFor, peers } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: false })
  scanStream.push(observationFor(peers[0]))
  const identified = await pollFor('device information', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.firmwareRevision !== null ? tile : null
  })
  assert.equal(identified.firmwareRevision, '3.2.1')
  assert.equal(identified.modelNumber, 'H10')
  assert.equal(identified.serialNumber, 'E997042F')
  assert.equal(identified.manufacturerName, 'Polar Electro Oy')
  await scenario.dispatch('stop', {})

  const missing = makeDashboardHost({ reads: { [FIRMWARE_REVISION]: undefined } })
  const missingScenario = new LiveDashboardScenario(missing.host)
  await missingScenario.dispatch('start', { ecg: false })
  missing.scanStream.push(missing.observationFor(missing.peers[0]))
  const reported = await pollFor('failed firmware read', () => {
    const tile = tileOf(missingScenario, missing.peers[0].id)
    return tile !== null && tile.infoErrors.firmwareRevision !== undefined ? tile : null
  })
  assert.equal(reported.firmwareRevision, null)
  assert.equal(reported.infoErrors.firmwareRevision.code, 'gatt.attribute-not-found')
  assert.equal(reported.modelNumber, 'H10', 'one missing characteristic never hides the others')
  await missingScenario.dispatch('stop', {})
})

test('the ECG buffer is bounded and the display is a downsampled ~5 s window', async () => {
  const { host, scanStream, observationFor, peers, subscriptions, runtime } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: true })
  scanStream.push(observationFor(peers[0]))
  await pollFor('tile streaming', () => tileOf(scenario, peers[0].id)?.status === 'streaming')
  const ecg = lastSubscription(subscriptions, PMD_DATA)
  assert.ok(ecg !== null, 'PMD data subscribed')
  let timestampNs = 0n
  const pushFrames = count => {
    for (let frame = 0; frame < count; frame += 1) {
      timestampNs += 53_846_154n
      ecg.values.push({ value: ecgFrameBytes(timestampNs, [100, 200, 300, 400, 500, 600, 700]), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: frame })
    }
  }
  pushFrames(200)
  const bounded = await pollFor('200 ECG frames counted', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.ecgSamples === 1400 ? tile : null
  })
  assert.ok(bounded.ecgBuffered <= ECG_BUFFER_CAP_SAMPLES, `buffer capped (${bounded.ecgBuffered})`)
  assert.ok(bounded.ecgDisplay.length <= ECG_DISPLAY_MAX_POINTS, `display downsampled (${bounded.ecgDisplay.length})`)
  pushFrames(300)
  const stillBounded = await pollFor('500 ECG frames counted', () => {
    const tile = tileOf(scenario, peers[0].id)
    return tile !== null && tile.ecgSamples === 3500 ? tile : null
  })
  assert.ok(stillBounded.ecgBuffered <= ECG_BUFFER_CAP_SAMPLES, `buffer still capped (${stillBounded.ecgBuffered})`)
  assert.equal(stillBounded.ecgBuffered, ECG_BUFFER_CAP_SAMPLES, 'the oldest samples scroll out of the window')
  assert.ok(stillBounded.ecgDisplay.length <= ECG_DISPLAY_MAX_POINTS)
  await scenario.dispatch('stop', {})
})

test('downsampleEcg keeps the newest window and decimates to the point budget', () => {
  assert.deepEqual(downsampleEcg([], 650, 130), [])
  const buffer = Array.from({ length: 1300 }, (_, index) => index)
  const display = downsampleEcg(buffer, ECG_WINDOW_SAMPLES, ECG_DISPLAY_MAX_POINTS)
  assert.ok(display.length <= ECG_DISPLAY_MAX_POINTS, `decimated to ${display.length} points`)
  assert.equal(display.at(-1), 1299, 'the newest sample stays on screen')
  assert.ok(display[0] >= 650, 'the window scrolls: samples older than ~5 s fall off')
})

test('many straps stream side by side, one tile each', async () => {
  const strapA = { id: 'peer-h10-a', name: 'Polar H10 AAAAAA01', rssi: -55, reference: null, sources: ['test'] }
  const strapB = { id: 'peer-h10-b', name: 'Polar H10 BBBBBB02', rssi: -70, reference: null, sources: ['test'] }
  const { host, scanStream, observationFor, subscriptions, runtime } = makeDashboardHost({ peers: [strapA, strapB] })
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: false })
  scanStream.push(observationFor(strapA))
  scanStream.push(observationFor(strapB))
  await pollFor('both tiles streaming', () => {
    const tiles = scenario.snapshot().tiles
    return tiles[strapA.id]?.status === 'streaming' && tiles[strapB.id]?.status === 'streaming'
  })
  assert.deepEqual([...scenario.snapshot().tileOrder].sort(), ['peer-h10-a', 'peer-h10-b'])
  const hrStreams = subscriptions.filter(subscription => subscription.uuid === HR_MEASUREMENT)
  assert.equal(hrStreams.length, 2, 'one heart-rate subscription per strap')
  hrStreams[0].values.push({ value: hrBytes(70), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: 0 })
  hrStreams[1].values.push({ value: hrBytes(120), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: 0 })
  await pollFor('per-strap heart rates', () => scenario.snapshot().tiles[strapA.id]?.bpm === 70 && scenario.snapshot().tiles[strapB.id]?.bpm === 120)
  await scenario.dispatch('stop', {})
  assert.equal(scenario.snapshot().tiles[strapA.id]?.status, 'off')
  assert.equal(scenario.snapshot().tiles[strapB.id]?.status, 'off')
})

test('a named device list scans exact names; bad arguments are refused, never ignored', async () => {
  const { host, calls, scanStream, observationFor, peers } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { devices: ['Polar H10 E997042F'], ecg: false })
  const scan = calls.find(call => call.startsWith('scan '))
  assert.deepEqual(JSON.parse(scan.slice('scan '.length)), {
    anyOf: [{ services: { any: [HR_SERVICE] }, names: { exact: ['Polar H10 E997042F'] } }]
  })
  scanStream.push(observationFor(peers[0]))
  await pollFor('named tile', () => tileOf(scenario, peers[0].id)?.status === 'streaming')
  await scenario.dispatch('stop', {})
  await assert.rejects(scenario.dispatch('start', { devices: [ '' ] }), { code: 'scenario.invalid-argument' })
  await assert.rejects(scenario.dispatch('start', { devices: 42 }), { code: 'scenario.invalid-argument' })
  await scenario.dispatch('stop', {}).catch(() => {})
})

test('a second start while running is refused; the snapshot command reports the tiles', async () => {
  const { host, scanStream, observationFor, peers } = makeDashboardHost()
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { ecg: false })
  await assert.rejects(scenario.dispatch('start', {}), { code: 'scenario.busy' })
  scanStream.push(observationFor(peers[0]))
  await pollFor('tile present', () => tileOf(scenario, peers[0].id) !== null)
  const snapshot = await scenario.dispatch('snapshot', {})
  assert.deepEqual(Object.keys(snapshot.tiles), [peers[0].id])
  await scenario.dispatch('stop', {})
})

test('a chooser-only backend acquires through choose, never scan', async () => {
  const { host, calls, peers } = makeDashboardHost({ discovery: 'system-chooser' })
  const scenario = new LiveDashboardScenario(host)
  await scenario.dispatch('start', { devices: ['Polar H10 E997042F'], ecg: false })
  await pollFor('chooser tile streaming', () => tileOf(scenario, peers[0].id)?.status === 'streaming')
  assert.ok(calls.some(call => call.startsWith('choose ')), 'the system chooser acquires the peer')
  assert.ok(!calls.some(call => call.startsWith('scan ')), 'no scan on a chooser-only backend')
  await scenario.dispatch('stop', {})
})
