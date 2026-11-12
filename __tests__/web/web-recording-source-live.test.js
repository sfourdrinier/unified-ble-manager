const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')

const options = { signal: null, deadline: null, delivery: { itemCapacity: 8, byteCapacity: 64, reservedControlCapacity: 1, overflowPolicy: 'error' } }

async function fixture() {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const choose = backend.choose({ filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] }, { signal: null, deadline: null })
  await boundary.flush()
  boundary.resolveChooser()
  const selected = await choose
  const connection = await backend.connections.connect(selected.peerId, 'recording', { signal: null, deadline: null })
  const database = await backend.gatt.discover(connection.connection, { signal: null, deadline: null })
  const path = (await database.snapshot()).characteristics[0].path
  return { boundary, backend, connection, database, path }
}

test('real GATT lease shares a subscriber and wakes the supervisor from DOM admission', async () => {
  const f = await fixture()
  const subscriber = await f.database.subscribe(f.path, options)
  const deliveries = []
  const journal = {
    prepare: jest.fn(async () => {}), open: jest.fn(async () => {}),
    commit: jest.fn(async delivery => { deliveries.push(delivery) }), retire: jest.fn(async () => {}),
    seal: jest.fn(async () => {}), releaseAuthority: jest.fn(async () => {}), classifyFailure: () => 'failure'
  }
  const owner = f.backend.createRecordingSourceOwner(f.database, [{ memberId: 'hr', path: f.path, deliveryMode: 'prefer-notification' }], options, 1)
  const supervisor = new WebOrderedRecordingSupervisor(owner, journal)
  expect(f.boundary.notificationStarts).toBe(1)
  expect((await supervisor.start()).state).toBe('active')
  f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: new Uint8Array([42]) })
  for (let i = 0; i < 30 && deliveries.length === 0; i++) await Promise.resolve()
  expect(deliveries[0].record).toMatchObject({ memberId: '0', value: new Uint8Array([42]), sourceMembership: { member: '0', epoch: '1' }, delivery: 'unknown', provenance: 'read-or-notification' })
  supervisor.stop()
  await supervisor.retry()
  expect(supervisor.status().state).toBe('finished')
  expect(f.boundary.notificationStops).toBe(0)
  await subscriber.remove()
  expect(f.boundary.notificationStops).toBe(1)
  await f.connection.release()
  await f.backend.destroy()
})

test('connection loss fences the retained source with its actual reason', async () => {
  const f = await fixture()
  const owner = f.backend.createRecordingSourceOwner(f.database, [{ memberId: 'hr', path: f.path, deliveryMode: 'prefer-notification' }], options, 1)
  owner.reserve()
  const { source, leases } = owner.install(() => {})
  await leases[0].enable()
  await f.connection.release()
  const records = []
  while (source.peek() !== null) { const entry = source.peek(); records.push(entry.record); source.ackExact(entry.id) }
  expect(records.at(-1)).toMatchObject({ kind: 'terminal', reason: 'owner-released', uncertainTail: true })
  await leases[0].release()
  await owner.release()
  await f.backend.destroy()
})

test('a dispatched late enable remains owned until failed physical cleanup is retried', async () => {
  const f = await fixture()
  const original = f.boundary.characteristic.startNotifications
  let finishStart
  f.boundary.characteristic.startNotifications = () => new Promise(resolve => { finishStart = () => resolve(original()) })
  const owner = f.backend.createRecordingSourceOwner(f.database, [{ memberId: 'hr', path: f.path, deliveryMode: 'prefer-notification' }], options, 1)
  owner.reserve()
  const { source, leases } = owner.install(() => {})
  const enabling = leases[0].enable()
  for (let i = 0; i < 10 && finishStart === undefined; i++) await Promise.resolve()
  source.retire('closed')
  const cleanup = leases[0].release()
  f.boundary.failNextNotificationStop(new Error('stop failed'))
  finishStart()
  await enabling
  await expect(cleanup).rejects.toThrow('cleanup failed')
  expect(f.boundary.notificationStops).toBe(1)
  expectConsoleErrorMatching('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
  await leases[0].release()
  expect(f.boundary.notificationStops).toBe(2)
  while (source.peek()) source.ackExact(source.peek().id)
  await owner.release()
  await f.connection.release()
  await f.backend.destroy()
})

test('joined graph receives retained ingress while another graph owns the physical listener', async () => {
  const f = await fixture()
  const subscriber = await f.database.subscribe(f.path, options)
  const joined = await f.backend.connections.connect(f.connection.connection.peerId, 'joined', { signal: null, deadline: null })
  const other = await f.backend.gatt.discover(joined.connection, { signal: null, deadline: null })
  const path = (await other.snapshot()).characteristics[0].path
  const owner = f.backend.createRecordingSourceOwner(other, [{ memberId: 'hr', path, deliveryMode: 'prefer-notification' }], options, 1)
  owner.reserve()
  const { source, leases } = owner.install(() => {})
  await leases[0].enable()
  f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: new Uint8Array([42]) })
  const actual = source.peek()
  source.retire('closed')
  while (source.peek()) source.ackExact(source.peek().id)
  await leases[0].release()
  await owner.release()
  await subscriber.remove()
  await joined.release()
  await f.connection.release()
  await f.backend.destroy()
  expect(actual?.record).toMatchObject({ kind: 'value', value: new Uint8Array([42]) })
})
