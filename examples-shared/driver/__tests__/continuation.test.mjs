import { test } from 'node:test'
import assert from 'node:assert/strict'
import { SCENARIO_IDS, createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'
import { POLAR_PREFERRED_MTU } from '../polar-pmd.ts'

function eventsOf(scenario) {
  return scenario.recentEvents().map(event => event.kind)
}

function androidManager(overrides = {}) {
  return createFakeManager({
    capabilities: {
      'background:wake-on-appearance': 'limited',
      'background:native-resubscribe': 'limited',
      'background:headless-task': 'unsupported',
      'background:wake-notification': 'unsupported'
    },
    ...overrides
  })
}

function registryFor(manager, configureContinuation = async () => ({ manager })) {
  return createScenarioRegistry({
    ...createFakeHost({ manager, adapterHostManager: m => ({ manager: m }) }),
    configureContinuation
  })
}

test('headless diagnostic command is optional, offline and propagates storage failure', async () => {
  const { manager } = androidManager()
  const base = createFakeHost({ manager })
  await assert.rejects(createScenarioRegistry(base).dispatch('continuation', 'headless-history', {}), error => error.code === 'capability.unsupported')
  const expected = { count: 1, records: [{ state: 'completed', batteryPercent: 73, cleanupState: 'released' }] }
  const registry = createScenarioRegistry({ ...base,
    createManager: async () => { throw new Error('diagnostics must not open radio') },
    readHeadlessContinuationHistory: async () => expected
  })
  assert.deepEqual(await registry.dispatch('continuation', 'headless-history', {}), expected)
  const failure = new Error('storage unavailable')
  const broken = createScenarioRegistry({ ...base, readHeadlessContinuationHistory: async () => { throw failure } })
  await assert.rejects(broken.dispatch('continuation', 'headless-history', {}), error => error === failure)
})

test('offline recording prepare retains its token until explicit acknowledgement without opening a manager', async () => {
  const { manager } = androidManager()
  const calls = []
  const registry = createScenarioRegistry({
    ...createFakeHost({ manager }),
    createManager: async () => { throw new Error('offline controls must not open a manager') },
    continuationRecordings: () => ({
      status: async id => { calls.push(['status', id]); return { state: 'recording' } },
      prepare: async (id, options) => { calls.push(['prepare', id, options]); return { token: 'held-token', records: [{ sensorSecret: 'do-not-log' }], bytes: 30, more: false } },
      acknowledge: async (id, token) => { calls.push(['acknowledge', id, token]); return { token, acknowledged: true, records: 1 } },
      stop: async id => { calls.push(['stop', id]); return { state: 'stopped', radioRelease: 'not-requested' } },
      clear: async id => { calls.push(['clear', id]); return { cleared: true, records: 0 } }
    })
  })
  const batch = await registry.dispatch('continuation', 'recording-prepare', { recordingId: 'capture_1', maxItems: 5, maxBytes: 1000 })
  assert.equal(batch.token, 'held-token')
  assert.equal(batch.records[0].sensorSecret, 'do-not-log')
  assert.equal(JSON.stringify(registry.get('continuation').recentEvents()).includes('do-not-log'), false)
  assert.deepEqual(calls, [['prepare', 'capture_1', { maxItems: 5, maxBytes: 1000 }]])
  await registry.dispatch('continuation', 'recording-acknowledge', { recordingId: 'capture_1', token: batch.token })
  await registry.dispatch('continuation', 'recording-status', { recordingId: 'capture_1' })
  await registry.dispatch('continuation', 'recording-stop', { recordingId: 'capture_1' })
  await registry.dispatch('continuation', 'recording-clear', { recordingId: 'capture_1' })
  assert.deepEqual(calls.slice(1).map(call => call[0]), ['acknowledge', 'status', 'stop', 'clear'])
})

test('headless and foreground declarations forward required public configuration', async () => {
  const { manager } = androidManager()
  const declarations = []
  const registry = registryFor(manager, async declaration => { declarations.push(declaration); return { manager } })
  await registry.dispatch('continuation', 'declare', { onAppearance: 'headless-task', headlessTaskName: 'UBMContinuationWake' })
  assert.equal(declarations[0].headlessTaskName, 'UBMContinuationWake')
  const foregroundService = { notification: { channelId: 'ubm', channelName: 'BLE', title: 'Recording', body: 'Collecting', icon: 'ic_ble' } }
  await registry.dispatch('continuation', 'declare', { onAppearance: 'foreground-service', foregroundService })
  assert.deepEqual(declarations[1].foregroundService, foregroundService)
  for (const malformed of [
    { onAppearance: 'headless-task' },
    { onAppearance: 'foreground-service' },
    { onAppearance: 'native', headlessTaskName: 'task' },
    { onAppearance: 'foreground-service', foregroundService: { notification: { channelId: 'a', channelName: 'b', title: 'c', body: 2 } } }
  ]) await assert.rejects(registry.dispatch('continuation', 'declare', malformed))
  assert.equal(declarations.length, 2)
})

test('declare persists the real public declaration and stop disarms it before clearing state', async () => {
  const { manager } = androidManager()
  const declared = []
  const registry = registryFor(manager, async declaration => { declared.push(declaration); return { manager } })
  await registry.dispatch('continuation', 'declare', { onAppearance: 'native', peerId: 'A0:9E:1A:E9:B9:3D' })
  assert.equal(declared.length, 1)
  assert.equal(declared[0].resubscribe[0].characteristicUuid, '00002a37-0000-1000-8000-00805f9b34fb')
  await registry.dispatch('continuation', 'stop', {})
  assert.equal(declared[1].onAppearance, 'record-only')
  assert.equal(registry.get('continuation').snapshot().strategy, null)
})

test('fresh scenario stop disarms a previously persisted native order without relying on local display state', async () => {
  const { manager } = androidManager()
  const declarations = []
  const registry = registryFor(manager, async declaration => { declarations.push(declaration); return { manager } })
  assert.equal(registry.get('continuation').snapshot().strategy, null)
  await registry.dispatch('continuation', 'stop', {})
  assert.deepEqual(declarations, [{ onAppearance: 'record-only', resubscribe: [] }])
})

test('persisted recipe remains observable and manager cleanup remains retryable after refusal', async () => {
  const { manager } = androidManager()
  const declared = []
  let releases = 0
  manager.destroy = async () => (++releases === 1 ? { state: 'release-failed', failures: [] } : { state: 'released', failures: [] })
  const registry = registryFor(manager, async declaration => { declared.push(declaration); return { manager } })
  await assert.rejects(registry.dispatch('continuation', 'declare', { onAppearance: 'native' }))
  assert.equal(registry.get('continuation').snapshot().strategy, 'native')
  await registry.dispatch('continuation', 'stop', {})
  assert.ok(releases >= 3)
  assert.equal(declared.at(-1).onAppearance, 'record-only')
  assert.equal(registry.get('continuation').snapshot().strategy, null)
})

test('H10 ECG and ACC recipe includes generic setup, MTU and explicit durable quotas', async () => {
  const { manager } = androidManager()
  let received
  const registry = registryFor(manager, async declaration => { received = declaration; return { manager } })
  await registry.dispatch('continuation', 'declare', { measurements: 'hr-ecg-acc', sampleRateHz: 200, rangeG: 8,
    recordingId: 'capture_1', maxBytes: 1048576, maxRecords: 10000 })
  assert.equal(received.setup.length, 4)
  assert.equal(received.link.mtu.requested, POLAR_PREFERRED_MTU)
  assert.deepEqual(received.recording, { id: 'capture_1', maxBytes: 1048576, maxRecords: 10000 })
  assert.equal(received.resubscribe.length, 3)
})

test('status does not reconfigure a persisted order and rejected cleanup is retried before disarming', async () => {
  const { manager } = androidManager()
  let configurations = 0
  let creations = 0
  let releases = 0
  manager.destroy = async () => {
    releases += 1
    if (releases === 2) throw new Error('cleanup rejected')
    return { state: 'released', failures: [] }
  }
  const registry = createScenarioRegistry({
    ...createFakeHost({ manager, adapterHostManager: m => ({ manager: m }) }),
    createManager: async () => { creations += 1; return { manager } },
    configureContinuation: async () => { configurations += 1; return { manager } }
  })
  await registry.dispatch('continuation', 'declare', {})
  assert.equal(creations, 0, 'configure returns the owning manager; no second default manager')
  await assert.rejects(registry.dispatch('continuation', 'status', {}), /cleanup rejected/)
  assert.equal(configurations, 1, 'status must not persist a default record-only declaration')
  assert.equal(registry.get('continuation').snapshot().strategy, 'native')
  await registry.dispatch('continuation', 'stop', {})
  assert.equal(releases, 4, 'retry failed owner before creating and releasing disarm owner')
  assert.equal(configurations, 2)
})

test('refused disarm cleanup preserves the persisted record-only fact until retry', async () => {
  const { manager } = androidManager()
  let releases = 0
  let configurations = 0
  manager.destroy = async () => (++releases === 2 ? { state: 'release-failed', failures: [] } : { state: 'released', failures: [] })
  const registry = registryFor(manager, async () => { configurations += 1; return { manager } })
  await registry.dispatch('continuation', 'declare', {})
  await assert.rejects(registry.dispatch('continuation', 'stop', {}))
  assert.equal(registry.get('continuation').snapshot().strategy, 'record-only')
  await registry.dispatch('continuation', 'stop', {})
  assert.equal(configurations, 2, 'retry cleanup, not declaration side effects')
  assert.equal(registry.get('continuation').snapshot().strategy, null)
})

test('unsupported or failed declaration never reports a successful armed order', async () => {
  const { manager } = androidManager()
  for (const configure of [null, async () => { throw new Error('persistence refused') }]) {
    const registry = registryFor(manager, configure)
    await assert.rejects(registry.dispatch('continuation', 'declare', { onAppearance: 'native' }))
    assert.equal(registry.get('continuation').snapshot().strategy, null)
    assert.ok(!eventsOf(registry.get('continuation')).includes('continuation-declared'))
  }
})

test('every host lists the continuation scenario after restoration', () => {
  assert.deepEqual([...SCENARIO_IDS].slice(7, 9), ['restoration', 'continuation'])
  const { manager } = androidManager()
  const registry = registryFor(manager)
  assert.equal(registry.get('continuation').id, 'continuation')
})

test('declare records the standing order and reports each strategy capability verbatim', async () => {
  const { manager } = androidManager()
  const registry = registryFor(manager)
  const result = await registry.dispatch('continuation', 'declare', {
    onAppearance: 'native',
    peerId: 'A0:9E:1A:E9:B9:3D'
  })
  assert.equal(result.onAppearance, 'native')
  const scenario = registry.get('continuation')
  assert.equal(scenario.snapshot().strategy, 'native')
  assert.equal(scenario.snapshot().peerId, 'A0:9E:1A:E9:B9:3D')
  assert.deepEqual(scenario.snapshot().capabilities, {
    'background:wake-on-appearance': 'limited',
    'background:native-resubscribe': 'limited',
    'background:headless-task': 'unsupported',
    'background:wake-notification': 'unsupported'
  })
  assert.ok(eventsOf(scenario).includes('continuation-declared'))
  assert.ok(eventsOf(scenario).includes('continuation-capabilities'))
  await registry.dispatch('continuation', 'stop', {})
})

test('declare refuses an unknown strategy instead of a silent default', async () => {
  const { manager } = androidManager()
  const registry = registryFor(manager)
  await assert.rejects(registry.dispatch('continuation', 'declare', { onAppearance: 'auto-magic' }), {
    code: 'scenario.invalid-continuation'
  })
  await registry.dispatch('continuation', 'stop', {})
})

test('unregistered capabilities report unregistered instead of an invented answer', async () => {
  const { manager } = createFakeManager()
  const registry = registryFor(manager)
  await registry.dispatch('continuation', 'declare', { onAppearance: 'record-only' })
  const scenario = registry.get('continuation')
  assert.deepEqual(scenario.snapshot().capabilities, {
    'background:wake-on-appearance': 'unregistered',
    'background:native-resubscribe': 'unregistered',
    'background:headless-task': 'unregistered',
    'background:wake-notification': 'unregistered'
  })
  await registry.dispatch('continuation', 'stop', {})
})

test('status without a host continuation API reports unregistered verbatim', async () => {
  const { manager } = androidManager()
  const registry = registryFor(manager)
  const result = await registry.dispatch('continuation', 'status', {})
  assert.equal(result.state, 'unregistered')
  await registry.dispatch('continuation', 'stop', {})
})

test('status and backlog surface the host continuation API verbatim, with loss accounting', async () => {
  const { manager, calls } = androidManager({
    continuation: {
      status: {
        strategy: 'native',
        peerId: null,
        resubscribe: 1,
        malformedDeclarations: 0,
        lastWake: {
          observedAtMs: 12345,
          event: 'continuation.completed',
          strategy: 'native',
          peerAddress: 'A0:9E:1A:E9:B9:3D',
          code: null,
          reason: null
        }
      },
      claim: {
        values: [{ consumer: 'ubm-continuation-0', heartRate: 72 }],
        streamEnds: [{ consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }],
        afterCutoffLoss: { items: 1, bytes: 141 },
        recording: { id: 'retained_capture' },
        disposeFailure: null,
        controlLost: 0,
        disposed: true
      }
    }
  })
  const registry = registryFor(manager)
  const status = await registry.dispatch('continuation', 'status', {})
  assert.equal(status.lastWake.event, 'continuation.completed')
  const backlog = await registry.dispatch('continuation', 'backlog', {})
  assert.equal(backlog.values, 1)
  assert.equal(backlog.droppedItems, 5)
  assert.equal(backlog.controlLost, 0)
  assert.deepEqual(backlog.afterCutoffLoss, { items: 1, bytes: 141 })
  assert.deepEqual(backlog.recording, { id: 'retained_capture' })
  assert.equal(backlog.disposeFailure, null)
  assert.deepEqual(backlog.streamEnds, [{ consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }])
  assert.equal(JSON.stringify(backlog).includes('heartRate'), false)
  assert.ok(calls.includes('continuation.claim'))
  const scenario = registry.get('continuation')
  assert.ok(eventsOf(scenario).includes('continuation-backlog'))
  await registry.dispatch('continuation', 'stop', {})
})

test('backlog without a host continuation API reports the capability answer verbatim', async () => {
  const { manager } = androidManager({ continuation: 'unsupported' })
  const registry = registryFor(manager)
  const result = await registry.dispatch('continuation', 'backlog', {})
  assert.equal(result.state, 'capability-unsupported')
  await registry.dispatch('continuation', 'stop', {})
})
