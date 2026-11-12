const { NavigatorWebBluetoothBoundary } = require('../../src/web/navigator-web-bluetooth-boundary')
const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const operation = { signal: null, deadline: null }
const options = { ...operation, delivery: { itemCapacity: 16, byteCapacity: 128, reservedControlCapacity: 1, overflowPolicy: 'error' } }

async function fixture(properties) {
  const characteristics = properties.map(property => {
    const listeners = new Set()
    const characteristic = {
      uuid: characteristicUuid, properties: { read: true, ...property }, value: null,
      getDescriptors: async () => [],
      addEventListener: jest.fn((type, listener) => listeners.add(listener)),
      removeEventListener: jest.fn((type, listener) => listeners.delete(listener)),
      startNotifications: jest.fn(async () => characteristic),
      stopNotifications: jest.fn(async () => characteristic),
      readValue: jest.fn(),
      writeValueWithResponse: jest.fn(),
      writeValueWithoutResponse: jest.fn(),
      emit(bytes) {
        characteristic.value = new DataView(new Uint8Array(bytes).buffer)
        for (const listener of listeners) listener({ target: characteristic })
      }
    }
    return characteristic
  })
  const service = { uuid: serviceUuid, getCharacteristics: async () => characteristics }
  const gatt = {
    connected: false,
    connect: async () => { gatt.connected = true; return gatt },
    disconnect: () => { gatt.connected = false },
    getPrimaryServices: async () => [service]
  }
  const device = { id: 'navigator-peer', gatt, addEventListener: () => {}, removeEventListener: () => {} }
  const boundary = new NavigatorWebBluetoothBoundary({
    implementationVersion: 'test', browserEngine: 'test', bluetooth: { requestDevice: async () => device },
    isSecureContext: () => true, hasTransientUserActivation: () => true, now: () => 1,
    setTimer: callback => ({ callback }), clearTimer: () => {}, addPageLifecycleListener: () => () => {}
  })
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const peer = await backend.choose({ filters: [], acceptAllDevices: true, optionalServices: [serviceUuid] }, operation)
  const lease = await backend.connections.connect(peer.peerId, 'event-provenance', operation)
  const database = await backend.gatt.discover(lease.connection, operation)
  const paths = (await database.snapshot()).characteristics.map(item => item.path)
  return { backend, lease, database, paths, characteristics }
}

test('Navigator read events and concurrent notifications reach both streams without guessed mode or lost bytes', async () => {
  const f = await fixture([{ notify: true }, { indicate: true }, { notify: true, indicate: true }])
  try {
    const ordinary = await Promise.all(f.paths.map(path => f.database.subscribe(path, options)))
    const setup = await f.database.subscribeOrdered(f.paths.map((path, index) => ({ memberId: String(index), path, deliveryMode: 'prefer-notification' })), options)
    expect(setup.state).toBe('ready')
    expect(setup.members.map(member => member.effectiveDelivery)).toEqual(['unknown', 'unknown', 'unknown'])
    let finishRead
    f.characteristics[0].readValue = () => new Promise(resolve => {
      finishRead = () => {
        f.characteristics[0].emit([11, 12])
        resolve(f.characteristics[0].value)
      }
    })
    const read = f.database.read(f.paths[0], operation)
    while (finishRead === undefined) await Promise.resolve()
    f.characteristics[0].emit([7, 8])
    f.characteristics[0].emit([7, 8])
    f.characteristics[1].emit([9])
    f.characteristics[2].emit([10])
    finishRead()
    expect(await read).toEqual({ value: new Uint8Array([11, 12]), provenance: 'read-response' })
    const ordered = setup.stream[Symbol.asyncIterator]()
    const records = await Promise.all(Array.from({ length: 5 }, () => ordered.next()))
    expect(records.map(result => result.value)).toEqual([
      expect.objectContaining({ memberId: '0', ordinal: '1', value: new Uint8Array([7, 8]), delivery: 'unknown', provenance: 'read-or-notification' }),
      expect.objectContaining({ memberId: '0', ordinal: '2', value: new Uint8Array([7, 8]), delivery: 'unknown', provenance: 'read-or-notification' }),
      expect.objectContaining({ memberId: '1', ordinal: '3', value: new Uint8Array([9]), delivery: 'unknown', provenance: 'read-or-notification' }),
      expect.objectContaining({ memberId: '2', ordinal: '4', value: new Uint8Array([10]), delivery: 'unknown', provenance: 'read-or-notification' }),
      expect.objectContaining({ memberId: '0', ordinal: '5', value: new Uint8Array([11, 12]), delivery: 'unknown', provenance: 'read-or-notification' })
    ])
    for (const [index, bytes] of [[0, [[7, 8], [7, 8], [11, 12]]], [1, [[9]]], [2, [[10]]]]) {
      const iterator = ordinary[index].values[Symbol.asyncIterator]()
      for (const value of bytes) expect((await iterator.next()).value).toMatchObject({ kind: 'value', value: { value: new Uint8Array(value), delivery: 'unknown', provenance: 'read-or-notification' } })
    }
    await setup.stream.close()
    await Promise.all(ordinary.map(subscription => subscription.remove()))
  } finally { await f.lease.release(); await f.backend.destroy() }
})

test.each([
  [{ notify: true }, 'require-notification'],
  [{ indicate: true }, 'require-indication'],
  [{ notify: true, indicate: true }, 'require-notification'],
  [{ notify: true, indicate: true }, 'require-indication']
])('hard mode %s / %s refuses ordinary and ordered delivery before enabling', async (properties, mode) => {
  const f = await fixture([properties])
  try {
    await expect(f.database.subscribe(f.paths[0], { ...options, deliveryMode: mode })).rejects.toMatchObject({ normalized: { code: 'capability.limited' } })
    const setup = await f.database.subscribeOrdered([{ memberId: 'a', path: f.paths[0], deliveryMode: mode }], options)
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'capability.limited' }, members: [{ memberId: 'a', acquisition: 'not-started' }] })
    expect(() => f.backend.createRecordingSourceOwner(f.database, [{ memberId: 'a', path: f.paths[0], deliveryMode: mode }], options, 1)).toThrow('capability.limited')
    expect(f.characteristics[0].startNotifications).not.toHaveBeenCalled()
    await setup.close()
  } finally { await f.lease.release(); await f.backend.destroy() }
})

test.each([
  ['unshared capability refusal', false, { notify: true }, 'require-notification', 'capability.limited'],
  ['shared capability refusal', true, { indicate: true }, 'require-indication', 'capability.limited'],
  ['unshared property refusal', false, { indicate: true }, 'require-notification', 'gatt.property-not-supported'],
  ['shared property refusal', true, { notify: true }, 'require-indication', 'gatt.property-not-supported']
])('ordered group validates a later hard requirement before any effect: %s', async (_label, shared, properties, mode, code) => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const f = await fixture([{ notify: true }, properties])
  let existing
  let register
  try {
    if (shared) existing = await f.database.subscribe(f.paths[0], options)
    register = jest.spyOn(WebNotificationIngress.prototype, 'register')
    const starts = f.characteristics.map(characteristic => characteristic.startNotifications.mock.calls.length)
    const stops = f.characteristics.map(characteristic => characteristic.stopNotifications.mock.calls.length)
    const listeners = f.characteristics.map(characteristic => [characteristic.addEventListener.mock.calls.length, characteristic.removeEventListener.mock.calls.length])
    const setup = await f.database.subscribeOrdered([
      { memberId: 'soft', path: f.paths[0], deliveryMode: 'prefer-notification' },
      { memberId: 'hard', path: f.paths[1], deliveryMode: mode }
    ], options)
    expect(setup).toMatchObject({ state: 'failed', error: { code }, members: [
      { memberId: 'soft', acquisition: 'not-started' },
      { memberId: 'hard', acquisition: 'not-started' }
    ] })
    expect(register).not.toHaveBeenCalled()
    expect(f.characteristics.map(characteristic => characteristic.startNotifications.mock.calls.length)).toEqual(starts)
    expect(f.characteristics.map(characteristic => characteristic.stopNotifications.mock.calls.length)).toEqual(stops)
    expect(f.characteristics.map(characteristic => [characteristic.addEventListener.mock.calls.length, characteristic.removeEventListener.mock.calls.length])).toEqual(listeners)
    for (const characteristic of f.characteristics) {
      expect(characteristic.readValue).not.toHaveBeenCalled()
      expect(characteristic.writeValueWithResponse).not.toHaveBeenCalled()
      expect(characteristic.writeValueWithoutResponse).not.toHaveBeenCalled()
    }
    expect(await setup.stream[Symbol.asyncIterator]().next()).toMatchObject({ value: { kind: 'terminal', reason: 'source-failed', uncertainTail: true } })
    if (existing) {
      f.characteristics[0].emit([42])
      expect((await existing.values[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'value', value: { value: new Uint8Array([42]) } })
    }
    await setup.close()
    expect(f.characteristics.map(characteristic => characteristic.stopNotifications.mock.calls.length)).toEqual(stops)
  } finally {
    register?.mockRestore()
    if (existing) await existing.remove()
    await f.lease.release(); await f.backend.destroy()
  }
})

test('Navigator read-generated event reaches the recording journal and replay with conservative provenance', async () => {
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const { recordingDatabase, recordingBridge } = require('./web-continuation-recording-controller-helper')
  const f = await fixture([{ notify: true }])
  let existing
  try {
    const path = f.paths[0]
    existing = await f.database.subscribe(path, options)
    const store = recordingDatabase()
    const declaration = { id: 'navigator-read', maxBytes: 1048576, maxRecords: 10, producerIncarnation: 'owner', collectionId: '1', sourceGeneration: 'generation', ownerEpoch: '1',
      context: { kind: 'ordered-recording', version: 1, peerId: 'navigator-peer', connectionGeneration: String(path.connectionGeneration), databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
        members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence), characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence) }, delivery: null, preference: null }] } }
    const bridge = recordingBridge(store, declaration)
    const owner = f.backend.createRecordingSourceOwner(f.database, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], options, 1)
    const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
    expect((await supervisor.start()).state).toBe('active')
    f.characteristics[0].readValue = async () => {
      f.characteristics[0].emit([11, 12])
      return f.characteristics[0].value
    }
    expect(await f.database.read(path, operation)).toEqual({ value: new Uint8Array([11, 12]), provenance: 'read-response' })
    expect((await existing.values[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'value', value: { value: new Uint8Array([11, 12]), provenance: 'read-or-notification', delivery: 'unknown' } })
    supervisor.stop()
    await supervisor.retry()
    if (supervisor.status().state === 'waiting-drain') await supervisor.retry()
    expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
    const controller = await createWebContinuationRecordingController({ database: store, randomToken: () => 'token' })
    const batch = await controller.prepareOrdered('navigator-read', { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'terminal'])
    expect(batch.records[0].record).toMatchObject({ member: '0', membership: { member: '0', epoch: '1' }, bytes: new Uint8Array([11, 12]), provenance: 'read-or-notification', delivery: 'unknown' })
    expect(f.characteristics[0].stopNotifications).not.toHaveBeenCalled()
  } finally {
    if (existing) await existing.remove()
    await f.lease.release(); await f.backend.destroy()
  }
})
