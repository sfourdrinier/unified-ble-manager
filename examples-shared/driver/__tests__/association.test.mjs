import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

function fixture(associate, destroy = async () => ({ state: 'released', failures: [] })) {
  const { manager } = createFakeManager()
  const calls = []
  const owned = { ...manager, destroy: async () => { calls.push('destroy'); return destroy() } }
  if (associate) owned.association = { associate: async request => { calls.push(request); return associate(request) } }
  const host = createFakeHost({ manager: owned, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  return { calls, runtime: host.runtime, registry: createScenarioRegistry(host) }
}

test('association deadline releases manager and reports a late accepted association without deleting it', async () => {
  let complete
  let entered
  const started = new Promise(resolve => { entered = resolve })
  const { registry, runtime, calls } = fixture(() => { entered(); return new Promise(resolve => { complete = resolve }) })
  const pending = registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' })
  await started
  const refusal = assert.rejects(pending, { code: 'operation.timeout' })
  runtime.advance(60000)
  await refusal
  assert.equal(calls.at(-1), 'destroy')
  const late = new Promise(resolve => {
    const unsubscribe = registry.get('restoration').subscribe(update => {
      if (update.type === 'event' && update.event.kind === 'association-late-result') { unsubscribe(); resolve() }
    })
  })
  complete({ source: 'associated', associationId: 12, peerId: '20:E1:5D:9E:A0:7F', displayName: null })
  await late
  assert.ok(registry.get('restoration').recentEvents().some(e => e.kind === 'association-late-result'))
  assert.equal(runtime.pendingTimers(), 0)
})

test('association uses exact supplied name and releases its manager after system consent', async () => {
  const answer = { source: 'associated', associationId: 12, peerId: '20:E1:5D:9E:A0:7F', displayName: 'SIM Polar H10 0001' }
  const { registry, calls } = fixture(async () => answer)
  assert.deepEqual(await registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' }), answer)
  assert.deepEqual(calls, [{ name: 'SIM Polar H10 0001' }, 'destroy'])
})

test('suspended JS timer reports elapsed deadline but preserves actual accepted association', async () => {
  let now = 1000
  const answer = { source: 'associated', associationId: 9, peerId: '20:e1:5d:9e:a0:7f', displayName: 'SIM Polar H10 0001' }
  const { registry, calls, runtime } = fixture(async () => { now += 65000; return answer })
  runtime.now = () => now
  const result = await registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' })
  assert.deepEqual(result, { ...answer, timing: { state: 'deadline-expired', budgetMs: 60000, elapsedMs: 65000, followUp: 'caller-decides' } })
  const late = registry.get('restoration').recentEvents().find(e => e.kind === 'association-late-result')
  assert.deepEqual(late.data.result, answer)
  assert.equal(late.data.followUp, 'caller-decides')
  assert.deepEqual(calls, [{ name: 'SIM Polar H10 0001' }, 'destroy'])
  assert.equal(runtime.pendingTimers(), 0)
})

for (const accepted of [true, false]) {
  test(`stopping a chooser preserves its eventual ${accepted ? 'acceptance' : 'refusal'} without inventing cancellation of OS work`, async () => {
    let complete, refuse, entered
    const started = new Promise(resolve => { entered = resolve })
    const { registry, calls } = fixture(() => { entered(); return new Promise((resolve, reject) => { complete = resolve; refuse = reject }) })
    const pending = registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' })
    await started
    const cancelled = assert.rejects(pending, { code: 'operation.cancelled' })
    await registry.dispatch('restoration', 'stop', {})
    await cancelled
    const late = new Promise(resolve => {
      const unsubscribe = registry.get('restoration').subscribe(update => {
        if (update.type === 'event' && update.event.kind === 'association-late-result') { unsubscribe(); resolve(update.event.data) }
      })
    })
    if (accepted) complete({ source: 'associated', associationId: 9, peerId: '20:e1:5d:9e:a0:7f', displayName: null })
    else refuse(Object.assign(new Error('OS refused'), { code: 'permission.denied' }))
    const observed = await late
    if (accepted) { assert.equal(observed.result.associationId, 9); assert.equal(observed.followUp, 'caller-decides') }
    else assert.equal(observed.error.code, 'permission.denied')
    assert.equal(calls.filter(call => call === 'destroy').length, 1)
  })
}

for (const code of ['operation.cancelled', 'permission.denied', 'capability.unsupported']) {
  test(`association preserves ${code} and releases its manager`, async () => {
    const error = Object.assign(new Error(code), { code })
    const { registry, calls } = fixture(async () => { throw error })
    await assert.rejects(registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' }), e => e === error)
    assert.equal(calls.at(-1), 'destroy')
  })
}

test('association refuses absent host API and invalid non-exact names', async () => {
  const { registry, calls } = fixture()
  for (const name of ['', 'SIM*', ' SIM', 'SIM ']) {
    await assert.rejects(registry.dispatch('restoration', 'associate', { name }), { code: 'scenario.invalid-argument' })
  }
  assert.deepEqual(calls, [])
  await assert.rejects(registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' }), { code: 'capability.unsupported' })
  assert.deepEqual(calls, ['destroy'])
})

for (const rejection of [false, true]) {
  test(`association cleanup ${rejection ? 'rejection' : 'failed receipt'} remains retryable through stop`, async () => {
    let failing = true
    const { registry, calls } = fixture(async () => ({ source: 'associated', associationId: 12, peerId: '20:E1:5D:9E:A0:7F', displayName: null }), async () => {
      if (failing && rejection) throw new Error('cleanup refused')
      return { state: failing ? 'release-failed' : 'released', failures: failing ? [{ code: 'platform.failure' }] : [] }
    })
    await assert.rejects(registry.dispatch('restoration', 'associate', { name: 'SIM Polar H10 0001' }), { code: 'scenario.cleanup-failed' })
    failing = false
    const stopped = await registry.stopAll()
    assert.equal(stopped.failures.length, 0)
    const attempts = calls.filter(x => x === 'destroy').length
    assert.ok(attempts >= 2)
    await registry.stopAll()
    assert.equal(calls.filter(x => x === 'destroy').length, attempts)
  })
}
