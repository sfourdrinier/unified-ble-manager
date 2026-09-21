import { test } from 'node:test'
import assert from 'node:assert/strict'
import { SCENARIO_IDS, createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

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

function registryFor(manager) {
  return createScenarioRegistry(createFakeHost({ manager, adapterHostManager: m => ({ manager: m }) }))
}

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
