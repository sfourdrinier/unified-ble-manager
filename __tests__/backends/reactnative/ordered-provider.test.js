// __tests__/backends/reactnative/ordered-provider.test.js
const { rustCoreHarness, environment, subscribeOptions } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeManagerHost } = require('../../../src/react-native-manager')

const options = { signal: null, deadline: null }
function value(memberId, ordinal, byte) {
  return { kind: 'value', memberId, ordinal, sourceMembership: { member: memberId, epoch: '1' }, bytes: [byte], delivery: 'notification', provenance: 'notification' }
}

test('public database ordered setup crosses session boundary and preserves A/B/A and loss', async () => {
  const harness = rustCoreHarness()
  const native = harness.native
  const original = native.invoke.bind(native)
  const calls = []
  const records = [value('0', '9007199254740993', 1), value('1', '9007199254740994', 2), value('0', '9007199254740995', 3), { kind: 'marker', markerId: '7', ordinal: '9007199254740996' }, { kind: 'loss', loss: { scope: { kind: 'unknown' }, unit: 'source-records', count: null, bytes: null } }]
  let index = 0
  let failFirstAck = true
  let failFirstForget = true
  native.invoke = async (session, op, text) => {
    if (!op.startsWith('gatt.ordered.')) return original(session, op, text)
    const args = JSON.parse(text); calls.push([op, args])
    let result = null
    if (op === 'gatt.ordered.version') result = { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') result = { versions: [1] }
    if (op === 'gatt.ordered.marker.version') result = { wireVersion: 1 }
    if (op === 'gatt.ordered.marker.reserve') result = { handle: '18446744073709551615' }
    if (op === 'gatt.ordered.marker.start' || op === 'gatt.ordered.marker.status') result = { state: 'admitted', ordinal: '9007199254740996', write: { state: 'settled', error: null } }
    if (op === 'gatt.ordered.reserve') result = { collectionId: '1', reclaimCapability: 'capability' }
    if (op === 'gatt.ordered.status') result = { sourceGeneration: { kind: 'counter', value: '1' }, members: [0, 1].map(id => ({ memberId: String(id), acquisition: 'acquired', effectiveDelivery: 'notification', cleanupReleased: true, cleanupError: null })), cleanupComplete: true, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.poll') result = index < records.length ? { deliveryId: String(index + 1), record: JSON.stringify(records[index]) } : { deliveryId: null, record: '{"kind":"empty"}' }
    if (op === 'gatt.ordered.ack-delivery') {
      if (failFirstAck) { failFirstAck = false; return JSON.stringify({ ok: false, error: { code: 'platform.failure', domain: 'gatt', operation: 'test.ack', detail: null } }) }
      index++
    }
    if (op === 'gatt.ordered.close') result = 'released'
    if (op === 'gatt.ordered.forget-group' && failFirstForget) { failFirstForget = false; return JSON.stringify({ ok: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'test.forget', detail: null } }) }
    return JSON.stringify({ ok: true, value: result })
  }
  const host = await createReactNativeManagerHost(environment(harness))
  const manager = host.manager
  const backend = manager.attachedBackend.backend
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await manager.connect(peerId, options)
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover(options))
  const chars = database.servicesByUuid('0000180f-0000-1000-8000-00805f9b34fb')[0].characteristicsByUuid('00002a19-0000-1000-8000-00805f9b34fb')
  const setup = await database.subscribeOrdered(chars.slice(0, 2).map((characteristic, i) => ({ memberId: i ? 'b' : 'a', characteristic })))
  expect(setup.state).toBe('ready')
  const marker = setup.stream.reserveMarkerWrite()
  expect(await marker.start(chars[0], Uint8Array.from([1, 2]), 'with-response')).toMatchObject({ state: 'admitted', ordinal: '9007199254740996' })
  expect(calls.find(([op]) => op === 'gatt.ordered.marker.start')[1]).toMatchObject({ peerId: DEFAULT_PEER, collectionId: '1', handle: '18446744073709551615', valueB64: 'AQI=', mode: 'with-response' })
  const handle = host.services.orderedRecoveryHandle(setup.stream)
  expect(handle).toEqual({ peerId: DEFAULT_PEER, collectionId: '1', reclaimCapability: 'capability' })
  const iterator = setup.stream[Symbol.asyncIterator]()
  const items = []
  await expect(iterator.next()).rejects.toThrow()
  for (let i = 0; i < 5; i++) items.push((await iterator.next()).value)
  expect(items.map(item => item.kind === 'value' ? item.memberId : item.kind)).toEqual(['a', 'b', 'a', 'marker', 'loss'])
  expect(items[0].ordinal).toBe('9007199254740993')
  expect(calls.filter(([op]) => op === 'gatt.ordered.ack-delivery').map(([, args]) => args.deliveryId)).toEqual(['1', '1', '2', '3', '4', '5'])
  expect(items[4].lostSourceRecords).toBeNull()
  await marker.forget()
  expect((await setup.stream.close()).group.state).toBe('release-failed')
  expect((await setup.stream.close()).group.state).toBe('released')
  expect(calls.filter(([op]) => op === 'gatt.ordered.ack-group')).toHaveLength(1)
  expect(calls.filter(([op]) => op === 'gatt.ordered.forget-group')).toHaveLength(2)
  expect(calls.find(([op]) => op === 'gatt.ordered.forget-group')[1].reclaimCapability).toBe('capability')
  await manager.destroy()
})

test('replacement host reclaims only native-authenticated context', async () => {
  const harness = rustCoreHarness()
  const native = harness.native
  const original = native.invoke.bind(native)
  let active = true
  native.invoke = async (session, op, text) => {
    if (!op.startsWith('gatt.ordered.')) return original(session, op, text)
    const args = JSON.parse(text)
    let result = null
    if (op === 'gatt.ordered.version') result = { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') result = { versions: [1] }
    if (op === 'gatt.ordered.marker.version') result = { wireVersion: 0 }
    if (op === 'gatt.ordered.reclaim') {
      if (active || args.reclaimCapability !== 'secret') return JSON.stringify({ ok: false, error: { code: 'ownership.denied', domain: 'gatt', operation: 'gatt.ordered.reclaim', detail: null } })
      result = { connectionGeneration: '11', databaseGeneration: '12', publicConnectionGeneration: 'original-c', publicDatabaseGeneration: 'original-d', maxItems: 4, maxBytes: 64, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'original-a', selector: { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', serviceOccurrence: 0, characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb', characteristicOccurrence: 0 }, requestedDelivery: 'prefer-notification', deliveryMode: null, preferredDeliveryMode: 'notification' }] }
    }
    if (op === 'gatt.ordered.status') result = { sourceGeneration: { kind: 'counter', value: '7' }, members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'notification', cleanupReleased: true, cleanupError: null }], cleanupComplete: true, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.poll') result = { deliveryId: '1', record: JSON.stringify(value('0', '9007199254740993', 1)) }
    if (op === 'gatt.ordered.close') result = 'released'
    return JSON.stringify({ ok: true, value: result })
  }
  const host = await createReactNativeManagerHost(environment(harness))
  const handle = { peerId: DEFAULT_PEER, collectionId: '1', reclaimCapability: 'secret' }
  await expect(host.services.reclaimOrdered(handle)).rejects.toThrow()
  active = false
  const recovered = await host.services.reclaimOrdered(handle)
  expect(recovered.state).toBe('reclaimed')
  const item = (await recovered.stream[Symbol.asyncIterator]().next()).value
  expect(item).toMatchObject({ memberId: 'original-a', connectionGeneration: 'original-c', databaseGeneration: 'original-d', ordinal: '9007199254740993' })
  await recovered.stream.close()
  await host.manager.destroy()
})

test('public abort immediately after reserve cleans unopened native owner', async () => {
  const harness = rustCoreHarness()
  const native = harness.native
  const original = native.invoke.bind(native)
  const controller = new AbortController()
  const calls = []
  let closed = false
  native.invoke = async (session, op, text) => {
    if (!op.startsWith('gatt.ordered.')) return original(session, op, text)
    const args = JSON.parse(text); calls.push([op, args])
    let result = null
    if (op === 'gatt.ordered.version') result = { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') result = { versions: [1] }
    if (op === 'gatt.ordered.marker.version') result = { wireVersion: 0 }
    if (op === 'gatt.ordered.reserve') { controller.abort(); result = { collectionId: '9', reclaimCapability: 'cap' } }
    if (op === 'gatt.ordered.status') result = { sourceGeneration: null, members: [], cleanupComplete: closed, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') { closed = true; result = 'released' }
    if (op === 'gatt.ordered.poll' || op === 'gatt.ordered.open') throw new Error(`unexpected ${op}`)
    return JSON.stringify({ ok: true, value: result })
  }
  const host = await createReactNativeManagerHost(environment(harness))
  const backend = host.manager.attachedBackend.backend
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await host.manager.connect(peerId, options)
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover(options))
  const characteristic = database.service('0000180d-0000-1000-8000-00805f9b34fb').characteristic('00002a37-0000-1000-8000-00805f9b34fb')
  const result = await database.subscribeOrdered([{ memberId: 'a', characteristic }], { signal: controller.signal })
  expect(result.stream.reserveMarkerWrite).toBeUndefined()
  expect(result).toMatchObject({ state: 'failed', error: { code: 'operation.aborted' }, members: [{ memberId: 'a', acquisition: 'not-started' }], observations: [], cleanup: { group: { state: 'released' } } })
  expect(calls.map(([op]) => op)).toEqual(['gatt.ordered.marker.version', 'gatt.ordered.version', 'gatt.ordered.failure-metadata.version', 'gatt.ordered.reserve', 'gatt.ordered.status', 'gatt.ordered.close', 'gatt.ordered.status', 'gatt.ordered.ack-group', 'gatt.ordered.forget-group'])
  await host.manager.destroy()
})

test('public failed setup preserves admitted prefix across a later malformed delivery', async () => {
  const harness = rustCoreHarness()
  const native = harness.native
  const original = native.invoke.bind(native)
  let cursor = 0
  let malformed = true
  native.invoke = async (session, op, text) => {
    if (!op.startsWith('gatt.ordered.')) return original(session, op, text)
    let result = null
    if (op === 'gatt.ordered.version') result = { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') result = { versions: [1] }
    if (op === 'gatt.ordered.marker.version') result = { wireVersion: 0 }
    if (op === 'gatt.ordered.reserve') result = { collectionId: '10', reclaimCapability: 'cap' }
    if (op === 'gatt.ordered.open') return JSON.stringify({ ok: false, error: { code: 'operation.aborted', domain: 'gatt', operation: 'test.open', detail: null } })
    if (op === 'gatt.ordered.status') result = { sourceGeneration: { kind: 'counter', value: '1' }, members: [{ memberId: '0', acquisition: 'indeterminate', effectiveDelivery: 'notification', cleanupReleased: false, cleanupError: null }], cleanupComplete: false, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') result = 'pending-source'
    if (op === 'gatt.ordered.poll') result = cursor === 0 ? { deliveryId: '1', record: JSON.stringify(value('0', '1', 5)) } : { deliveryId: '2', record: malformed ? '{' : JSON.stringify(value('0', '2', 6)) }
    if (op === 'gatt.ordered.ack-delivery') cursor++
    return JSON.stringify({ ok: true, value: result })
  }
  const host = await createReactNativeManagerHost(environment(harness))
  const backend = host.manager.attachedBackend.backend
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await host.manager.connect(peerId, options)
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover(options))
  const characteristic = database.service('0000180d-0000-1000-8000-00805f9b34fb').characteristic('00002a37-0000-1000-8000-00805f9b34fb')
  const setup = await database.subscribeOrdered([{ memberId: 'a', characteristic }])
  expect(setup).toMatchObject({ state: 'failed', observations: [{ kind: 'value', memberId: 'a', ordinal: '1' }] })
  expect(setup.cleanup.group.failures.some(f => f.resourceKind === 'ordered-observations')).toBe(true)
  expect(cursor).toBe(1)
  malformed = false
  expect((await setup.stream[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'value', memberId: 'a', ordinal: '2' })
  expect(cursor).toBe(2)
  await host.manager.destroy()
})
