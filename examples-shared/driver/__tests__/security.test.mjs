import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

const state = {
  bond: 'bonded',
  encryption: 'unknown',
  authentication: 'unknown',
  secureConnections: 'unknown',
  pairingPossible: true,
  measuredAtMonotonicMs: 7,
  limitations: []
}

function fixture() {
  const { manager, calls } = createFakeManager()
  manager.security = {
    state: async (peer, options) => {
      assert.equal(peer.id, 'peer-h10')
      assert.equal(options.signal.aborted, false)
      calls.push('security.state')
      return state
    },
    pair: async (peer, options) => {
      assert.equal(peer.id, 'peer-h10')
      assert.equal(options.ceremony, 'system')
      assert.equal(options.signal.aborted, false)
      calls.push('security.pair')
      return { outcome: 'already-paired', state }
    },
    cancelPairing: async peer => {
      assert.equal(peer.id, 'peer-h10')
      calls.push('security.cancel')
      return { outcome: 'cancelled' }
    },
    unpair: async peer => {
      assert.equal(peer.id, 'peer-h10')
      calls.push('security.unpair')
      return { outcome: 'unpaired' }
    },
    watch: peer => {
      assert.equal(peer.id, 'peer-h10')
      return {
        [Symbol.asyncIterator]() {
          return {
            next: async () => ({ done: false, value: { kind: 'state', peerId: peer.id, sequence: 1, state } }),
            return: async () => {
              calls.push('security.watch.return')
              return { done: true }
            }
          }
        }
      }
    }
  }
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  return { manager, calls, registry }
}

test('security scenario selects through ordinary host manager and preserves native state/pair/watch receipts', async () => {
  const { registry, calls } = fixture()
  await registry.dispatch('security', 'select', { device: 'SIM Polar H10*' })
  assert.deepEqual((await registry.dispatch('security', 'state', {})).result, state)
  assert.deepEqual((await registry.dispatch('security', 'pair', {})).result, { outcome: 'already-paired', state })
  const watched = await registry.dispatch('security', 'watch', { timeoutMs: 100, maxEvents: 1 })
  assert.equal(watched.events[0].state.measuredAtMonotonicMs, 7)
  assert.equal(calls.filter(call => call === 'security.watch.return').length, 1)
  assert.ok(!calls.some(call => call.startsWith('connect')))
  assert.ok(!calls.includes('security.unpair'))
  await registry.dispatch('security', 'stop', {})
  assert.ok(calls.includes('manager.destroy'))
  assert.ok(!calls.includes('security.unpair'))
})

test('security unpair requires an explicit confirmation; no destructive operation is implicit', async () => {
  const { registry, calls } = fixture()
  await assert.rejects(registry.dispatch('security', 'unpair', {}), { code: 'scenario.invalid-argument' })
  assert.deepEqual(calls, [])
  await registry.dispatch('security', 'select', {})
  assert.equal((await registry.dispatch('security', 'unpair', { confirm: true })).result.outcome, 'unpaired')
  assert.equal(calls.filter(call => call === 'security.unpair').length, 1)
  await registry.dispatch('security', 'stop', {})
})

test('security watch timeout/cancel closes the public iterator and keeps cleanup owned', async () => {
  for (const cancel of [false, true]) {
    const { manager, registry, calls } = fixture()
    let wake
    manager.security.watch = () => ({
      [Symbol.asyncIterator]() {
        return {
          next: () =>
            new Promise(resolve => {
              wake = resolve
            }),
          return: async () => {
            calls.push('security.watch.return')
            wake?.({ done: true })
            return { done: true }
          }
        }
      }
    })
    await registry.dispatch('security', 'select', {})
    const watching = registry.dispatch('security', 'watch', { timeoutMs: cancel ? 1000 : 1 })
    const rejected = assert.rejects(watching, {
      code: cancel ? 'operation.aborted' : 'scenario.security-watch-timeout'
    })
    if (cancel) {
      while (wake === undefined) await Promise.resolve()
      await registry.dispatch('security', 'stop', {})
    }
    await rejected
    assert.equal(calls.filter(call => call === 'security.watch.return').length, 1)
    await registry.dispatch('security', 'stop', {})
    assert.ok(calls.includes('manager.destroy'))
  }
})

test('security operation failures propagate original native detail and stop releases the manager', async () => {
  const { manager, registry, calls } = fixture()
  const failure = Object.assign(new Error('native pairing refusal'), { code: 'platform.failure' })
  manager.security.pair = async () => {
    throw failure
  }
  await registry.dispatch('security', 'select', {})
  await assert.rejects(registry.dispatch('security', 'pair', {}), error => error === failure)
  await registry.dispatch('security', 'stop', {})
  assert.ok(calls.includes('manager.destroy'))
})

test('failed watch cleanup stays owned and stop retries without deleting a bond', async () => {
  const { manager, registry, calls } = fixture()
  let attempts = 0
  manager.security.watch = () => ({
    [Symbol.asyncIterator]() {
      return {
        next: async () => ({ done: false, value: { kind: 'state', peerId: 'peer-h10', sequence: 1, state } }),
        return: async () => {
          attempts += 1
          if (attempts === 1) throw new Error('native watch cleanup refused')
          return { done: true }
        }
      }
    }
  })
  await registry.dispatch('security', 'select', {})
  await assert.rejects(registry.dispatch('security', 'watch', {}), /native watch cleanup refused/)
  await registry.dispatch('security', 'stop', {})
  assert.equal(attempts, 2)
  assert.ok(calls.includes('manager.destroy'))
  assert.ok(!calls.includes('security.unpair'))
})

test('a synchronous unsupported watch does not poison the next supported attempt', async () => {
  const { manager, registry } = fixture()
  const original = manager.security.watch
  manager.security.watch = () => {
    throw Object.assign(new Error('unsupported'), { code: 'capability.unsupported' })
  }
  await registry.dispatch('security', 'select', {})
  await assert.rejects(registry.dispatch('security', 'watch', {}), { code: 'capability.unsupported' })
  manager.security.watch = original
  assert.equal((await registry.dispatch('security', 'watch', {})).events.length, 1)
  await registry.dispatch('security', 'stop', {})
})

test('a terminal watch with no actual state event cannot pass acceptance', async () => {
  const { manager, registry } = fixture()
  manager.security.watch = () => ({
    [Symbol.asyncIterator]() {
      return { next: async () => ({ done: true }), return: async () => ({ done: true }) }
    }
  })
  await registry.dispatch('security', 'select', {})
  await assert.rejects(registry.dispatch('security', 'watch', {}), { code: 'scenario.security-watch-terminal' })
  await registry.dispatch('security', 'stop', {})
})

test('reference desktop trusted setup explicitly grants security scopes without changing library defaults', () => {
  const electron = fs.readFileSync(new URL('../../../example-electron/driver/main.cjs', import.meta.url), 'utf8')
  for (const permission of ['security:state', 'security:pair', 'security:cancel-pairing', 'security:unpair'])
    assert.ok(electron.includes(`'${permission}'`))
  const tauri = JSON.parse(
    fs.readFileSync(new URL('../../../example-tauri/src-tauri/capabilities/main.json', import.meta.url), 'utf8')
  )
  for (const permission of ['state', 'pair', 'cancel-pairing', 'unpair'])
    assert.ok(tauri.permissions.includes(`unified-ble-manager:allow-security-${permission}`))
})
