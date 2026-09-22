// h10-capture scenario tests: versioned fingerprint shape, public-API-only
// acquisition, and statistical summaries. Uses a local manager double so the
// real-host timing knobs stay tiny.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createFakeRuntime } from './fake-runtime.mjs'
import { H10CaptureScenario, FINGERPRINT_VERSION, summarizeDistribution } from '../scenarios/h10-capture.ts'
import { PMD_CONTROL_POINT, PMD_DATA } from '../polar-pmd.ts'

function hrBytes(bpm) {
  return new Uint8Array([0x16, bpm, 0x00, 0x04])
}

function makeHost(overrides = {}) {
  const calls = []
  const hrStream = makeStream()
  const ecgStream = makeStream()
  const cpStream = makeStream()
  const batteryStream = makeStream()
  const scanStream = makeStream()
  const now = { at: 1000 }
  const runtime = createFakeRuntime('test/test')
  const peer = { id: 'peer-h10', name: 'Polar H10 E997042F', rssi: -60, reference: null, sources: ['test'] }
  const charUuids = {
    battery: '00002a19-0000-1000-8000-00805f9b34fb',
    bsl: '00002a38-0000-1000-8000-00805f9b34fb',
    cp: PMD_CONTROL_POINT,
    data: PMD_DATA
  }
  const reads = {
    '00002a19-0000-1000-8000-00805f9b34fb': new Uint8Array([85]),
    '00002a29-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('Polar Electro Oy'),
    '00002a24-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('H10'),
    '00002a25-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('E997042F'),
    '00002a26-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('3.2.1'),
    '00002a27-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('9'),
    '00002a28-0000-1000-8000-00805f9b34fb': new TextEncoder().encode('3.2.1'),
    '00002a23-0000-1000-8000-00805f9b34fb': new Uint8Array([1, 0, 0, 0, 0, 0x6b, 0, 0]),
    '00002a38-0000-1000-8000-00805f9b34fb': new Uint8Array([1]),
    [charUuids.cp]: new Uint8Array([0x00, 0x01, 0x00]),
    ...overrides.reads
  }
  const characteristic = (service, uuid) => ({
    uuid,
    service: { uuid: service },
    properties: { read: true, notify: true, indicate: uuid === charUuids.cp, writeWithResponse: true, writeWithoutResponse: false },
    occurrence: 0,
    descriptors: [],
    access: {},
    async read() {
      calls.push(`read ${uuid}`)
      const bytes = reads[uuid]
      if (bytes === undefined) throw Object.assign(new Error(`no ${uuid}`), { code: 'gatt.attribute-not-found' })
      return bytes
    },
    async write(bytes, options) {
      calls.push(`write ${uuid}`)
      const first = bytes[0]
      // PMD control point: answer GET_SETTINGS/START/STOP with SUCCESS, anything else INVALID_OP.
      if (uuid === charUuids.cp) {
        const status = first === 0x01 || first === 0x02 || first === 0x03 ? 0x00 : 0x01
        const response = first === 0x01 && status === 0x00
          ? new Uint8Array([0xf0, first, bytes[1] ?? 0, 0x00, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0e, 0x00])
          : new Uint8Array([0xf0, first ?? 0, bytes[1] ?? 0, status])
        queueMicrotask(() => {
          cpStream.push({ value: response, delivery: 'indication', observedAtMonotonicMs: runtime.now(), sequence: 1 })
        })
      }
      return { state: 'written' }
    },
    async subscribe(options) {
      calls.push(`subscribe ${uuid}`)
      const values = uuid === charUuids.data ? ecgStream : uuid === charUuids.cp ? cpStream : uuid === charUuids.battery ? batteryStream : hrStream
      return {
        uuid,
        values,
        requestedDelivery: options?.delivery ?? null,
        effectiveDelivery: 'notification',
        async remove() {
          calls.push(`unsubscribe ${uuid}`)
          return { state: 'released', failures: [] }
        }
      }
    }
  })
  const gatt = {
    generation: 'db-1',
    services: [
      { uuid: '0000180d-0000-1000-8000-00805f9b34fb', occurrence: 0, primary: true, includedServices: [], characteristics: [] },
      { uuid: '0000180f-0000-1000-8000-00805f9b34fb', occurrence: 0, primary: true, includedServices: [], characteristics: [] }
    ],
    characteristic: (service, uuid) => characteristic(String(service), String(uuid)),
    snapshot: () => ({
      generation: 'db-1',
      services: [{ uuid: '0000180d-0000-1000-8000-00805f9b34fb', occurrence: 0, primary: true }],
      characteristics: [{ uuid: '00002a37-0000-1000-8000-00805f9b34fb', occurrence: 0, serviceOccurrence: 0, properties: { notify: true } }],
      descriptors: [{ uuid: '00002902-0000-1000-8000-00805f9b34fb', occurrence: 0 }]
    })
  }
  const connection = {
    connectionGeneration: 'gen-1',
    lifecycleEvents: makeStream(),
    controls: {
      requestMtu: async mtu => { calls.push(`requestMtu ${mtu}`); return { kind: 'negotiated', attMtu: 247 } },
      effectiveMtu: async () => ({ kind: 'measured', attMtu: 247 }),
      maximumWriteLength: async mode => ({ kind: 'measured', maximumWriteLength: 244 }),
      readRssi: async () => ({ kind: 'measured', rssi: -60 }),
      readPhy: async () => { throw Object.assign(new Error('unsupported'), { code: 'capability.unsupported' }) },
      parameters: async () => ({ kind: 'measured', intervalMs: 30 })
    },
    async discover() { calls.push('discover'); return gatt },
    async release() { calls.push('connection.release'); return { state: 'released', failures: [] } }
  }
  // Finding 209: one manager admits one scan at a time, like every real
  // backend. The advertisement scan stays open until its stop, so a find
  // (which opens a second scan) is refused with scan.already-active.
  let scanOpen = false
  const alreadyActive = () =>
    Object.assign(new Error('scan.already-active: scan scan-1 is still active; stop it before starting a new scan'), {
      code: 'scan.already-active'
    })
  const manager = {
    discovery: { kind: 'continuous-scan' },
    adapter: {
      async state() { return { availability: 'available', power: 'on', authorization: 'granted' } },
      async waitUntilReady() { calls.push('waitUntilReady scan'); return { availability: 'available', power: 'on', authorization: 'granted' } }
    },
    capabilities: {
      supports: id => id === 'discovery:continuous-scan',
      get: () => undefined,
      list: () => []
    },
    async find() {
      calls.push('find')
      if (scanOpen) throw alreadyActive()
      return peer
    },
    async choose() { return peer },
    async scan(options) {
      calls.push('scan all')
      if (scanOpen) throw alreadyActive()
      scanOpen = true
      return {
        plan: { queryDigest: 'digest-1' },
        state: makeStream(),
        events: makeStream(),
        observations: scanStream,
        async stop() {
          calls.push('scan.stop')
          scanOpen = false
          scanStream.end()
          return { state: 'released', failures: [] }
        }
      }
    },
    async connect() { calls.push('connect direct'); return connection },
    async destroy() { calls.push('manager.destroy'); return { state: 'released', failures: [] } }
  }
  const host = {
    identity: { host: 'node', platform: 'linux', backend: 'test/identity-probe', model: 'fake', osVersion: '0', appBuild: {} },
    runtime,
    appState: null,
    userGesture: null,
    createManager: async () => ({ manager, prepare: async () => {}, acquireBackgroundLease: async () => ({ state: 'x', detail: null, release: async () => null }) })
  }
  // Two scan observations, then the source ends delivery while the scan
  // session stays open (finding 209: every loop exit — stream end or the
  // duration break — must stop the scan before find opens the next one).
  // The end rides the fake clock so the capture observes it mid-run, like a
  // real radio going quiet, instead of all in the first microtask drain.
  runtime.schedule(() => scanStream.end(), 500)
  queueMicrotask(() => {
    scanStream.push({
      peer,
      localName: 'Polar H10 E997042F',
      rssi: -60,
      connectable: true,
      serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
      manufacturerData: [{ companyId: 107, data: new Uint8Array([]) }],
      serviceData: null,
      observedAtMonotonicMs: runtime.now()
    })
    scanStream.push({
      peer,
      localName: 'Polar H10 E997042F',
      rssi: -61,
      connectable: true,
      serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
      manufacturerData: [{ companyId: 107, data: new Uint8Array([]) }],
      serviceData: null,
      observedAtMonotonicMs: runtime.now()
    })
  })
  return { host, calls, hrStream, ecgStream, cpStream, runtime }
}

function makeStream() {
  const items = []
  let wake = null
  let ended = false
  return {
    push(item) { items.push({ kind: 'value', value: item }); wake?.() },
    end() { ended = true; wake?.() },
    async *[Symbol.asyncIterator]() {
      for (;;) {
        if (items.length > 0) { yield items.shift(); continue }
        if (ended) return
        await new Promise(resolve => (wake = resolve))
        wake = null
      }
    }
  }
}

test('summarizeDistribution reports n/min/p10/p50/p90/max/mean/stdev', () => {
  const dist = summarizeDistribution([1000, 1000, 1000, 1000])
  assert.equal(dist.n, 4)
  assert.equal(dist.min, 1000)
  assert.equal(dist.max, 1000)
  assert.equal(dist.mean, 1000)
  assert.equal(dist.stdev, 0)
  assert.equal(summarizeDistribution([]).n, 0)
})

test('h10-capture records a versioned fingerprint through public API only', async () => {
  const { host, calls, hrStream, ecgStream, runtime } = makeHost()
  const ecgFrame = () => {
    const bytes = new Uint8Array(13)
    bytes[0] = 0x00
    bytes[9] = 0x00
    return bytes
  }
  const { ScenarioRegistry } = await import('../scenario-core.ts')
  const { createScenarioRegistry } = await import('../create-driver.ts')
  const { adapterHostManager } = await import('../host.ts')
  void adapterHostManager
  void ScenarioRegistry
  void createScenarioRegistry
  const scenario = new H10CaptureScenario(host)
  assert.equal(scenario.describe().commands.find(c => c.name === 'capture').acceptsDevice, true)
  // Feed HR values + ECG frame while the capture runs.
  // The scenario waits on the fake clock; keep advancing it until the run
  // settles instead of a fixed number of turns.
  let settled = false
  const run = scenario.dispatch('capture', { device: 'Polar H10 E997042F', scanDurationMs: 50, hrDurationMs: 2500, hrMinValues: 2, ecgFrames: 1 }).then(
    value => { settled = true; return value },
    error => { settled = true; throw error }
  )
  for (let i = 0; i < 200 && !settled; i += 1) {
    hrStream.push({ value: hrBytes(72), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: i })
    ecgStream.push({ value: ecgFrame(), delivery: 'notification', observedAtMonotonicMs: runtime.now(), sequence: i })
    runtime.advance(1000)
    await new Promise(resolve => setImmediate(resolve))
  }
  const fingerprint = await run
  assert.equal(fingerprint.version, FINGERPRINT_VERSION)
  assert.equal(fingerprint.device.query.name, 'Polar H10 E997042F')
  assert.ok(fingerprint.advertisement.observations >= 2, 'scan observations recorded')
  assert.ok(fingerprint.gatt.services.length >= 1, 'GATT services recorded')
  assert.equal(fingerprint.values.batteryLevelPercent.ok, true)
  assert.equal(fingerprint.values.bodySensorLocation.ok, true)
  assert.equal(fingerprint.values.pmdFeatures.ok, true)
  assert.ok(fingerprint.timings.hrNotificationIntervalMs.n >= 2, 'HR intervals measured')
  assert.equal(fingerprint.timings.pmdResponseMs.n, 3, 'get-settings repeated for a latency distribution')
  assert.ok(fingerprint.behaviour.invalidPmdCommand.errorCode !== undefined, 'invalid PMD command probed')
  // The fake host carries a sentinel backend label, so this asserts the
  // scenario propagates host identity instead of echoing a hardcoded string.
  assert.equal(fingerprint.host.backend, 'test/identity-probe')
  assert.equal(typeof fingerprint.capturedAt, 'string')
  // Finding 209: the advertisement scan stops before find opens the second
  // scan, so the single-scan arbitration admits it.
  assert.ok(calls.includes('scan.stop'), `the capture scan stopped: ${JSON.stringify(calls)}`)
  assert.ok(calls.indexOf('scan.stop') < calls.indexOf('find'), `the capture scan stops before find: ${JSON.stringify(calls)}`)
})

test('h10-capture refuses bad durations instead of silently capturing nothing', async () => {
  const { host } = makeHost()
  const scenario = new H10CaptureScenario(host)
  await assert.rejects(scenario.dispatch('capture', { scanDurationMs: -1 }), /scanDurationMs/)
})
