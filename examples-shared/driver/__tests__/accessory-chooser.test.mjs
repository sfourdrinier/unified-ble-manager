import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { encodePeerReference } from 'unified-ble-manager'
import { createScenarioRegistry } from '../create-driver.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

const APPLE_ID = '23288D29-3C2B-4D84-9000-000000000001'
const APPLE_REFERENCE = { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_ID }

test('saved authorized selection uses the real directory without picker, scanning or global permission preparation', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const peer = {
    id: 'saved-accessory',
    name: 'example-expo',
    reference: APPLE_REFERENCE,
    sources: ['origin-authorized'],
    state: { connection: 'unknown' }
  }
  manager.peers.authorized = async function (options) {
    assert.equal(this, manager.peers)
    assert.equal(options.signal.aborted, false)
    assert.equal(options.timeoutMs, 1000)
    calls.push('peers.authorized')
    return [peer]
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      host: 'expo',
      platform: 'ios',
      adapterHostManager: manager => ({
        manager,
        prepare: async () => {
          calls.push('global.prepare')
        }
      })
    })
  )
  const result = await registry.dispatch('accessory-chooser', 'select-authorized', { timeoutMs: 1000 })
  assert.equal(result.peer.id, peer.id)
  assert.equal(result.connection, 'not-requested')
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  assert.ok(!calls.includes('global.prepare'))
  assert.ok(!calls.includes('choose'))
  assert.ok(!calls.some(call => call.startsWith('find')))
  await registry.dispatch('accessory-chooser', 'stop', {})
  assert.ok(calls.includes('manager.destroy'))
})

test('saved authorized selection refuses ambiguity and non-authorized identities rather than guessing', async () => {
  const valid = { id: 'saved', name: 'example-expo', reference: APPLE_REFERENCE, sources: ['origin-authorized'] }
  for (const peers of [
    [],
    [valid, { ...valid, id: 'other' }],
    [{ ...valid, reference: null }],
    [{ ...valid, sources: ['backend-cache'] }],
    [{ ...valid, reference: { ...APPLE_REFERENCE, scope: 'system' } }]
  ]) {
    const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
    manager.peers.authorized = async () => peers
    const registry = createScenarioRegistry(
      createFakeHost({
        manager,
        adapterHostManager: manager => ({ manager, prepare: async () => undefined })
      })
    )
    await assert.rejects(registry.dispatch('accessory-chooser', 'select-authorized', {}), {
      code: 'scenario.authorized-selection-unavailable'
    })
    assert.ok(!calls.includes('choose'))
    assert.ok(calls.includes('manager.destroy'))
  }
})

test('saved authorized selection forwards an exact persisted reference and rejects malformed input before allocation', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  manager.peers.authorized = async options => {
    assert.deepEqual(options.references, [APPLE_REFERENCE])
    return [{ id: 'saved', name: 'example-expo', reference: APPLE_REFERENCE, sources: ['origin-authorized'] }]
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await assert.rejects(registry.dispatch('accessory-chooser', 'select-authorized', { reference: 'invalid' }))
  assert.ok(!calls.includes('manager.destroy'))
  await registry.dispatch('accessory-chooser', 'select-authorized', { reference: encodePeerReference(APPLE_REFERENCE) })
  await registry.dispatch('accessory-chooser', 'stop', {})
})

test('saved authorized selection deadline releases its owner and observes a late query rejection', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  let rejectQuery
  manager.peers.authorized = () =>
    new Promise((_, reject) => {
      rejectQuery = reject
    })
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await assert.rejects(registry.dispatch('accessory-chooser', 'select-authorized', { timeoutMs: 1 }), {
    code: 'scenario.authorized-selection-timeout'
  })
  assert.ok(calls.includes('manager.destroy'))
  rejectQuery(new Error('late native directory refusal'))
  await assert.rejects(registry.dispatch('accessory-chooser', 'connect-selected', {}), {
    code: 'scenario.no-selected-peer'
  })
})

test('stopping saved authorization lookup rejects a late peer instead of adopting it', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  let resolveQuery
  let entered
  const started = new Promise(resolve => {
    entered = resolve
  })
  manager.peers.authorized = () => {
    entered()
    return new Promise(resolve => {
      resolveQuery = resolve
    })
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  const pending = registry.dispatch('accessory-chooser', 'select-authorized', {})
  const rejected = assert.rejects(pending, { code: 'operation.aborted' })
  await started
  await registry.dispatch('accessory-chooser', 'stop', {})
  resolveQuery([{ id: 'saved', name: 'example-expo', reference: APPLE_REFERENCE, sources: ['origin-authorized'] }])
  await rejected
  assert.ok(calls.includes('manager.destroy'))
  await assert.rejects(registry.dispatch('accessory-chooser', 'connect-selected', {}), {
    code: 'scenario.no-selected-peer'
  })
})

test('selected adapter state calls the actual selected manager receiver while setup is pending', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const original = manager.adapter.state
  manager.adapter.state = async function () {
    assert.equal(this, manager.adapter)
    calls.push('adapter.state')
    return original.call(this)
  }
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  await assert.rejects(registry.dispatch('accessory-chooser', 'selected-adapter-state', {}), {
    code: 'scenario.no-selected-peer'
  })
  await registry.dispatch('accessory-chooser', 'choose', {})
  assert.equal((await registry.dispatch('accessory-chooser', 'selected-adapter-state', {})).power, 'on')
  assert.equal(calls.filter(call => call === 'adapter.state').length, 1)
  assert.ok(!calls.some(call => call.startsWith('find')))
  await registry.dispatch('accessory-chooser', 'cancel', {})
  await assert.rejects(registry.dispatch('accessory-chooser', 'selected-adapter-state', {}), {
    code: 'scenario.no-selected-peer'
  })
})

test('selected adapter state remains available during held connection preparation', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  let releasePrepare
  let entered
  const enteredPrepare = new Promise(resolve => {
    entered = resolve
  })
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({
        manager,
        prepare: async () => {
          entered()
          await new Promise(resolve => {
            releasePrepare = resolve
          })
        }
      })
    })
  )
  await registry.dispatch('accessory-chooser', 'choose', {})
  const connecting = registry.dispatch('accessory-chooser', 'connect-selected', {})
  await enteredPrepare
  assert.equal((await registry.dispatch('accessory-chooser', 'selected-adapter-state', {})).power, 'on')
  releasePrepare()
  await connecting
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('selected reference uses same-manager connected directory and exact selected ID, without discovery', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  manager.choose = async () => ({
    id: 'scoped-selected',
    name: 'SIM Polar H10',
    reference: null,
    sources: ['origin-authorized']
  })
  manager.peers.connected = async function (options) {
    assert.equal(this, manager.peers)
    assert.equal(options.signal.aborted, false)
    assert.ok(options.timeoutMs > 0 && options.timeoutMs <= 1000)
    calls.push('peers.connected')
    return [
      { id: 'other', state: { connection: 'connected' }, reference: { ...APPLE_REFERENCE, opaqueId: 'OTHER' } },
      { id: 'scoped-selected', state: { connection: 'connected' }, reference: APPLE_REFERENCE }
    ]
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      host: 'expo',
      platform: 'ios',
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await registry.dispatch('accessory-chooser', 'choose', {})
  await assert.rejects(registry.dispatch('accessory-chooser', 'selected-reference', {}), {
    code: 'scenario.no-selected-peer'
  })
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  const result = await registry.dispatch('accessory-chooser', 'selected-reference', { timeoutMs: 1000 })
  assert.equal(result.peerId, 'scoped-selected')
  assert.deepEqual(result.reference, APPLE_REFERENCE)
  assert.equal(result.nativePeerId, APPLE_ID)
  assert.equal(calls.filter(call => call === 'peers.connected').length, 1)
  assert.ok(!calls.some(call => call.startsWith('find')))
  await registry.dispatch('accessory-chooser', 'cancel', {})
  await assert.rejects(registry.dispatch('accessory-chooser', 'selected-reference', {}), {
    code: 'scenario.no-selected-peer'
  })
})

test('selected reference refuses absent, duplicated, disconnected, and wrong-origin records without altering ownership', async () => {
  for (const records of [
    [],
    [{ id: 'other', state: { connection: 'connected' }, reference: APPLE_REFERENCE }],
    Array.from({ length: 2 }, () => ({
      id: 'peer-h10',
      state: { connection: 'connected' },
      reference: APPLE_REFERENCE
    })),
    [{ id: 'peer-h10', state: { connection: 'disconnected' }, reference: APPLE_REFERENCE }],
    [{ id: 'peer-h10', state: { connection: 'connected' }, reference: { ...APPLE_REFERENCE, scope: 'system' } }],
    [{ id: 'peer-h10', state: { connection: 'connected' }, reference: { ...APPLE_REFERENCE, backendId: 'other' } }],
    [{ id: 'peer-h10', state: { connection: 'connected' }, reference: { ...APPLE_REFERENCE, opaqueId: 'not-a-uuid' } }]
  ]) {
    const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
    manager.peers.connected = async () => records
    const registry = createScenarioRegistry(
      createFakeHost({
        manager,
        host: 'expo',
        platform: 'ios',
        adapterHostManager: manager => ({ manager, prepare: async () => undefined })
      })
    )
    await registry.dispatch('accessory-chooser', 'choose', {})
    await registry.dispatch('accessory-chooser', 'connect-selected', {})
    await assert.rejects(registry.dispatch('accessory-chooser', 'selected-reference', {}), {
      code: 'scenario.peer-reference-unavailable'
    })
    assert.ok(!calls.includes('connection.release'))
    assert.ok(!calls.includes('manager.destroy'))
    await registry.dispatch('accessory-chooser', 'cancel', {})
  }
})

test('held selected-reference directory query has a finite deadline and preserves late rejection', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  let rejectQuery
  manager.peers.connected = () =>
    new Promise((_, reject) => {
      rejectQuery = reject
    })
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      host: 'expo',
      platform: 'ios',
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await registry.dispatch('accessory-chooser', 'choose', {})
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  await assert.rejects(registry.dispatch('accessory-chooser', 'selected-reference', { timeoutMs: 1 }), {
    code: 'scenario.peer-reference-timeout'
  })
  rejectQuery(new Error('late directory refusal'))
  assert.ok(!calls.includes('connection.release'))
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('stopping during a selected-reference query refuses a late matching record', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  let resolveQuery
  let entered
  const started = new Promise(resolve => {
    entered = resolve
  })
  manager.peers.connected = () => {
    entered()
    return new Promise(resolve => {
      resolveQuery = resolve
    })
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      host: 'expo',
      platform: 'ios',
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await registry.dispatch('accessory-chooser', 'choose', {})
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  const query = registry.dispatch('accessory-chooser', 'selected-reference', {})
  const rejected = assert.rejects(query, { code: 'operation.aborted' })
  await started
  await registry.dispatch('accessory-chooser', 'cancel', {})
  resolveQuery([{ id: 'peer-h10', state: { connection: 'connected' }, reference: APPLE_REFERENCE }])
  await rejected
})

test('Android selected origin reference forwards only a native address from the same backend', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  const reference = {
    version: 1,
    backendId: 'unified-ble:react-native-android',
    scope: 'origin',
    opaqueId: 'AA:BB:CC:DD:EE:FF'
  }
  manager.peers.connected = async () => [{ id: 'peer-h10', state: { connection: 'connected' }, reference }]
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      host: 'expo',
      platform: 'android',
      adapterHostManager: manager => ({ manager, prepare: async () => undefined })
    })
  )
  await registry.dispatch('accessory-chooser', 'choose', {})
  await registry.dispatch('accessory-chooser', 'connect-selected', {})
  const result = await registry.dispatch('accessory-chooser', 'selected-reference', {})
  assert.equal(result.nativePeerId, 'AA:BB:CC:DD:EE:FF')
  assert.deepEqual(result.reference, reference)
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

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
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({
        manager,
        prepare: async () => {
          authorized = true
          order.push('prepare')
        }
      })
    })
  )
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
  assert.deepEqual(app.ios.infoPlist.NSAccessorySetupBluetoothCompanyIdentifiers, ['006B'])
  const profile = JSON.parse(
    fs.readFileSync(new URL('../../../tool/h10-sim/profiles/stock-h10.json', import.meta.url), 'utf8')
  )
  assert.equal(
    Number.parseInt(app.ios.infoPlist.NSAccessorySetupBluetoothCompanyIdentifiers[0], 16),
    profile.advertising.manufacturer_company
  )
  assert.equal(profile.advertising.manufacturer_payload_hex, '3f155252')
})

test('manufacturer arguments forward public bytes conjunctively without replacing service/name', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  const original = manager.choose.bind(manager)
  let options
  manager.choose = input => {
    options = input
    return original(input)
  }
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  await registry.dispatch('accessory-chooser', 'choose', {
    manufacturerCompanyIdentifier: 107,
    manufacturerPrefix: [63, 21, 82, 82]
  })
  assert.equal(options.filters[0].localNamePrefix, 'SIM Polar H10')
  assert.equal(options.filters[0].serviceUuids.length, 1)
  assert.deepEqual(options.filters[0].manufacturerData, [
    { companyIdentifier: 107, dataPrefix: new Uint8Array([63, 21, 82, 82]) }
  ])
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('OR alternatives append strict public filters without replacing the default conjunction', async () => {
  const { manager } = createFakeManager({ discovery: 'system-chooser' })
  const original = manager.choose.bind(manager)
  let options
  manager.choose = input => {
    options = input
    return original(input)
  }
  const registry = createScenarioRegistry(
    createFakeHost({ manager, adapterHostManager: manager => ({ manager, prepare: async () => undefined }) })
  )
  await registry.dispatch('accessory-chooser', 'choose', {
    alternativeFilters: [
      {
        serviceUuids: ['180d'],
        localNamePrefix: 'SIM Polar H10 Other',
        manufacturerCompanyIdentifier: 107,
        manufacturerPrefix: [63]
      }
    ]
  })
  assert.equal(options.filters.length, 2)
  assert.equal(options.filters[0].localNamePrefix, 'SIM Polar H10')
  assert.deepEqual(options.filters[1], {
    serviceUuids: ['180d'],
    localNamePrefix: 'SIM Polar H10 Other',
    manufacturerData: [{ companyIdentifier: 107, dataPrefix: new Uint8Array([63]) }]
  })
  await registry.dispatch('accessory-chooser', 'cancel', {})
})

test('malformed alternative JSON refuses before manager allocation', async () => {
  for (const alternativeFilters of [
    null,
    {},
    [null],
    [[]],
    [{}],
    [{ unknown: 1 }],
    [{ serviceUuids: '180d' }],
    [{ serviceUuids: [true] }],
    [{ localNamePrefix: 7 }],
    [{ manufacturerPrefix: [1] }]
  ]) {
    const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
    const registry = createScenarioRegistry(createFakeHost({ manager }))
    await assert.rejects(registry.dispatch('accessory-chooser', 'choose', { alternativeFilters }), {
      code: 'scenario.invalid-argument'
    })
    assert.deepEqual(calls, [])
  }
})

test('explicit inactive probe reaches the same public manager and retains actual refusal', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const refusal = new Error('native foreground required')
  manager.choose = async () => {
    calls.push('choose:inactive-native')
    throw refusal
  }
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({ manager, prepare: async () => undefined }),
      appState: { current: () => ({ state: 'background', foreground: false }) }
    })
  )
  await assert.rejects(
    registry.dispatch('accessory-chooser', 'probe-native-inactive-refusal', {}),
    error => error === refusal
  )
  assert.ok(calls.includes('choose:inactive-native'))
  assert.ok(calls.includes('manager.destroy'))
})

test('inactive probe rejects active/unknown state before allocation', async () => {
  for (const appState of [
    undefined,
    { current: () => ({ state: 'active', foreground: true }) },
    { current: () => ({ state: 'unknown', foreground: null }) }
  ]) {
    const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
    const registry = createScenarioRegistry(createFakeHost({ manager, appState }))
    await assert.rejects(registry.dispatch('accessory-chooser', 'probe-native-inactive-refusal', {}), {
      code: 'scenario.inactive-required'
    })
    assert.deepEqual(calls, [])
  }
})

test('inactive probe never converts unexpected selection into a refusal and releases its owner', async () => {
  const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
  const registry = createScenarioRegistry(
    createFakeHost({
      manager,
      adapterHostManager: manager => ({ manager, prepare: async () => undefined }),
      appState: { current: () => ({ state: 'background', foreground: false }) }
    })
  )
  await assert.rejects(registry.dispatch('accessory-chooser', 'probe-native-inactive-refusal', {}), {
    code: 'scenario.expected-refusal'
  })
  assert.ok(calls.some(call => call.startsWith('choose')))
  assert.ok(calls.includes('manager.destroy'))
  assert.equal(registry.get('accessory-chooser').snapshot().peer, null)
})

test('malformed or unpaired manufacturer arguments refuse before manager allocation', async () => {
  for (const input of [
    { manufacturerCompanyIdentifier: 107 },
    { manufacturerPrefix: [1] },
    { manufacturerCompanyIdentifier: -1, manufacturerPrefix: [1] },
    { manufacturerCompanyIdentifier: 65536, manufacturerPrefix: [1] },
    { manufacturerCompanyIdentifier: 1.5, manufacturerPrefix: [1] },
    { manufacturerCompanyIdentifier: '107', manufacturerPrefix: [1] },
    ...[null, [], '3f', [-1], [256], [1.5], ['1']].map(manufacturerPrefix => ({
      manufacturerCompanyIdentifier: 107,
      manufacturerPrefix
    }))
  ]) {
    const { manager, calls } = createFakeManager({ discovery: 'system-chooser' })
    const registry = createScenarioRegistry(createFakeHost({ manager }))
    await assert.rejects(registry.dispatch('accessory-chooser', 'choose', input), { code: 'scenario.invalid-argument' })
    assert.deepEqual(calls, [], JSON.stringify(input))
  }
})
