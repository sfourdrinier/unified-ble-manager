import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createHeadlessContinuationJob, registerHeadlessContinuation, installHeadlessContinuation, HEADLESS_CONTINUATION_TASK } from '../headless-continuation-job.ts'

function fixture() {
  const evidence = []
  const calls = []
  let expire
  const manager = {
    async destroy() { calls.push('destroy'); return { state: 'released', failures: [] } },
    async withDiscoveredConnection(peer, options, action) {
      calls.push(['connect', peer, options])
      return action({ gatt: { characteristic(service, characteristic) {
        calls.push(['characteristic', service, characteristic])
        return { async read(readOptions) { calls.push(['read', readOptions]); return Uint8Array.of(73) } }
      } } })
    }
  }
  const job = createHeadlessContinuationJob({ createManager: async () => manager, now: () => 1000,
    save: async value => evidence.push(value),
    scheduleDeadline: callback => { expire = callback; return () => calls.push('cancel-deadline') }
  })
  return { job, manager, evidence, calls, expire: () => expire() }
}
const payload = { peerId: 'AA:BB:CC:DD:EE:FF', event: 'companion.appeared' }

test('module reevaluation retains the original job and retries its failed manager cleanup', async () => {
  const scope = {}
  let provider
  let registrations = 0
  let destroyed = 0
  let created = 0
  const f = fixture()
  f.manager.destroy = async () => (++destroyed === 1 ? { state: 'release-failed', failures: [] } : { state: 'released', failures: [] })
  const host = { createManager: async () => { created++; return f.manager }, save: async () => {} }
  const registry = { registerHeadlessTask: (_name, value) => { registrations++; provider = value } }
  installHeadlessContinuation(registry, 'android', host, scope)
  await assert.rejects(provider()(payload))
  installHeadlessContinuation(registry, 'android', { ...host, createManager: async () => { throw new Error('replacement must not discard owner') } }, scope)
  await provider()(payload)
  assert.equal(registrations, 1)
  assert.equal(created, 2)
  assert.equal(destroyed, 3)
})

test('both Expo entry paths import registration before loading the UI', () => {
  const app = readFileSync(new URL('../../../App.tsx', import.meta.url), 'utf8')
  const entry = readFileSync(new URL('../../../index.js', import.meta.url), 'utf8')
  assert.ok(app.indexOf('register-headless-continuation') < app.indexOf("import React"))
  assert.ok(entry.indexOf("import './src/driver/register-headless-continuation'") < entry.indexOf("import 'expo/AppEntry'"))
})

test('Android registration uses the exact declared task and forwards the wake payload', async () => {
  let registered
  const calls = []
  const registry = { registerHeadlessTask: (name, provider) => { registered = { name, provider } } }
  registerHeadlessContinuation(registry, 'ios', async value => { calls.push(value) })
  assert.equal(registered, undefined)
  registerHeadlessContinuation(registry, 'android', async value => { calls.push(value) })
  assert.equal(registered.name, HEADLESS_CONTINUATION_TASK)
  await registered.provider()(payload)
  assert.deepEqual(calls, [payload])
})

test('registered task job reads only the known peer and persists completion after cleanup', async () => {
  assert.equal(HEADLESS_CONTINUATION_TASK, 'UBMContinuationWake')
  const f = fixture()
  await f.job(payload)
  assert.equal(f.calls[0][0], 'connect')
  assert.deepEqual(f.calls[0][1], { address: payload.peerId, addressType: 'public' })
  assert.equal(f.calls[0][2].timeoutMs, 15000)
  assert.equal(f.evidence.at(-1).state, 'completed')
  assert.equal(f.evidence.at(-1).cleanup.state, 'released')
  assert.equal(f.evidence.at(-1).batteryPercent, 73)
  assert.equal(f.calls.includes('destroy'), true)
})

test('invalid wake payload does not create radio work', async () => {
  const f = fixture()
  await assert.rejects(f.job({ peerId: '', event: 'other' }))
  assert.equal(f.calls.length, 0)
  assert.equal(f.evidence.at(-1).state, 'failed')
})

test('deadline reaches the active public operation and failure evidence includes cleanup', async () => {
  const f = fixture()
  f.manager.withDiscoveredConnection = async (_peer, options) => {
    f.expire()
    assert.equal(options.signal.aborted, true)
    throw new Error('deadline expired')
  }
  await assert.rejects(f.job(payload), /deadline expired/)
  assert.equal(f.evidence.at(-1).state, 'failed')
  assert.equal(f.evidence.at(-1).cleanup.state, 'released')
})

test('failed cleanup is retained and retried before another manager is admitted', async () => {
  const f = fixture()
  let releases = 0
  f.manager.destroy = async () => (++releases === 1 ? { state: 'release-failed', failures: [] } : { state: 'released', failures: [] })
  await assert.rejects(f.job(payload), /cleanup/)
  assert.equal(f.evidence.at(-1).cleanup.state, 'release-failed')
  await f.job(payload)
  assert.equal(releases, 3)
  assert.equal(f.evidence.at(-1).state, 'completed')
})

test('evidence storage failure is not reported as successful task completion', async () => {
  const job = createHeadlessContinuationJob({ createManager: async () => { throw new Error('must not create') }, save: async () => { throw new Error('storage refused') } })
  await assert.rejects(job(payload), /storage refused/)
})

test('rejected cleanup is retained and its failure is persisted, never a completed task', async () => {
  const f = fixture()
  let releases = 0
  f.manager.destroy = async () => {
    if (++releases === 1) throw new Error('destroy rejected')
    return { state: 'released', failures: [] }
  }
  await assert.rejects(f.job(payload), /destroy rejected/)
  assert.equal(f.evidence.at(-1).state, 'failed')
  assert.equal(f.evidence.at(-1).cleanup.state, 'rejected')
  await f.job(payload)
  assert.equal(releases, 3)
})

test('active plus pending JS jobs are bounded even when the first manager admission is held', async () => {
  let release
  let entered
  const held = new Promise(resolve => { release = resolve })
  const started = new Promise(resolve => { entered = resolve })
  const f = fixture()
  let creations = 0
  const job = createHeadlessContinuationJob({ now: () => 1000, save: async () => {},
    scheduleDeadline: () => () => {},
    createManager: async () => { creations += 1; entered(); await held; return f.manager }
  })
  const admitted = Array.from({ length: 4 }, () => job(payload))
  await started
  await assert.rejects(job(payload), /admission limit/)
  assert.equal(creations, 1)
  release()
  await Promise.all(admitted)
  await job(payload)
  assert.equal(creations, 5, 'settled tasks return admission capacity')
})
