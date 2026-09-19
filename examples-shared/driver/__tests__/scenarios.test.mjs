import { test } from 'node:test'
import assert from 'node:assert/strict'
import { BleError } from 'unified-ble-manager'
import { SCENARIO_IDS, createScenarioRegistry, disposeDriver } from '../create-driver.ts'
import { adapterHostManager, capabilityLease, hostLabel } from '../host.ts'
import { PMD_CONTROL_POINT, PMD_DATA } from '../polar-pmd.ts'
import { PendingUserGestureGate } from '../user-gesture.ts'
import { createFakeHost, createFakeManager, settle } from './fake-host.mjs'

function eventsOf(scenario) {
  return scenario.recentEvents().map(event => event.kind)
}

test('every host gets the same scenarios in the same order', () => {
  const { manager } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  assert.deepEqual(registry.list().map(scenario => scenario.id), [...SCENARIO_IDS])
})

test('a scanning host finds the H10 by query after the adapter readiness wait for scan', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'continuous-scan' })
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  const result = await registry.dispatch('h10-stream', 'start', {})
  assert.equal(result.device, 'Polar H10 1234')
  assert.deepEqual(calls.slice(0, 5), ['waitUntilReady scan', calls[1], 'connect direct', 'discover', 'subscribe 00002a37-0000-1000-8000-00805f9b34fb'])
  assert.match(calls[1], /^find /)
  assert.ok(!calls.some(call => call.startsWith('choose')))
  const stop = await registry.dispatch('h10-stream', 'stop', {})
  assert.deepEqual(stop.cleanup.map(step => [step.step, step.state]), [
    ['subscription.remove', 'released'],
    ['connection.release', 'released'],
    ['manager.destroy', 'released']
  ])
})

test('a chooser host with a gesture gate parks in awaiting-user-gesture until a real grant, then chooses', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const gate = new PendingUserGestureGate()
  const registry = createScenarioRegistry(createFakeHost({ manager, userGesture: gate, adapterHostManager }))
  const scenario = registry.get('h10-stream')
  const run = registry.dispatch('h10-stream', 'start', {})
  await settle()
  assert.equal(scenario.snapshot().phase, 'awaiting-user-gesture')
  assert.ok(eventsOf(scenario).includes('user-gesture-required'))
  assert.equal(gate.pending().length, 1)
  assert.ok(!calls.some(call => call.startsWith('choose')), 'no chooser before the gesture')
  assert.deepEqual(calls, ['waitUntilReady choose'])
  gate.grant(gate.pending()[0].id)
  await run
  const choose = calls.find(call => call.startsWith('choose'))
  assert.deepEqual(JSON.parse(choose.slice('choose '.length)), {
    filters: [{ serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'], localNamePrefix: 'Polar H10' }],
    optionalServices: ['0000180f-0000-1000-8000-00805f9b34fb', '0000180a-0000-1000-8000-00805f9b34fb', 'fb005c80-02e7-f387-1cad-8acd2d8df0c8']
  })
  assert.ok(eventsOf(scenario).includes('user-gesture-received'))
  await registry.dispatch('h10-stream', 'stop', {})
})

test('stop while awaiting a gesture aborts the run without opening the chooser', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const gate = new PendingUserGestureGate()
  const registry = createScenarioRegistry(createFakeHost({ manager, userGesture: gate, adapterHostManager }))
  const run = registry.dispatch('device-info', 'read', {})
  await settle()
  await registry.dispatch('device-info', 'stop', {})
  await assert.rejects(run, { code: 'operation.aborted' })
  assert.equal(gate.pending().length, 0)
  assert.ok(!calls.some(call => call.startsWith('choose')))
  assert.ok(calls.includes('manager.destroy'))
})

test('a chooser host without a gate (Tauri/Electron would scan; this is the plain chooser path) chooses at once', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  await registry.dispatch('h10-stream', 'start', {})
  assert.ok(calls.some(call => call.startsWith('choose')))
  assert.ok(!eventsOf(registry.get('h10-stream')).includes('user-gesture-required'))
  await registry.dispatch('h10-stream', 'stop', {})
})

test('ecg subscribes to the PMD control point and data before reading the features, identically on every host', async () => {
  const { manager, calls, subscriptions } = createFakeManager({ reads: { [PMD_CONTROL_POINT]: new Uint8Array([0x0f, 0x00]) } })
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  manager.connect = (original => async (target, options) => {
    const connection = await original(target, options)
    connection.controls = {
      requestMtu: async () => ({ kind: 'unsupported' }),
      effectiveMtu: async () => ({ kind: 'unsupported' })
    }
    return connection
  })(manager.connect)
  await assert.rejects(registry.dispatch('ecg', 'start', {}), { code: 'pmd.ecg-unsupported' })
  const order = calls.filter(call => call.includes(PMD_CONTROL_POINT) || call.includes(PMD_DATA))
  assert.deepEqual(order.slice(0, 3), [`subscribe ${PMD_CONTROL_POINT}`, `subscribe ${PMD_DATA}`, `read ${PMD_CONTROL_POINT}`])
  assert.equal(subscriptions.length, 2)
})

test('background on a host without app lifecycle reports untracked instead of inventing a state', async () => {
  const { manager } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, host: 'node', adapterHostManager }))
  const scenario = registry.get('background')
  assert.equal(scenario.snapshot().appState, 'untracked')
  await registry.dispatch('background', 'start', { autoReconnect: false })
  assert.ok(eventsOf(scenario).includes('app-state-untracked'))
  assert.equal(scenario.snapshot().currentPeriod, null)
  await registry.dispatch('background', 'stop', {})
})

test('background tracks the host app state and reports the library answer for the lease', async () => {
  const { manager } = createFakeManager()
  const listeners = new Set()
  let reading = { state: 'visible', foreground: true }
  const appState = {
    current: () => reading,
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    }
  }
  const registry = createScenarioRegistry(createFakeHost({ manager, appState, adapterHostManager }))
  const scenario = registry.get('background')
  await registry.dispatch('background', 'start', { autoReconnect: false, backgroundLease: true })
  assert.equal(scenario.snapshot().leaseState, 'capability-unregistered')
  reading = { state: 'hidden', foreground: false }
  for (const listener of listeners) listener(reading)
  assert.equal(scenario.snapshot().appState, 'hidden')
  assert.equal(scenario.snapshot().currentPeriod.foreground, false)
  await registry.dispatch('background', 'stop', {})
  assert.equal(listeners.size, 0)
  assert.deepEqual(scenario.snapshot().periods.map(period => period.appState), ['visible', 'hidden'])
})

test('restoration start records the known peer and reports the platform capability answers verbatim', async () => {
  const { manager } = createFakeManager({
    capabilities: { 'state:restoration-adoption': 'limited', 'state:presence-observation': 'unsupported' }
  })
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  const scenario = registry.get('restoration')
  await registry.dispatch('restoration', 'start', { autoReconnect: false })
  assert.equal(scenario.snapshot().knownPeerId, 'peer-h10')
  assert.ok(eventsOf(scenario).includes('restoration-known-peer'))
  assert.ok(eventsOf(scenario).includes('restoration-capabilities'))
  assert.equal(scenario.snapshot().adoptionCapability, 'limited')
  assert.equal(scenario.snapshot().presenceCapability, 'unsupported')
  await registry.dispatch('restoration', 'stop', {})
})

test('restoration reports unregistered capabilities as unregistered instead of inventing an answer', async () => {
  const { manager } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  const scenario = registry.get('restoration')
  await registry.dispatch('restoration', 'start', { autoReconnect: false })
  assert.equal(scenario.snapshot().adoptionCapability, 'unregistered')
  assert.equal(scenario.snapshot().presenceCapability, 'unregistered')
  await registry.dispatch('restoration', 'stop', {})
})

test('restoration reconnect dials the recorded peer id directly with no new scan', async () => {
  const { manager, calls } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  await registry.dispatch('restoration', 'start', { autoReconnect: false })
  await registry.dispatch('restoration', 'stop', {})
  const finds = calls.filter(call => call.startsWith('find ')).length
  await registry.dispatch('restoration', 'reconnect', { peerId: 'peer-h10', intent: 'when-available' })
  assert.equal(calls.filter(call => call.startsWith('find ')).length, finds)
  assert.ok(calls.includes('connect when-available'))
  const scenario = registry.get('restoration')
  assert.ok(eventsOf(scenario).includes('restoration-reconnected'))
  assert.equal(scenario.snapshot().reconnects, 1)
  assert.equal(scenario.snapshot().knownPeerId, 'peer-h10')
  await registry.dispatch('restoration', 'stop', {})
})

test('restoration reconnect without a known peer is refused, never silently skipped', async () => {
  const { manager } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  await assert.rejects(registry.dispatch('restoration', 'reconnect', {}), { code: 'scenario.no-known-peer' })
})

test('capabilityLease passes the library capability report on verbatim and holds nothing', async () => {
  const { manager } = createFakeManager({ capabilities: { 'background:desktop-maintain-connection': 'supported', 'web:background-operation': 'unsupported' } })
  const supported = await capabilityLease(manager, 'background:desktop-maintain-connection')
  assert.equal(supported.state, 'capability-supported')
  assert.deepEqual(await supported.release(), { feature: 'background:desktop-maintain-connection', held: false })
  const unsupported = await capabilityLease(manager, 'web:background-operation')
  assert.equal(unsupported.state, 'capability-unsupported')
  assert.deepEqual(unsupported.detail.descriptor.limitations, [{ code: 'web:background-operation-unsupported' }])
  assert.equal((await capabilityLease(manager, 'lifecycle:page-persistence')).state, 'capability-unregistered')
  assert.equal(hostLabel({ host: 'node', platform: 'macos' }), 'node/macos')
})

test('a gesture grant for an unknown request is refused, not ignored', () => {
  const gate = new PendingUserGestureGate()
  assert.throws(() => gate.grant(42), { code: 'host.user-gesture-unknown' })
})

const PEER_ACQUIRING = [
  ['h10-stream', 'start', {}],
  ['link-loss', 'start', {}],
  ['device-info', 'read', {}],
  ['mtu', 'probe', {}],
  ['ecg', 'start', {}],
  ['background', 'start', { autoReconnect: false }],
  ['restoration', 'start', { autoReconnect: false }],
  ['h10-capture', 'capture', {}]
]

async function acquisitionCalls(fake, scenario, command, args) {
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  // Only the acquisition is under test; the run is stopped and not awaited
  // (link-loss's real connection supervisor cannot finish against this double).
  void registry.dispatch(scenario, command, args).catch(() => {})
  await settle()
  void registry.dispatch(scenario, 'stop', {}).catch(() => {})
  return fake.calls.filter(call => /^(find|choose|waitUntilReady)\b/.test(call)).map(call => call.split(' ').slice(0, 2).join(' '))
}

test('the backend capability report, not discovery.kind or the host name, picks find vs choose in every peer-acquiring scenario', async () => {
  for (const [scenario, command, args] of PEER_ACQUIRING) {
    // scan supported, chooser unsupported, but a misleading discovery kind: still find()
    const scanning = createFakeManager({ discovery: 'system-chooser', capabilities: { 'discovery:continuous-scan': 'supported', 'discovery:system-chooser': 'unsupported' } })
    const scanCalls = await acquisitionCalls(scanning, scenario, command, args)
    assert.deepEqual(scanCalls.map(call => call.split(' ')[0]), ['waitUntilReady', 'find'], `${scenario} on a scanning backend`)
    assert.equal(scanCalls[0], 'waitUntilReady scan')
    // scan unsupported, chooser supported: choose()
    const choosing = createFakeManager({ discovery: 'continuous-scan', capabilities: { 'discovery:continuous-scan': 'unsupported', 'discovery:system-chooser': 'supported' } })
    const chooseCalls = await acquisitionCalls(choosing, scenario, command, args)
    assert.deepEqual(chooseCalls.map(call => call.split(' ')[0]), ['waitUntilReady', 'choose'], `${scenario} on a chooser-only backend`)
    assert.equal(chooseCalls[0], 'waitUntilReady choose')
  }
})

test('a backend reporting both scan and chooser scans; one reporting neither is asked to find so the library answers', async () => {
  const both = createFakeManager({ capabilities: { 'discovery:continuous-scan': 'supported', 'discovery:system-chooser': 'supported' } })
  assert.deepEqual((await acquisitionCalls(both, 'device-info', 'read', {})).map(call => call.split(' ')[0]), ['waitUntilReady', 'find'])
  const neither = createFakeManager({ capabilities: { 'discovery:continuous-scan': 'unsupported', 'discovery:system-chooser': 'unsupported' } })
  assert.deepEqual((await acquisitionCalls(neither, 'device-info', 'read', {})).map(call => call.split(' ')[0]), ['waitUntilReady', 'find'])
})

// --- transient connect failures: one explicit retry, only when the library says the caller decides ---

const transientConnectFailure = () =>
  new BleError('connection.failed', 'connection', 'connection.connect', {
    retryability: 'caller-decides',
    platform: { domain: 'android.gatt', code: '133', safeMessage: 'GATT_ERROR', metadata: {} }
  })
const finalConnectFailure = () => new BleError('connection.failed', 'connection', 'connection.connect', { retryability: 'never' })

const SINGLE_SHOT = [
  ['h10-stream', 'start', {}],
  ['device-info', 'read', {}],
  ['mtu', 'probe', {}],
  ['ecg', 'start', {}]
]

async function runSingleShot(fake, scenario, command, args) {
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  const outcome = await registry.dispatch(scenario, command, args).then(
    result => ({ ok: true, result }),
    error => ({ ok: false, error })
  )
  if (registry.get(scenario).snapshot().phase === 'streaming') await registry.dispatch(scenario, 'stop', {})
  return { registry, outcome, connects: fake.calls.filter(call => call.startsWith('connect ')).length }
}

test('a caller-decides connect failure is retried exactly once, after an explicit connect-retry event, in every single-shot scenario', async () => {
  for (const [scenario, command, args] of SINGLE_SHOT) {
    const fake = createFakeManager({ connectFailures: [transientConnectFailure()] })
    const { registry, connects } = await runSingleShot(fake, scenario, command, args)
    assert.equal(connects, 2, `${scenario} connects twice`)
    const events = registry.get(scenario).recentEvents()
    const retry = events.find(event => event.kind === 'connect-retry')
    assert.ok(retry !== undefined, `${scenario} emits connect-retry`)
    assert.equal(retry.data.attempt, 2)
    assert.equal(retry.data.maxAttempts, 2)
    assert.equal(retry.data.error.code, 'connection.failed')
    assert.equal(retry.data.error.detail.retryability, 'caller-decides')
    const connected = events.find(event => event.kind === 'connected')
    assert.equal(connected.data.attempt, 2, `${scenario} reports which attempt connected`)
    assert.ok(retry.seq < connected.seq)
  }
})

test('a never-retryable connect failure is not retried', async () => {
  for (const [scenario, command, args] of SINGLE_SHOT) {
    const fake = createFakeManager({ connectFailures: [finalConnectFailure()] })
    const { registry, outcome, connects } = await runSingleShot(fake, scenario, command, args)
    assert.equal(connects, 1, `${scenario} connects once`)
    assert.equal(outcome.ok, false)
    assert.equal(outcome.error.code, 'connection.failed')
    assert.ok(!registry.get(scenario).recentEvents().some(event => event.kind === 'connect-retry'))
  }
})

test('a second caller-decides failure ends the run with that failure: never more than one retry', async () => {
  const fake = createFakeManager({ connectFailures: [transientConnectFailure(), transientConnectFailure(), transientConnectFailure()] })
  const { registry, outcome, connects } = await runSingleShot(fake, 'device-info', 'read', {})
  assert.equal(connects, 2)
  assert.equal(outcome.ok, false)
  assert.equal(outcome.error.retryability, 'caller-decides')
  assert.equal(registry.get('device-info').recentEvents().filter(event => event.kind === 'connect-retry').length, 1)
  assert.equal(registry.get('device-info').snapshot().phase, 'failed')
})

test('retryability is read only from a BleError, never inferred from a look-alike code or field', async () => {
  const lookAlike = Object.assign(new Error('gatt 133'), { code: 'connection.failed', retryability: 'caller-decides' })
  const fake = createFakeManager({ connectFailures: [lookAlike] })
  const { outcome, connects } = await runSingleShot(fake, 'mtu', 'probe', {})
  assert.equal(connects, 1)
  assert.equal(outcome.ok, false)
})

test('stop during the connect that failed does not retry', async () => {
  const fake = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  fake.manager.connect = async () => {
    fake.calls.push('connect direct')
    await registry.dispatch('device-info', 'stop', {})
    throw transientConnectFailure()
  }
  await assert.rejects(registry.dispatch('device-info', 'read', {}), { code: 'connection.failed' })
  assert.equal(fake.calls.filter(call => call.startsWith('connect ')).length, 1)
  assert.ok(!registry.get('device-info').recentEvents().some(event => event.kind === 'connect-retry'))
})

// --- device selection: one strap per host ---

const STRAP_A = 'Polar H10 E997042F'
const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'

function findQuery(calls) {
  const find = calls.find(call => call.startsWith('find '))
  return JSON.parse(find.slice('find '.length))
}

function chooseFilters(calls) {
  const choose = calls.find(call => call.startsWith('choose '))
  return JSON.parse(choose.slice('choose '.length)).filters
}

test('every peer-acquiring command declares and accepts a device argument; stop does not', () => {
  const { manager } = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
  const accepting = registry
    .describe()
    .flatMap(scenario => scenario.commands.filter(command => command.acceptsDevice).map(command => `${scenario.id}.${command.name}`))
  assert.deepEqual(accepting.sort(), PEER_ACQUIRING.map(([scenario, command]) => `${scenario}.${command}`).sort())
})

test('an exact device name becomes a names.exact query; a trailing * a prefix; none keeps the Polar H10 prefix', async () => {
  for (const [scenario, command, extra] of PEER_ACQUIRING) {
    const cases = [
      [{ device: STRAP_A }, { exact: [STRAP_A] }],
      [{ device: 'Polar H10 E99*' }, { prefixes: ['Polar H10 E99'] }],
      [{}, { prefixes: ['Polar H10'] }]
    ]
    for (const [deviceArgs, names] of cases) {
      const fake = createFakeManager({ peerName: STRAP_A })
      await acquisitionCalls(fake, scenario, command, { ...extra, ...deviceArgs })
      assert.deepEqual(findQuery(fake.calls), { anyOf: [{ services: { any: [HR_SERVICE] }, names }] }, `${scenario} ${JSON.stringify(deviceArgs)}`)
    }
  }
})

test('the Web chooser filter carries the device name as its name prefix', async () => {
  const exact = createFakeManager({ discovery: 'system-chooser', peerName: STRAP_A })
  await acquisitionCalls(exact, 'device-info', 'read', { device: STRAP_A })
  assert.deepEqual(chooseFilters(exact.calls), [{ serviceUuids: [HR_SERVICE], localNamePrefix: STRAP_A }])
  const prefix = createFakeManager({ discovery: 'system-chooser' })
  await acquisitionCalls(prefix, 'device-info', 'read', { device: 'Polar H10 E9*' })
  assert.deepEqual(chooseFilters(prefix.calls), [{ serviceUuids: [HR_SERVICE], localNamePrefix: 'Polar H10 E9' }])
})

test('a chooser pick that is not the exact device asked for fails the run instead of using another strap', async () => {
  const fake = createFakeManager({ discovery: 'system-chooser', peerName: `${STRAP_A}X` })
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  await assert.rejects(registry.dispatch('device-info', 'read', { device: STRAP_A }), { code: 'scenario.device-mismatch' })
  assert.ok(!fake.calls.some(call => call.startsWith('connect ')))
  assert.ok(fake.calls.includes('manager.destroy'))
})

test('the chosen device (id, name and the query that found it) is reported in every result and snapshot', async () => {
  const expected = { id: 'peer-h10', name: STRAP_A, query: { match: 'exact', name: STRAP_A } }
  for (const [scenario, command, extra] of [['h10-stream', 'start', {}], ['device-info', 'read', {}], ['mtu', 'probe', {}]]) {
    const fake = createFakeManager({ peerName: STRAP_A })
    const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
    const result = await registry.dispatch(scenario, command, { ...extra, device: STRAP_A })
    assert.deepEqual(result.peer, expected, `${scenario} result`)
    assert.deepEqual(registry.get(scenario).snapshot().peer, expected, `${scenario} snapshot`)
    assert.equal(registry.get(scenario).snapshot().device, STRAP_A)
    if (registry.get(scenario).snapshot().phase === 'streaming') await registry.dispatch(scenario, 'stop', {})
  }
  const fake = createFakeManager({ peerName: STRAP_A, reads: { [PMD_CONTROL_POINT]: new Uint8Array([0x0f, 0x00]) } })
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  await registry.dispatch('ecg', 'start', {}).catch(() => {})
  assert.deepEqual(registry.get('ecg').snapshot().peer, { id: 'peer-h10', name: STRAP_A, query: { match: 'prefix', name: 'Polar H10' } })
})

test('a device argument that is not a non-empty name is refused before anything runs', async () => {
  const fake = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  for (const device of [42, '', '*', ' ']) {
    await assert.rejects(registry.dispatch('device-info', 'read', { device }), { code: 'scenario.invalid-argument' }, JSON.stringify(device))
  }
  await assert.rejects(registry.dispatch('device-info', 'stop', { device: STRAP_A }), { code: 'scenario.invalid-argument' })
  assert.deepEqual(fake.calls, [])
})

// --- stopAll: what a Fast Refresh / HMR dispose calls ---

test('stopAll releases a streaming run and leaves no connection behind', async () => {
  const fake = createFakeManager()
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  await registry.dispatch('h10-stream', 'start', {})
  const report = await registry.stopAll()
  const entry = report.scenarios.find(item => item.scenario === 'h10-stream')
  assert.equal(entry.wasRunning, true)
  assert.deepEqual(entry.cleanup.map(step => [step.step, step.state]), [
    ['subscription.remove', 'released'],
    ['connection.release', 'released'],
    ['manager.destroy', 'released']
  ])
  assert.ok(report.scenarios.filter(item => item.scenario !== 'h10-stream').every(item => !item.wasRunning))
  assert.equal(registry.get('h10-stream').snapshot().phase, 'stopped')
  assert.deepEqual(report.failures, [])
})

test('stopAll aborts a run still acquiring its peer', async () => {
  const fake = createFakeManager({ discovery: 'system-chooser' })
  const gate = new PendingUserGestureGate()
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, userGesture: gate, adapterHostManager }))
  const run = registry.dispatch('ecg', 'start', {})
  await settle()
  await registry.stopAll()
  await assert.rejects(run, { code: 'operation.aborted' })
  assert.ok(fake.calls.includes('manager.destroy'))
  assert.ok(!fake.calls.some(call => call.startsWith('connect ')))
})

test('stopAll rejects with the failed release but still destroys the manager', async () => {
  const fake = createFakeManager({ releaseError: Object.assign(new Error('gatt close failed'), { code: 'platform.failure' }) })
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  await registry.dispatch('h10-stream', 'start', {})
  await assert.rejects(registry.stopAll(), error => {
    assert.equal(error.code, 'scenario.stop-all-failed')
    assert.deepEqual(error.report.failures.map(failure => [failure.scenario, failure.step, failure.state, failure.detail.code]), [
      ['h10-stream', 'connection.release', 'threw', 'platform.failure']
    ])
    return true
  })
  assert.ok(fake.calls.includes('manager.destroy'))
})

test('disposeDriver (the HMR / Fast Refresh hook) stops the remote channel first, then every run, and logs the report', async () => {
  const fake = createFakeManager()
  const host = createFakeHost({ manager: fake.manager, adapterHostManager })
  const registry = createScenarioRegistry(host)
  await registry.dispatch('h10-stream', 'start', {})
  const order = []
  const remote = { stop: () => order.push(`remote.stop with ${fake.calls.includes('connection.release') ? 'released' : 'open'} connection`) }
  const report = await disposeDriver({ remote, registry, runtime: host.runtime }, 'hmr-dispose')
  assert.deepEqual(order, ['remote.stop with open connection'])
  assert.ok(fake.calls.includes('connection.release') && fake.calls.includes('manager.destroy'))
  assert.equal(report.scenarios.find(entry => entry.scenario === 'h10-stream').wasRunning, true)
  assert.ok(host.runtime.logs.some(entry => entry.scope === 'driver' && entry.message === 'hmr-dispose: every scenario stopped'))
})

test('disposeDriver reports a cleanup failure and rethrows it', async () => {
  const fake = createFakeManager({ releaseError: Object.assign(new Error('gatt close failed'), { code: 'platform.failure' }) })
  const host = createFakeHost({ manager: fake.manager, adapterHostManager })
  const registry = createScenarioRegistry(host)
  await registry.dispatch('h10-stream', 'start', {})
  await assert.rejects(disposeDriver({ remote: null, registry, runtime: host.runtime }, 'hmr-dispose'), { code: 'scenario.stop-all-failed' })
  const logged = host.runtime.logs.find(entry => entry.message === 'hmr-dispose: cleanup failed')
  assert.equal(logged.detail.code, 'scenario.stop-all-failed')
  assert.equal(logged.detail.detail.cause.failures[0].step, 'connection.release')
})

test('finding 185: a stuck scan fails one scenario loudly and the next scenario heals it without a process restart', async () => {
  // Every scenario run creates its own manager on the process-shared scan
  // registry. The first run's find leaves its scan open (a stop that failed
  // while find still reported it loudly); the failed run tears down and
  // destroys its manager, and the next run heals the retained membership
  // instead of failing scan.already-active. The doubles model the fixed
  // provider contract; the provider tests prove the provider honors it.
  const sharedScans = new Map()
  let ordinal = 0
  const created = []
  const host = createFakeHost({ manager: createFakeManager().manager, adapterHostManager })
  host.createManager = async () => {
    ordinal += 1
    const fake = createFakeManager({
      managerId: `sequence-${ordinal}`,
      sharedScans,
      failFindStop: ordinal === 1 ? 1 : 0,
      failDispose: ordinal === 1 ? 1 : 0
    })
    created.push(fake)
    return adapterHostManager(fake.manager, 'background:desktop-maintain-connection')
  }
  const registry = createScenarioRegistry(host)

  await assert.rejects(registry.dispatch('h10-stream', 'start', {}), /scan stop failed/)
  assert.ok(created[0].calls.includes('manager.destroy'), 'the failed run tears down its manager')

  const result = await registry.dispatch('h10-stream', 'start', {})
  assert.equal(result.device, 'Polar H10 1234')
  assert.ok(created[1].calls.includes('heal stuck scan'), 'the next run heals the retained membership')
  assert.ok(!created[1].calls.some(call => call.includes('already-active')))

  const stop = await registry.dispatch('h10-stream', 'stop', {})
  assert.deepEqual(stop.cleanup.map(step => [step.step, step.state]), [
    ['subscription.remove', 'released'],
    ['connection.release', 'released'],
    ['manager.destroy', 'released']
  ])
})

test('finding 185: destroy clears the owner stuck scan so a later manager finds cleanly', async () => {
  const sharedScans = new Map()
  const first = createFakeManager({ managerId: 'owner', sharedScans, failFindStop: 1 })
  await assert.rejects(first.manager.find({ query: {} }), /scan stop failed/)
  assert.equal(sharedScans.get('stuck')?.owner, 'owner')
  await first.manager.destroy()
  assert.ok(!sharedScans.has('stuck'), 'no scan lease survives the destroyed manager')
  const second = createFakeManager({ managerId: 'next', sharedScans })
  const peer = await second.manager.find({ query: {} })
  assert.equal(peer.name, 'Polar H10 1234')
})
