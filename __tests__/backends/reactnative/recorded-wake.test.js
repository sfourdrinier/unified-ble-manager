// __tests__/backends/reactnative/recorded-wake.test.js
const {
  rustCoreHarness,
  environment,
  settle,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const options = { signal: null, deadline: null }

async function nextWake(iterator) {
  let item = (await iterator.next()).value
  if (item.kind === 'overflow') item = (await iterator.next()).value
  return item
}

test('recorded handles wake initially, on a native hint and on lost control; removal closes the stream', async () => {
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const peer = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await manager.connect(peer, options)
  const database = await connection.discover(options)
  const path = (await database.snapshot()).characteristics[0].path
  const subscription = await database.subscribeRecorded(path, {
    ...subscribeOptions(),
    recording: { id: 'wake', maxBytes: 1048576, maxRecords: 100 }
  })
  const iterator = subscription.changed[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  const session = harness.native.liveSessions()[0]
  const consumer = [...session.consumers.keys()][0]
  harness.native.push(session, { t: 'recording-changed', consumer, recordingId: 'wake' })
  await settle()
  expect((await iterator.next()).value).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  harness.native.loseControl(() =>
    harness.native.push(session, { t: 'recording-changed', consumer, recordingId: 'wake' })
  )
  await settle()
  const afterLoss = (await iterator.next()).value
  if (afterLoss.kind === 'overflow') {
    expect((await iterator.next()).value).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  } else {
    expect(afterLoss).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  }
  const beforeIdle = harness.native.calls.filter(call => call[0] === 'drain').length
  await settle(100)
  expect(harness.native.calls.filter(call => call[0] === 'drain')).toHaveLength(beforeIdle)
  harness.native.failNext('gatt.unsubscribe', 'platform.failure', 'gatt', 'ubm-mobile.gatt.unsubscribe')
  expect((await subscription.remove()).state).toBe('release-failed')
  harness.native.push(session, { t: 'recording-changed', consumer, recordingId: 'wake' })
  await settle()
  expect((await iterator.next()).value).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  expect((await subscription.remove()).state).toBe('released')
  expect(await nextWake(iterator)).toEqual({ kind: 'value', value: { recordingId: 'wake' } })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await manager.destroy()
})

test('public characteristic exposes only recorded wake hints', async () => {
  const { createPublicBleManager } = require('../../../src/public/ble-manager')
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  const peer = manager.attachedBackend.backend.connections.peerFromAddress({
    address: DEFAULT_PEER,
    addressType: 'public'
  })
  const connection = await publicManager.connect(peer)
  const database = await connection.discover()
  const subscription = await database.services[0].characteristics[0].subscribeRecorded({
    recording: { id: 'public-wake', maxBytes: 1048576, maxRecords: 100 }
  })
  expect(subscription.values).toBeUndefined()
  expect((await subscription.changed[Symbol.asyncIterator]().next()).value).toEqual({
    kind: 'value',
    value: { recordingId: 'public-wake' }
  })
  expect((await subscription.remove()).state).toBe('released')
  await manager.destroy()
})

test.each(['peer', 'local', 'adapter'])(
  'public recorded wake preserves a queued hint and ends on %s link loss',
  async reason => {
    const { createPublicBleManager } = require('../../../src/public/ble-manager')
    const harness = rustCoreHarness()
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const publicManager = await createPublicBleManager(manager, () => 1000)
    const peer = manager.attachedBackend.backend.connections.peerFromAddress({
      address: DEFAULT_PEER,
      addressType: 'public'
    })
    const connection = await publicManager.connect(peer)
    const database = await connection.discover()
    const subscription = await database.services[0].characteristics[0].subscribeRecorded({
      recording: { id: 'link-wake', maxBytes: 1048576, maxRecords: 100 }
    })
    const iterator = subscription.changed[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toEqual({ kind: 'value', value: { recordingId: 'link-wake' } })
    const session = harness.native.liveSessions()[0]
    const consumer = [...session.consumers.keys()][0]
    harness.native.push(session, { t: 'recording-changed', consumer, recordingId: 'link-wake' })
    harness.native.dropLink(DEFAULT_PEER, reason)
    await settle(120)
    expect(await nextWake(iterator)).toEqual({ kind: 'value', value: { recordingId: 'link-wake' } })
    expect((await iterator.next()).value).toMatchObject({
      kind: 'terminal',
      reason: reason === 'adapter' ? 'source-failed' : 'connection-lost'
    })
    expect((await subscription.remove()).state).toBe('released')
    await manager.destroy()
  }
)

test('confirmed delayed removal leaves a final wake for journal tail inspection', async () => {
  const { createPublicBleManager } = require('../../../src/public/ble-manager')
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  const peer = manager.attachedBackend.backend.connections.peerFromAddress({
    address: DEFAULT_PEER,
    addressType: 'public'
  })
  const connection = await publicManager.connect(peer)
  const database = await connection.discover()
  const subscription = await database.services[0].characteristics[0].subscribeRecorded({
    recording: { id: 'tail-wake', maxBytes: 1048576, maxRecords: 100 }
  })
  const iterator = subscription.changed[Symbol.asyncIterator]()
  expect(await nextWake(iterator)).toEqual({ kind: 'value', value: { recordingId: 'tail-wake' } })
  harness.native.hold('gatt.unsubscribe')
  const removal = subscription.remove()
  await settle()
  expect(harness.native.opsInvoked('gatt.unsubscribe')).toHaveLength(1)
  harness.native.release('gatt.unsubscribe')
  expect((await removal).state).toBe('released')
  expect(await nextWake(iterator)).toEqual({ kind: 'value', value: { recordingId: 'tail-wake' } })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await manager.destroy()
})
