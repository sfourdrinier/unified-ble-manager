import { test } from 'node:test'
import assert from 'node:assert/strict'
import { BleScenario, IDLE_BLE_STATE } from '../scenarios/ble-scenario.ts'
import { createFakeRuntime } from './fake-runtime.mjs'

const released = { state: 'released', failures: [] }
const refused = { state: 'release-failed', failures: [{ code: 'platform.failure' }] }
class Probe extends BleScenario {
  id = 'ledger-probe'
  title = 'ledger'
  description = 'ledger'
  commands = {}
  constructor() { super({ runtime: createFakeRuntime() }, IDLE_BLE_STATE) }
  open() { return this.runJourney(async () => ({})) }
  add(step, release) { this.own(step, release) }
}

test('stop overlaps a late automatic release without duplicating its retained identity', async () => {
  const probe = new Probe()
  let finish
  let attempts = 0
  probe.add('late', () => {
    attempts++
    return attempts === 1 ? new Promise(resolve => { finish = resolve }) : Promise.resolve(released)
  })
  const first = probe.stop()
  const concurrent = probe.stop()
  await Promise.resolve()
  assert.equal(attempts, 1)
  finish(refused)
  for (const outcome of await Promise.all([first, concurrent])) {
    assert.equal(outcome.cleanup.length, 1)
    assert.equal(outcome.cleanup[0].state, 'release-failed')
  }
  const retry = await probe.stop()
  assert.equal(retry.cleanup.length, 1)
  assert.equal(retry.cleanup[0].state, 'released')
  assert.equal(attempts, 2)
  assert.deepEqual((await probe.stop()).cleanup, [])
})

for (const reject of [false, true]) {
  test(`cleanup retains only ${reject ? 'rejected' : 'refused'} identities and retries through stop`, async () => {
    const probe = new Probe()
    await probe.open()
    const calls = []
    let failing = true
    probe.add('failed', async () => { calls.push('failed'); if (failing && reject) throw new Error('refused'); return failing ? refused : released })
    probe.add('success', async () => { calls.push('success'); return released })
    const first = await probe.stop()
    assert.equal(first.cleanup.length, 2)
    await assert.rejects(probe.open(), { code: 'scenario.busy' })
    failing = false
    assert.equal((await probe.stop()).cleanup[0].state, 'released')
    assert.deepEqual(calls, ['success', 'failed', 'failed', 'failed'])
    assert.deepEqual((await probe.stop()).cleanup, [])
  })
}

test('held cleanup is single-flight, fences admission, and retains a late failed identity', async () => {
  const probe = new Probe()
  await probe.open()
  let finish
  let attempts = 0
  probe.add('held', () => { attempts++; return new Promise(resolve => { finish = resolve }) })
  const first = probe.stop()
  const second = probe.stop()
  await assert.rejects(probe.open(), { code: 'scenario.busy' })
  let lateAttempts = 0
  const observed = new Promise(resolve => {
    const unsubscribe = probe.subscribe(update => {
      if (update.type === 'event' && update.event.kind === 'cleanup' && update.event.data.step.includes('late')) { unsubscribe(); resolve() }
    })
  })
  probe.add('late', async () => { lateAttempts++; return lateAttempts === 1 ? refused : released })
  await observed
  finish(released)
  await Promise.all([first, second])
  assert.equal(attempts, 1)
  const retry = await probe.stop()
  assert.equal(retry.cleanup[0].step, 'late')
  assert.equal(lateAttempts, 2)
  assert.equal(attempts, 1)
})
