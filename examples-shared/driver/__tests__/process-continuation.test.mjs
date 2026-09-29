import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

function registryFixture(controller) {
  const { manager } = createFakeManager()
  return createScenarioRegistry({ ...createFakeHost({ manager }),
    createManager: async () => { throw new Error('must not acquire another manager') },
    nativeContinuation: controller === undefined ? undefined : { status: async () => null, ...controller }
  })
}
function fixture(controller) { return registryFixture(controller).get('process-continuation') }
const declaration = { peerId: 'exact-peer', measurements: 'hr-ecg-acc', recordingId: 'capture', maxBytes: 1048576, maxRecords: 1000 }
const backlog = disposed => ({ values: [{ value: new Uint8Array([0, 72]) }], disposed, afterCutoffLoss: { items: 1, bytes: 2 }, disposeFailure: disposed ? null : { code: 'release-failed' } })

for (const fails of [false, true]) test(`claim then execute projects pending cleanup obligation through ${fails ? 'failure' : 'success'}`, async () => {
  let entered, finish
  const admitted = new Promise(resolve => { entered = resolve })
  const pending = new Promise(resolve => { finish = resolve })
  const failure = new Error('execute refused')
  const scenario = fixture({ execute: async () => { entered(); await pending; if (fails) throw failure; return { event: 'continuation.completed' } }, claim: async () => backlog(true) })
  await scenario.dispatch('claim', {})
  assert.equal(scenario.snapshot().owned, false)
  const executing = scenario.dispatch('execute', declaration)
  await admitted
  assert.equal(scenario.snapshot().owned, true, 'pending execute requires cleanup, not proof of native acquisition')
  await scenario.dispatch('status', {})
  assert.equal(scenario.snapshot().owned, true, 'concurrent null status must not erase pending obligation')
  finish()
  if (fails) await assert.rejects(executing, error => error === failure)
  else await executing
  assert.equal(scenario.snapshot().owned, true)
  await scenario.dispatch('claim', {})
  assert.equal(scenario.snapshot().owned, false)
})

test('fresh renderer does not project authoritative absence before any query', () => {
  assert.equal(fixture({}).snapshot().owned, null)
})

test('pending and rejected claim retain visible cleanup obligation, including a fresh renderer', async () => {
  const failure = new Error('claim refused')
  let entered, finish
  const admitted = new Promise(resolve => { entered = resolve })
  const pending = new Promise(resolve => { finish = resolve })
  let calls = 0
  const scenario = fixture({ claim: async () => { if (++calls === 1) { entered(); await pending; throw failure }; return backlog(true) } })
  const claiming = scenario.dispatch('claim', {})
  await admitted
  assert.equal(scenario.snapshot().owned, true)
  finish()
  await assert.rejects(claiming, error => error === failure)
  assert.equal(scenario.snapshot().owned, true)
  await scenario.dispatch('claim', {})
  assert.equal(scenario.snapshot().owned, false)
})

test('process continuation uses one host controller and preserves recipe, bytes and failed cleanup for retry', async () => {
  let calls = 0
  let claims = 0
  const scenario = fixture({ execute: async value => { calls++; assert.equal(value.peerId, 'exact-peer'); assert.equal(value.setup.length, 4); assert.equal(value.recording.id, 'capture'); return { event: 'continuation.completed' } },
    claim: async () => backlog(++claims > 1) })
  await scenario.dispatch('execute', declaration)
  await assert.rejects(scenario.dispatch('execute', declaration), error => error.code === 'scenario.busy')
  const first = await scenario.dispatch('claim', {})
  assert.equal(first.disposed, false)
  assert.equal(first.values[0].value, '0048')
  assert.equal((await scenario.stop()).cleanup[0].state, 'released')
  assert.equal(calls, 1)
  assert.equal(claims, 2)
  assert.equal((await scenario.stop()).cleanup.length, 0)
})

test('failed execute retains cleanup obligation, concurrent acquisition refused, original error survives', async () => {
  let finish
  const pending = new Promise(resolve => { finish = resolve })
  const failure = new Error('native refusal')
  const scenario = fixture({ execute: async () => { await pending; throw failure }, claim: async () => backlog(true) })
  const first = scenario.dispatch('execute', declaration)
  await assert.rejects(scenario.dispatch('execute', declaration), error => error.code === 'scenario.busy')
  finish()
  await assert.rejects(first, error => error === failure)
  assert.equal((await scenario.stop()).cleanup[0].state, 'released')
})

test('offline recording controls never execute or auto acknowledge and keep payload out of history', async () => {
  const operations = []
  const records = { token: 'token', records: [{ body: 'sensitive' }], bytes: 10, more: false }
  const scenario = fixture({ recordings: async () => ({
    prepare: async () => { operations.push('prepare'); return records },
    acknowledge: async (id, token) => { operations.push([id, token]); return { acknowledged: true } }
  }) })
  assert.deepEqual(await scenario.dispatch('recording-prepare', { recordingId: 'capture' }), records)
  assert.deepEqual(operations, ['prepare'])
  assert.equal(JSON.stringify(scenario.recentEvents()).includes('sensitive'), false)
  await scenario.dispatch('recording-acknowledge', { recordingId: 'capture', token: 'token' })
  assert.deepEqual(operations, ['prepare', ['capture', 'token']])
  assert.equal((await scenario.stop()).cleanup.length, 0)
})

test('missing host mechanism and missing explicit peer refuse without effects', async () => {
  const scenario = fixture(undefined)
  await assert.rejects(scenario.dispatch('status', {}), error => error.code === 'capability.unsupported')
  await assert.rejects(scenario.dispatch('execute', { measurements: 'hr' }), error => error.code === 'scenario.invalid-continuation')
})

test('fresh renderer stop consults existing process authority and retries failed disposal without executing', async () => {
  let disposed = false
  let calls = 0
  const scenario = fixture({ status: async () => disposed ? null : { queuedData: 1 },
    claim: async () => { calls++; disposed = calls > 1; return backlog(disposed) } })
  const first = await scenario.stop()
  assert.equal(first.cleanup[0].state, 'release-failed')
  assert.equal((await scenario.dispatch('last-claim', {})).values[0].value, '0048')
  assert.equal((await scenario.stop()).cleanup[0].state, 'released')
  assert.equal((await scenario.stop()).cleanup.length, 0)
  assert.equal(calls, 2)
})

test('stop waits actual held execute, coalesces cleanup, and never converts claim rejection to release', async () => {
  let finish
  const pending = new Promise(resolve => { finish = resolve })
  const failure = new Error('cleanup refused')
  let claims = 0
  const scenario = fixture({ execute: async () => { await pending; return { event: 'continuation.completed' } },
    claim: async () => { if (++claims === 1) throw failure; return backlog(true) } })
  const execute = scenario.dispatch('execute', declaration)
  const stop1 = scenario.stop()
  const stop2 = scenario.stop()
  assert.equal(claims, 0)
  finish()
  await execute
  await assert.rejects(stop1, error => error === failure)
  await assert.rejects(stop2, error => error === failure)
  assert.equal(claims, 1)
  assert.equal((await scenario.stop()).cleanup[0].state, 'released')
})

test('all explicit offline controls remain available without a process session', async () => {
  const calls = []
  const scenario = fixture({ status: async () => { throw new Error('offline controls must not query BLE') }, recordings: async () => ({
    status: async id => { calls.push(['status', id]); return { phase: 'stopped' } },
    stop: async id => { calls.push(['stop', id]); return { stopped: true } },
    clear: async id => { calls.push(['clear', id]); return { cleared: true } }
  }) })
  for (const op of ['status', 'stop', 'clear']) await scenario.dispatch(`recording-${op}`, { recordingId: 'capture' })
  assert.deepEqual(calls, [['status', 'capture'], ['stop', 'capture'], ['clear', 'capture']])
})

test('invalid recording quotas refuse before native execution', async () => {
  const scenario = fixture({ execute: async () => { throw new Error('must not execute') } })
  for (const overrides of [{ maxBytes: 1 }, { maxRecords: 1.5 }, { recordingId: '../escape' }, { maxRecords: undefined }, { measurements: null }]) {
    await assert.rejects(scenario.dispatch('execute', { ...declaration, ...overrides }), error => error.code === 'scenario.invalid-continuation')
  }
})

test('automatic registry shutdown returns exact handed-off bytes even when disposal fails', async () => {
  for (const disposed of [false, true]) {
    const registry = registryFixture({ status: async () => ({ queuedData: 1 }), claim: async () => backlog(disposed) })
    let report
    if (disposed) report = await registry.stopAll()
    else {
      await assert.rejects(registry.stopAll(), error => {
        report = error.report
        return error.name === 'StopAllError'
      })
    }
    const entry = report.scenarios.find(value => value.scenario === 'process-continuation')
    assert.equal(entry.cleanup[0].detail.values[0].value, '0048')
    assert.equal(entry.cleanup[0].detail.disposed, disposed)
    assert.equal(entry.cleanup[0].state, disposed ? 'released' : 'release-failed')
    assert.equal(JSON.stringify(registry.get('process-continuation').recentEvents()).includes('0048'), false)
  }
})

test('empty idle claim consults actual owner, without manufacturing release or hiding a live empty session', async () => {
  const empty = { selectors: [], values: [], streamEnds: [], control: [], controlLost: 0,
    afterCutoffLoss: { items: 0, bytes: 0 }, disposed: false, disposeFailure: null }
  let statuses = 0
  const idle = fixture({ status: async () => { statuses++; return null }, claim: async () => empty })
  assert.deepEqual(await idle.dispatch('claim', {}), empty)
  assert.equal(statuses, 1)
  assert.deepEqual(await idle.stop(), { wasRunning: false, cleanup: [] })
  const live = fixture({ status: async () => ({ queuedData: 0 }), claim: async () => empty })
  assert.deepEqual(await live.dispatch('claim', {}), empty)
  assert.equal((await live.stop()).cleanup[0].state, 'release-failed')
  const failure = new Error('status unavailable')
  const uncertain = fixture({ status: async () => { throw failure }, claim: async () => empty })
  await assert.rejects(uncertain.dispatch('claim', {}), error => error === failure)
  assert.deepEqual(await uncertain.dispatch('last-claim', {}), empty)
  await assert.rejects(uncertain.stop(), error => error === failure)
})
