import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

test('public chooser retains one manager and connects only its selected scoped peer, without scanning', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser', peerName: 'SIM Polar H10 0001' })
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  const selected = await registry.dispatch('accessory-chooser', 'choose', {})
  assert.equal(selected.connection, 'not-requested')
  assert.equal(calls.filter(call => call.startsWith('choose')).length, 1)
  assert.equal(calls.filter(call => call.startsWith('connect')).length, 0)
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  assert.equal(calls.filter(call => call.startsWith('connect')).length, 1)
  assert.ok(!calls.some(call => call.startsWith('find')))
  const stopped = await registry.dispatch('accessory-chooser', 'cancel', {})
  assert.deepEqual(
    stopped.cleanup.map(step => step.step),
    ['connection.release', 'manager.destroy']
  )
  await assert.rejects(registry.dispatch('accessory-chooser', 'connect-selected', {}), {
    code: 'scenario.no-selected-peer'
  })
})

test('inactive app refuses chooser before opening a manager or system UI', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const host = createFakeHost({
    manager,
    appState: { current: () => ({ state: 'background', foreground: false }) },
    adapterHostManager: manager => ({ manager, prepare: async () => undefined })
  })
  const registry = createScenarioRegistry(host)
  await assert.rejects(registry.dispatch('accessory-chooser', 'choose', {}), { code: 'scenario.foreground-required' })
  assert.deepEqual(calls, [])
})

test('system setup does not demand generic scan permission/readiness before the chooser', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({
        manager,
        prepare: async () => {
          throw new Error('scan preparation must not precede system accessory setup')
        }
      })
    })
  )
  const selected = await registry.dispatch('accessory-chooser', 'choose', {})
  assert.equal(selected.connection, 'not-requested')
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('selected peer remains on the same hosted manager and obtains connection readiness only after selection', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  let authorized = false
  const order = []
  const originalConnect = manager.connect.bind(manager)
  manager.connect = (...arguments_) => {
    assert.equal(authorized, true, 'native connection must not precede host authorization')
    order.push('connect')
    return originalConnect(...arguments_)
  }
  const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager: manager => ({
    manager,
    prepare: async () => { authorized = true; order.push('prepare') }
  }) }))
  await registry.dispatch('accessory-chooser', 'choose', {})
  assert.equal(authorized, false)
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  assert.deepEqual(order, ['prepare', 'connect'])
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('cancel aborts a held public picker and releases its manager without returning a late selection', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  let entered
  let signal
  const started = new Promise(resolve => {
    entered = resolve
  })
  manager.choose = options =>
    new Promise((_, reject) => {
      signal = options.signal
      signal.addEventListener('abort', () => reject(new Error('picker aborted')), { once: true })
      entered()
    })
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  const choice = registry.dispatch('accessory-chooser', 'choose', {})
  const rejected = assert.rejects(choice, /picker aborted/)
  await started
  await registry.dispatch('accessory-chooser', 'cancel', {})
  await rejected
  assert.equal(signal.aborted, true)
  assert.ok(calls.includes('manager.destroy'))
  assert.equal(registry.get('accessory-chooser').snapshot().peer, null)
})

test('the actual Expo consumer declares ASK support/service/name allowlists, not merely a plugin source assertion', () => {
  const app = JSON.parse(fs.readFileSync(new URL('../../../example-expo/app.json', import.meta.url), 'utf8')).expo
  assert.deepEqual(app.ios.infoPlist.NSAccessorySetupKitSupports, ['Bluetooth'])
  assert.deepEqual(app.ios.infoPlist.NSAccessorySetupBluetoothServices, ['180D'])
  assert.deepEqual(app.ios.infoPlist.NSAccessorySetupBluetoothNames, ['SIM Polar H10'])
})
