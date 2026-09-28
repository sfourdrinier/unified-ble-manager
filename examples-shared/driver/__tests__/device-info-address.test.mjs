import test from 'node:test'
import assert from 'node:assert/strict'
import { BleError } from 'unified-ble-manager'
import { createDeterministicTestBleManager } from 'unified-ble-manager/testing'
import { createScenarioRegistry } from '../create-driver.ts'
import { adapterHostManager } from '../host.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

function fixture(options = {}) {
  const fake = createFakeManager(options)
  const connections = []
  const original = fake.manager.connect
  fake.manager.connect = async (target, controls) => {
    connections.push({ target, controls })
    if (typeof target === 'string') {
      throw new BleError('peer.not-found', 'connection', 'fixture.connect', { retryability: 'never' })
    }
    const connection = await original(target, controls)
    return { ...connection, peer: { ...connection.peer, id: 'actual-connection-peer' } }
  }
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  return { ...fake, registry, connections }
}

test('address read passes the explicit public target without discovery, reports actual peer, and releases both owners', async () => {
  const { registry, calls, connections } = fixture({
    reads: { '00002a19-0000-1000-8000-00805f9b34fb': new Uint8Array([83]) }
  })
  const result = await registry.dispatch('device-info', 'read', { peerAddress: { address: 'aa-bb-cc-dd-ee-ff' } })
  assert.equal(connections.length, 1)
  assert.deepEqual(connections[0].target, { address: 'AA:BB:CC:DD:EE:FF', addressType: 'public' })
  assert.equal(connections[0].controls.timeoutMs, 20000)
  assert.ok(connections[0].controls.signal instanceof AbortSignal)
  assert.equal(connections[0].controls.intent, 'direct')
  assert.ok(!calls.some(call => /^(find|scan|choose)\b/.test(call)))
  assert.equal(result.peer.id, 'actual-connection-peer')
  assert.equal(result.peer.query, null)
  assert.equal(result.reads.batteryLevelPercent.value, 83)
  assert.deepEqual(calls.slice(-2), ['connection.release', 'manager.destroy'])
  assert.deepEqual(
    registry
      .get('device-info')
      .snapshot()
      .cleanup.map(row => row.state),
    ['released', 'released']
  )
})

test('invalid/conflicting address inputs and the removed manager-local ID route fail before manager creation', async () => {
  for (const raw of [
    { peerId: 'AA:BB:CC:DD:EE:FF' },
    { peerAddress: '' },
    { peerAddress: null },
    { peerAddress: 1 },
    { peerAddress: [] },
    { peerAddress: {} },
    { peerAddress: { address: null } },
    { peerAddress: { address: 1 } },
    { peerAddress: { address: '' } },
    { peerAddress: { address: 'not-an-address' } },
    { peerAddress: { address: ' AA:BB:CC:DD:EE:FF' } },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF', addressType: null } },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF', addressType: 'opaque' } },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF', extra: true } },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF' }, device: 'Polar H10*' },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF' }, device: null },
    { peerAddress: { address: 'AA:BB:CC:DD:EE:FF' }, peerId: 'p' }
  ]) {
    const { registry, calls, connections } = fixture()
    await assert.rejects(registry.dispatch('device-info', 'read', raw), { code: 'scenario.invalid-argument' })
    assert.deepEqual(calls, [])
    assert.deepEqual(connections, [])
    assert.equal(registry.get('device-info').snapshot().peer, null)
  }
})

test('typed address-targeting backend refusal is preserved, with manager released and no scan fallback', async () => {
  const refusal = new BleError('capability.unsupported', 'capability', 'connection.connect', {
    retryability: 'never',
    platform: { domain: 'fixture', code: 'address-refused', safeMessage: 'unsupported', metadata: {} }
  })
  const { registry, calls, connections } = fixture({ connectFailures: [refusal] })
  await assert.rejects(registry.dispatch('device-info', 'read', { peerAddress: { address: 'AA:BB:CC:DD:EE:FF' } }), error => error === refusal)
  assert.equal(connections.length, 1)
  assert.deepEqual(connections[0].target, { address: 'AA:BB:CC:DD:EE:FF', addressType: 'public' })
  assert.deepEqual(calls.slice(-1), ['manager.destroy'])
  assert.ok(!calls.some(call => /^(find|scan|choose)\b/.test(call)))
  assert.equal(registry.get('device-info').snapshot().error.detail.platform.code, 'address-refused')
})

test('ordinary device-name selection still finds and passes the discovered peer', async () => {
  const { registry, calls, connections } = fixture()
  await registry.dispatch('device-info', 'read', { device: 'Polar H10 1234' })
  assert.ok(calls.some(call => call.startsWith('find ')))
  assert.equal(connections[0].target.id, 'peer-h10')
})

test('random address uses the existing explicit single retry without discovery or reinterpretation', async () => {
  const transient = new BleError('connection.failed', 'connection', 'connection.connect', {
    retryability: 'caller-decides'
  })
  const { registry, calls, connections } = fixture({ connectFailures: [transient] })
  await registry.dispatch('device-info', 'read', { peerAddress: { address: 'AA:BB:CC:DD:EE:FF', addressType: 'random' } })
  assert.deepEqual(
    connections.map(row => row.target),
    [{ address: 'AA:BB:CC:DD:EE:FF', addressType: 'random' }, { address: 'AA:BB:CC:DD:EE:FF', addressType: 'random' }]
  )
  assert.ok(connections.every(row => row.controls.timeoutMs === 20000 && row.controls.intent === 'direct'))
  assert.ok(!calls.some(call => /^(find|scan|choose)\b/.test(call)))
  const retry = registry
    .get('device-info')
    .recentEvents()
    .find(event => event.kind === 'connect-retry')
  assert.equal(retry.data.error.code, 'connection.failed')
  assert.deepEqual(calls.slice(-2), ['connection.release', 'manager.destroy'])
})

test('semantic fixture refuses an unknown manager-local string rather than interpreting it as an address', async () => {
  const { manager, connections } = fixture()
  await assert.rejects(manager.connect('AA:BB:CC:DD:EE:FF', { intent: 'direct' }), { code: 'peer.not-found' })
  assert.equal(connections[0].target, 'AA:BB:CC:DD:EE:FF')
})

test('actual public manager keeps unsupported address targeting as a typed refusal, not a scan fallback', async () => {
  const { manager } = await createDeterministicTestBleManager()
  try {
    const registry = createScenarioRegistry(createFakeHost({ manager, adapterHostManager }))
    await assert.rejects(
      registry.dispatch('device-info', 'read', { peerAddress: { address: 'AA:BB:CC:DD:EE:FF' } }),
      { code: 'capability.unsupported' }
    )
  } finally {
    await manager.destroy()
  }
})
