import { test } from 'node:test'
import assert from 'node:assert/strict'
import { normalizeScanObservation, normalizeScanQuery, observationMatchesScanQuery } from 'unified-ble-manager/advanced'
import { ScanDetailsScenario } from '../scenarios/scan-details.ts'
import { adapterHostManager } from '../host.ts'
import { createFakeHost, createFakeManager, stream } from './fake-host.mjs'

async function startScan(durationMs, extra = {}) {
  const { manager, calls } = createFakeManager()
  const observations = stream()
  const state = stream()
  let admitted
  const started = new Promise(resolve => { admitted = resolve })
  manager.scan = async options => {
    admitted(options)
    return {
      state,
      observations,
      async stop() {
        calls.push('scan.stop')
        state.end()
        observations.end()
        return { state: 'released', failures: [] }
      }
    }
  }
  const host = createFakeHost({ manager, adapterHostManager })
  const scenario = new ScanDetailsScenario(host)
  const running = scenario.dispatch('scan', { durationMs, ...extra })
  const options = await started
  await new Promise(resolve => setImmediate(resolve))
  return {
    calls, host, scenario, options, running,
    observe(fields = {}) {
      observations.push({ kind: 'value', value: {
        peer: { id: 'peer-h10', name: 'Polar H10 1234', reference: null, sources: ['test'] },
        localName: 'Polar H10 1234', rssi: -60, connectable: true,
        serviceUuids: [], manufacturerData: [], serviceData: null, ...fields
      } })
    },
    end() {
      observations.push({ kind: 'terminal', reason: 'operation-timed-out', droppedItems: 0, droppedBytes: 0, replacedItems: 0 })
      observations.end()
      state.end()
    }
  }
}

test('explicit radio addresses combine conjunctively with the chosen service filter and retain cleanup', async () => {
  const scan = await startScan(500, { filter: 'heart-rate-service', addresses: ['dc:56:7b:d9:e8:a4'] })
  assert.deepEqual(scan.options.query, { anyOf: [{ services: { any: ['0000180d-0000-1000-8000-00805f9b34fb'] }, addresses: ['DC:56:7B:D9:E8:A4'] }] })
  const observed = { peerId: 'peer-h10', address: 'DC:56:7B:D9:E8:A4', localName: 'SIM Polar H10', rssi: -60,
    txPowerLevel: null, serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'], manufacturerData: [], serviceData: [] }
  const query = normalizeScanQuery(scan.options.query)
  assert.equal(observationMatchesScanQuery(query, normalizeScanObservation(observed)), true)
  assert.equal(observationMatchesScanQuery(query, normalizeScanObservation({ ...observed, address: 'DC:56:7B:D9:E8:A5' })), false)
  assert.equal(observationMatchesScanQuery(query, normalizeScanObservation({ ...observed, serviceUuids: [] })), false)
  scan.observe({ address: observed.address, serviceUuids: observed.serviceUuids })
  scan.end()
  assert.equal((await scan.running).observations, 1)
  assert.ok(scan.calls.includes('scan.stop'))
  assert.ok(scan.calls.includes('manager.destroy'))
})

test('explicit address-only scan leaves no hidden service/name selector', async () => {
  const scan = await startScan(500, { addresses: ['DC:56:7B:D9:E8:A4'] })
  assert.deepEqual(scan.options.query, { anyOf: [{ addresses: ['DC:56:7B:D9:E8:A4'] }] })
  scan.end()
  await scan.running
})

test('malformed address selectors reject before host manager allocation', async () => {
  for (const addresses of [[], null, {}, 'DC:56:7B:D9:E8:A4', [7], ['not-an-address']]) {
    const { manager, calls } = createFakeManager()
    const scenario = new ScanDetailsScenario(createFakeHost({ manager, adapterHostManager }))
    await assert.rejects(scenario.dispatch('scan', { addresses }))
    assert.deepEqual(calls, [])
  }
})

for (const [durationMs, count] of [[3000, 1], [10000, 1], [500.75, 1], [3000, 0]]) {
  test(`scan-details forwards its finite ${durationMs}ms lifetime and reports ${count} observations on native end without JS timers`, async () => {
    const scan = await startScan(durationMs)
    const { calls, host, scenario, options, running } = scan
    assert.equal(options.timeoutMs, Math.floor(durationMs))
    if (count > 0) scan.observe()
    scan.end()
    const summary = await running
    assert.equal(host.runtime.now(), 0, 'the JavaScript duration timer did not run')
    assert.equal(summary.observations, count)
    assert.equal(summary.uniquePeers, count)
    assert.equal(summary.peers.length, count)
    assert.ok(scenario.recentEvents().some(event => event.kind === 'stream-terminal' && event.data.reason === 'operation-timed-out'))
    assert.ok(calls.includes('scan.stop'))
    assert.ok(calls.includes('manager.destroy'))
  })
}

test('scan-details final idle window fills completed seconds and the actual partial bucket', async () => {
  const scan = await startScan(10000)
  scan.host.runtime.advance(4700)
  scan.observe()
  await new Promise(resolve => setImmediate(resolve))
  scan.host.runtime.advance(5280)
  scan.end()
  const summary = await scan.running
  assert.deepEqual(summary.perSecond, [0, 0, 0, 0, 1, 0, 0, 0, 0, 0])
  assert.equal(summary.elapsedMs, 9980)
  assert.equal(summary.observations, 1)
  assert.equal(summary.observationsPerSecond, 1000 / 9980)
  assert.deepEqual(scan.scenario.recentEvents().filter(event => event.kind === 'rate').map(event => event.data.windowEndMs), [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 9980])
})

for (const elapsedMs of [0, 1000, 2000]) {
  test(`scan-details ending at exact ${elapsedMs}ms boundary emits no zero-length trailing bucket`, async () => {
    const scan = await startScan(10000)
    scan.observe()
    await new Promise(resolve => setImmediate(resolve))
    scan.host.runtime.advance(elapsedMs)
    scan.end()
    const summary = await scan.running
    assert.equal(summary.perSecond.length, elapsedMs / 1000)
    assert.equal(summary.elapsedMs, elapsedMs)
    assert.equal(summary.observations, 1)
    assert.equal(summary.observationsPerSecond, elapsedMs > 0 ? 1000 / elapsedMs : null)
    assert.deepEqual(scan.scenario.recentEvents().filter(event => event.kind === 'rate').map(event => event.data.windowEndMs),
      Array.from({ length: elapsedMs / 1000 }, (_, index) => (index + 1) * 1000))
  })
}

for (const boundaryMs of [1000, 2000]) {
  test(`scan-details includes an observation at ${boundaryMs}ms in its right-closed final bucket`, async () => {
    const scan = await startScan(10000)
    scan.host.runtime.advance(boundaryMs)
    scan.observe()
    scan.end()
    const summary = await scan.running
    assert.deepEqual(summary.perSecond, boundaryMs === 1000 ? [1] : [0, 1])
    assert.equal(summary.observations, 1)
    assert.equal(summary.perSecond.reduce((total, count) => total + count, 0), summary.observations)
    assert.equal(summary.elapsedMs, boundaryMs)
    assert.deepEqual(scan.scenario.recentEvents().filter(event => event.kind === 'rate').map(event => event.data.windowEndMs),
      Array.from({ length: boundaryMs / 1000 }, (_, index) => (index + 1) * 1000))
  })
}

test('scan-details conserves repeated boundary observations and the following partial window', async () => {
  const scan = await startScan(10000)
  scan.observe()
  await new Promise(resolve => setImmediate(resolve))
  scan.host.runtime.advance(1000)
  scan.observe()
  scan.observe()
  await new Promise(resolve => setImmediate(resolve))
  scan.host.runtime.advance(1)
  scan.observe()
  scan.end()
  const summary = await scan.running
  assert.deepEqual(summary.perSecond, [3, 1])
  assert.equal(summary.observations, 4)
  assert.equal(summary.perSecond.reduce((total, count) => total + count, 0), summary.observations)
  assert.equal(summary.elapsedMs, 1001)
  assert.deepEqual(scan.scenario.recentEvents().filter(event => event.kind === 'rate').map(event => event.data.windowEndMs), [1000, 1001])
})

test('scan-details repeated observations at the final exact boundary share one positive-duration window', async () => {
  const scan = await startScan(10000)
  scan.host.runtime.advance(1000)
  scan.observe()
  await new Promise(resolve => setImmediate(resolve))
  scan.observe()
  scan.end()
  const summary = await scan.running
  assert.deepEqual(summary.perSecond, [2])
  assert.equal(summary.observations, 2)
  assert.equal(scan.scenario.recentEvents().filter(event => event.kind === 'rate').length, 1)
})
