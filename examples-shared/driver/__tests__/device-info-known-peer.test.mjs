import test from 'node:test'
import assert from 'node:assert/strict'
import { BleError } from 'unified-ble-manager'
import { createScenarioRegistry } from '../create-driver.ts'
import { adapterHostManager } from '../host.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

function fixture(options = {}) {
  const fake = createFakeManager(options)
  const connections = []
  const original = fake.manager.connect
  fake.manager.connect = async (target, controls) => {
    connections.push({ target, controls })
    const connection = await original(target, controls)
    return { ...connection, peer: { ...connection.peer, id: 'actual-connection-peer' } }
  }
  const registry = createScenarioRegistry(createFakeHost({ manager: fake.manager, adapterHostManager }))
  return { ...fake, registry, connections }
}

test('known peer read forwards exact string without discovery, reports actual peer, and releases both owners', async () => {
  const { registry, calls, connections } = fixture({
    reads: { '00002a19-0000-1000-8000-00805f9b34fb': new Uint8Array([83]) }
  })
  const result = await registry.dispatch('device-info', 'read', { peerId: 'AA:BB:CC:DD:EE:FF' })
  assert.equal(connections.length, 1)
  assert.equal(connections[0].target, 'AA:BB:CC:DD:EE:FF')
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

test('invalid/conflicting known peer inputs fail before manager creation', async () => {
  for (const raw of [
    { peerId: '' },
    { peerId: '  ' },
    { peerId: null },
    { peerId: 1 },
    { peerId: [] },
    { peerId: 'p', device: 'Polar H10*' },
    { peerId: 'p', device: null }
  ]) {
    const { registry, calls, connections } = fixture()
    await assert.rejects(registry.dispatch('device-info', 'read', raw), { code: 'scenario.invalid-argument' })
    assert.deepEqual(calls, [])
    assert.deepEqual(connections, [])
    assert.equal(registry.get('device-info').snapshot().peer, null)
  }
})

test('typed known peer backend refusal is preserved, with manager released and no scan fallback', async () => {
  const refusal = new BleError('capability.unsupported', 'capability', 'connection.connect', {
    retryability: 'never',
    platform: { domain: 'fixture', code: 'known-peer-refused', safeMessage: 'unsupported', metadata: {} }
  })
  const { registry, calls, connections } = fixture({ connectFailures: [refusal] })
  await assert.rejects(registry.dispatch('device-info', 'read', { peerId: 'known-peer' }), error => error === refusal)
  assert.equal(connections.length, 1)
  assert.equal(connections[0].target, 'known-peer')
  assert.deepEqual(calls.slice(-1), ['manager.destroy'])
  assert.ok(!calls.some(call => /^(find|scan|choose)\b/.test(call)))
  assert.equal(registry.get('device-info').snapshot().error.detail.platform.code, 'known-peer-refused')
})

test('ordinary device-name selection still finds and passes the discovered peer', async () => {
  const { registry, calls, connections } = fixture()
  await registry.dispatch('device-info', 'read', { device: 'Polar H10 1234' })
  assert.ok(calls.some(call => call.startsWith('find ')))
  assert.equal(connections[0].target.id, 'peer-h10')
})

test('known peer uses the existing explicit single retry without discovery or replacing the requested ID', async () => {
  const transient = new BleError('connection.failed', 'connection', 'connection.connect', {
    retryability: 'caller-decides'
  })
  const { registry, calls, connections } = fixture({ connectFailures: [transient] })
  await registry.dispatch('device-info', 'read', { peerId: 'known-peer' })
  assert.deepEqual(
    connections.map(row => row.target),
    ['known-peer', 'known-peer']
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
