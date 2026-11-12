const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { WebOrderedMarkerTransactions } = require('../../src/web/web-ordered-marker')

const operation = { signal: null, deadline: null }
const options = { ...operation, delivery: { itemCapacity: 16, byteCapacity: 128, reservedControlCapacity: 1, overflowPolicy: 'error' } }

async function fixture() {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const choosing = backend.choose({ filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] }, operation)
  await boundary.flush()
  boundary.resolveChooser()
  const selected = await choosing
  async function graph(name) {
    const lease = await backend.connections.connect(selected.peerId, name, operation)
    const database = await backend.gatt.discover(lease.connection, operation)
    const path = (await database.snapshot()).characteristics[0].path
    return { lease, database, path }
  }
  function emit(value) {
    boundary.emitNotification({ serviceUuid: boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: boundary.characteristicUuid, characteristicOccurrence: 0, value: new Uint8Array([value]) })
  }
  function retained(g, wake = () => {}, capacity = 16) {
    const characteristic = [...g.database.characteristicBoundaries.values()][0]
    return backend.gattRuntime.notificationIngress(g.database).registerRetained(new Map([[characteristic, 'member']]), capacity, 128, { wake, membership: true })
  }
  return { boundary, backend, graph, emit, retained }
}

test('a marker from another graph wake follows an already arrived value', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const second = f.retained(b)
    const target = { writeValueWithResponse: () => Promise.resolve() }
    const markers = new WebOrderedMarkerTransactions(second, second, 'collection', 'connection', 'database')
    let receipt
    f.retained(a, () => { if (!receipt) receipt = markers.write('0', target, new Uint8Array(), 'with-response', null) })
    f.emit(1)
    expect(receipt.marker).toEqual({ state: 'admitted', ordinal: '2' })
    expect(second.peek().record).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([1]) })
    second.ackExact(second.peek().id)
    expect(second.peek().record).toMatchObject({ kind: 'marker', ordinal: '2' })
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a synchronous wake cannot write a marker ahead of a staged public ordered value', async () => {
  const f = await fixture()
  try {
    f.boundary.characteristic.properties = { ...f.boundary.characteristic.properties, write: true }
    const write = jest.fn(() => Promise.resolve())
    f.boundary.characteristic.writeValueWithResponse = write
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], options)
    expect(setup.state).toBe('ready')
    const handle = setup.stream.reserveMarkerWrite()
    let result
    f.retained(a, () => { if (!result) result = handle.start(b.path, new Uint8Array([9]), 'with-response', operation) })
    f.emit(1)
    expect(await result).toMatchObject({ state: 'admitted', ordinal: '2' })
    expect(write).toHaveBeenCalledTimes(1)
    const iterator = setup.stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([1]) })
    expect((await iterator.next()).value).toMatchObject({ kind: 'marker', ordinal: '2' })
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test.each(['close', 'discard'])('%s from a sibling wake accounts for an already staged value', async action => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], options)
    let cleanup
    f.retained(a, () => {
      if (cleanup === undefined) {
        cleanup = setup.stream[action]()
        f.emit(99)
      }
    })
    f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([42, 43]))
    const result = await cleanup
    if (action === 'discard') {
      expect(result.discarded).toMatchObject({ values: '1', bytes: '2', terminal: { kind: 'terminal', reason: 'closed' }, losses: [] })
      expect((await setup.stream.discard()).discarded).toEqual(result.discarded)
    } else {
      const iterator = setup.stream[Symbol.asyncIterator]()
      expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([42, 43]) })
      expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed' })
      expect((await iterator.next()).done).toBe(true)
    }
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('pipelined reads retain loss attribution and observe terminal before completion', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 2, reservedControlCapacity: 2 }
    })
    f.emit(1); f.emit(2)
    let emitted = false
    f.retained(a, () => {
      if (emitted) return
      emitted = true
      f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([4, 4]))
      f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([5, 5, 5]))
    })
    f.emit(3)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const reads = Array.from({ length: 7 }, () => iterator.next())
    const results = await Promise.all(reads)
    expect(results.map(result => result.done ? 'done' : result.value.kind)).toEqual(['value', 'value', 'loss', 'loss', 'loss', 'terminal', 'done'])
    expect(results.slice(2, 5).map(result => result.value)).toEqual([
      expect.objectContaining({ scope: { kind: 'member', memberId: 'member' }, reason: 'overflow', lostValues: '1', lostBytes: '1' }),
      expect.objectContaining({ scope: { kind: 'member', memberId: 'member' }, reason: 'overflow', lostValues: '1', lostBytes: '2' }),
      expect.objectContaining({ scope: { kind: 'group' }, reason: 'overflow', lostValues: '1', lostBytes: '3' })
    ])
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('capacity-one ordered stream does not fabricate omissions after its source is fenced', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('bounded-a'); const b = await f.graph('bounded-b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1, reservedControlCapacity: 1 }
    })
    f.emit(1)
    let emitted = false
    f.retained(a, () => {
      if (emitted) return
      emitted = true
      for (let i = 0; i < 20; i++) f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array(i + 1))
    })
    f.emit(2)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const items = []
    for (;;) {
      const next = await iterator.next()
      if (next.done) break
      items.push(next.value)
    }
    expect(items.at(-1).kind).toBe('terminal')
    expect(items.some(item => item.kind === 'failure-observations-omitted')).toBe(false)
    expect(items.length).toBeLessThanOrEqual(5)
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('capacity-one public stream reserves omission evidence for an overflowing staged tail', async () => {
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const f = await fixture()
  try {
    const a = await f.graph('staged')
    const ordinary = await a.database.subscribe(a.path, options)
    const setup = await a.database.subscribeOrdered([{ memberId: 'member', path: a.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1, reservedControlCapacity: 1 }
    })
    f.emit(1)
    const original = CoreOrderedNotificationStream.prototype.emit
    let nested = false
    jest.spyOn(CoreOrderedNotificationStream.prototype, 'emit').mockImplementation(function (item) {
      if (!nested && item.kind === 'value') {
        nested = true
        f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array(2))
        f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array(3))
      }
      return original.call(this, item)
    })
    f.emit(2)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const items = []
    for (;;) {
      const next = await iterator.next()
      if (next.done) break
      items.push(next.value)
    }
    expect(items.map(item => item.kind)).toEqual(['value', 'loss', 'loss', 'failure-observations-omitted', 'terminal'])
    expect(items[2]).toMatchObject({ lostBytes: '2' })
    expect(items[3].count).toBeNull()
    await setup.stream.close(); await ordinary.remove(); await a.lease.release()
  } finally { jest.restoreAllMocks(); await f.backend.destroy() }
})

test('a close draining multiple staged values preserves consumer overflow and subsequent loss', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 2, reservedControlCapacity: 2 }
    })
    f.emit(1); f.emit(2)
    let closing
    let started = false
    f.retained(a, () => {
      if (started) return
      started = true
      f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([4, 4]))
      closing = setup.stream.close()
    })
    f.emit(3)
    await closing
    const iterator = setup.stream[Symbol.asyncIterator]()
    const results = await Promise.all(Array.from({ length: 6 }, () => iterator.next()))
    expect(results.map(result => result.done ? 'done' : result.value.kind)).toEqual(['value', 'value', 'loss', 'loss', 'terminal', 'done'])
    expect(results[2].value).toMatchObject({ reason: 'overflow', lostBytes: '1' })
    expect(results[3].value).toMatchObject({ reason: 'overflow', lostBytes: '2' })
    expect(results[4].value).toMatchObject({ reason: 'overflow' })
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a sink exception during close drain is reported and its staged bytes remain accounted', async () => {
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], options)
    const emit = jest.spyOn(CoreOrderedNotificationStream.prototype, 'emit').mockImplementationOnce(() => { throw new Error('drain sink failed') })
    let closing
    f.retained(a, () => { if (closing === undefined) closing = setup.stream.close() })
    f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([8, 9]))
    await expect(closing).rejects.toMatchObject({ normalized: { code: 'platform.failure' } })
    emit.mockRestore()
    const iterator = setup.stream[Symbol.asyncIterator]()
    const results = await Promise.all([iterator.next(), iterator.next(), iterator.next()])
    expect(results[0].value).toMatchObject({ kind: 'loss', reason: 'source-loss', lostValues: '1', lostBytes: '2' })
    expect(results[1].value).toMatchObject({ kind: 'terminal', reason: 'closed', fenceError: { code: 'platform.failure' } })
    expect(results[2].done).toBe(true)
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { jest.restoreAllMocks(); await f.backend.destroy() }
})

test('pipelined pending reads finish in order when a sibling wake closes the stream', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], options)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const reads = [iterator.next(), iterator.next(), iterator.next()]
    await Promise.resolve()
    let closing
    f.retained(a, () => { if (closing === undefined) closing = setup.stream.close() })
    f.emit(7)
    await closing
    const results = await Promise.all(reads)
    expect(results[0].value).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([7]) })
    expect(results[1].value).toMatchObject({ kind: 'terminal', reason: 'closed' })
    expect(results[2].done).toBe(true)
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a nested burst reports exact rejected bytes before the public ordered terminal', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1 }
    })
    expect(setup.state).toBe('ready')
    let emitted = false
    f.retained(a, () => {
      if (!emitted) {
        emitted = true
        f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([2, 3]))
      }
    })
    f.emit(1)
    const iterator = setup.stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1', value: new Uint8Array([1]) })
    expect((await iterator.next()).value).toEqual({ kind: 'loss', scope: { kind: 'group' }, reason: 'overflow', lostValues: '1', lostBytes: '2', lostSourceRecords: null })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a throwing staged sink preserves its loss ahead of a later staging overflow', async () => {
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const f = await fixture()
  const error = new Error('ordered sink failed')
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 2 }
    })
    const emit = jest.spyOn(CoreOrderedNotificationStream.prototype, 'emit')
    emit.mockImplementationOnce(() => { throw error })
    let emitted = false
    f.retained(a, () => {
      if (!emitted) {
        emitted = true
        f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([2, 3]))
        f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([4, 5, 6]))
      }
    })
    expect(() => f.emit(1)).toThrow(error)
    emit.mockRestore()
    const iterator = setup.stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toEqual({ kind: 'loss', scope: { kind: 'group' }, reason: 'source-loss', lostValues: '1', lostBytes: '1', lostSourceRecords: null })
    expect((await iterator.next()).value).toEqual({ kind: 'loss', scope: { kind: 'group' }, reason: 'source-loss', lostValues: '1', lostBytes: '2', lostSourceRecords: null })
    expect((await iterator.next()).value).toEqual({ kind: 'loss', scope: { kind: 'group' }, reason: 'overflow', lostValues: '1', lostBytes: '3', lostSourceRecords: null })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { jest.restoreAllMocks(); await f.backend.destroy() }
})

test('consumer overflow does not discard a later known staging loss', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1, reservedControlCapacity: 2 }
    })
    f.emit(1)
    let emitted = false
    f.retained(a, () => {
      if (!emitted) { emitted = true; f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([3, 4])) }
    })
    f.emit(2)
    const iterator = setup.stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', ordinal: '1' })
    expect((await iterator.next()).value).toMatchObject({ kind: 'loss', lostValues: '1', lostBytes: '1' })
    expect((await iterator.next()).value).toMatchObject({ kind: 'loss', lostValues: '1', lostBytes: '2' })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow' })
    await setup.stream.close(); await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('discard accounts for a late source loss and held terminal exactly once', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1, reservedControlCapacity: 2 }
    })
    f.emit(1)
    let emitted = false
    f.retained(a, () => {
      if (!emitted) { emitted = true; f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([3, 4])) }
    })
    f.emit(2)
    const iterator = setup.stream[Symbol.asyncIterator]()
    await iterator.next(); await iterator.next()
    expect((await iterator.next()).value).toMatchObject({ kind: 'loss', lostBytes: '2' })
    const first = await setup.stream.discard()
    const again = await setup.stream.discard()
    expect(first.discarded).toMatchObject({ controls: '1', terminal: { kind: 'terminal', reason: 'overflow' } })
    expect(again.discarded).toEqual(first.discarded)
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('discard immediately fences a terminal held behind a source loss', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const setup = await b.database.subscribeOrdered([{ memberId: 'member', path: b.path, deliveryMode: 'prefer-notification' }], {
      ...options, delivery: { ...options.delivery, itemCapacity: 1, reservedControlCapacity: 2 }
    })
    f.emit(1)
    let emitted = false
    f.retained(a, () => {
      if (!emitted) { emitted = true; f.boundary.deliverNotification(f.boundary.characteristic, new Uint8Array([3, 4])) }
    })
    f.emit(2)
    const iterator = setup.stream[Symbol.asyncIterator]()
    await iterator.next(); await iterator.next(); await iterator.next()
    const discarded = setup.stream.discard()
    expect(await iterator.next()).toEqual({ done: true, value: undefined })
    await discarded
    await a1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a marker reentering its own sink cannot pass the unfinished value', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress(); const characteristic = {}; const order = []
  let group
  group = ingress.register(new Map([[characteristic, 'member']]), 4, 8, () => {
    order.push('sink-start')
    expect(group.admitMarker(() => { order.push('marker'); return true })).toBeNull()
    order.push('sink-end')
    return true
  })
  ingress.admit(characteristic, new Uint8Array([1]))
  expect(order).toEqual(['sink-start', 'sink-end'])
  expect(group.admitMarker(() => { order.push('marker'); return true })).toBe('2')
})

test('a group registered after a nested arrival cannot receive that value', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    let fresh
    f.retained(a, () => { if (!fresh) { f.emit(2); fresh = f.retained(b) } })
    const existing = f.retained(b)
    f.emit(1)
    expect(values(existing)).toEqual([['1', 1], ['2', 2]])
    expect(values(fresh)).toEqual([])
    f.emit(3)
    expect(fresh.peek().record).toMatchObject({ kind: 'value', ordinal: '3', value: new Uint8Array([3]), sourceMembership: { member: 'member', epoch: '2' } })
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a reentrant burst meets retained capacity immediately and reports the rejected bytes', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    let emitted = false
    let terminalDuringBurst
    f.retained(a, () => { if (!emitted) { emitted = true; for (let i = 0; i < 1000; i++) f.emit(2); terminalDuringBurst = second.terminalReason } }, 2)
    const second = f.retained(b, () => {}, 2)
    f.emit(1)
    expect(terminalDuringBurst).toBe('overflow')
    expect(second.peek().record).toMatchObject({ kind: 'value', ordinal: '1' }); second.ackExact(second.peek().id)
    expect(second.peek().record).toMatchObject({ kind: 'value', ordinal: '2' }); second.ackExact(second.peek().id)
    expect(second.peek().record).toEqual({ kind: 'loss', lostValues: 1, lostBytes: 1 }); second.ackExact(second.peek().id)
    expect(second.peek().record).toEqual({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('deferred wakes coalesce without losing retained deliveries', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress()
  const target = {}
  const wake = jest.fn()
  const group = ingress.registerRetained(new Map([[target, 'member']]), 3, 8, { wake })
  ingress.holdCallbacks()
  ingress.admit(target, new Uint8Array([1]))
  ingress.admit(target, new Uint8Array([2]))
  expect(wake).not.toHaveBeenCalled()
  ingress.releaseCallbacks()
  expect(wake).toHaveBeenCalledTimes(1)
  expect(values(group)).toEqual([['1', 1], ['2', 2]])
})

function values(source) {
  const result = []
  while (source.peek()) {
    const entry = source.peek()
    if (entry.record.kind === 'value') result.push([entry.record.ordinal, ...entry.record.value])
    source.ackExact(entry.id)
  }
  return result
}

test('shared graphs receive each synchronous reentrant event once and in order alongside ordinary subscribers', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a')
    const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const a2 = await a.database.subscribe(a.path, options)
    const b1 = await b.database.subscribe(b.path, options)
    const firstIterator = a1.values[Symbol.asyncIterator]()
    await firstIterator.next()
    let nested = false
    const first = f.retained(a, () => { if (!nested) { nested = true; f.emit(2) } })
    const second = f.retained(b)
    f.emit(1)
    expect(values(first)).toEqual([['1', 1], ['2', 2]])
    expect(values(second)).toEqual([['1', 1], ['2', 2]])
    for (const subscription of [a1, a2, b1]) {
      const iterator = subscription === a1 ? firstIterator : subscription.values[Symbol.asyncIterator]()
      expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([1]))
      expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([2]))
    }
    expect(f.boundary.notificationStarts).toBe(1)
    await a1.remove(); await a2.remove(); await b1.remove()
    expect(f.boundary.notificationStops).toBe(1)
    await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('detached and replaced graphs stop receiving while surviving and replacement leases share the listener', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a')
    const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const b1 = await b.database.subscribe(b.path, options)
    const old = f.retained(a)
    const other = f.retained(b)
    f.emit(1)
    old.retire('closed')
    await a1.remove()
    f.emit(2)
    expect(values(old)).toEqual([['1', 1]])
    const replacementDatabase = await f.backend.gatt.discover(a.lease.connection, operation)
    const replacement = { database: replacementDatabase, path: (await replacementDatabase.snapshot()).characteristics[0].path }
    const a2 = await replacement.database.subscribe(replacement.path, options)
    const fresh = f.retained(replacement)
    f.emit(3)
    await a.lease.release()
    f.emit(4)
    expect(values(old)).toEqual([])
    expect(values(fresh)).toEqual([['1', 3]])
    expect(values(other)).toEqual([['1', 1], ['2', 2], ['3', 3], ['4', 4]])
    expect(f.boundary.notificationStarts).toBe(1)
    await a2.remove(); await b1.remove()
    expect(f.boundary.notificationStops).toBe(1)
    await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a graph detached from a wake retains the arrived value but not a later callback', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a')
    const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options)
    const b1 = await b.database.subscribe(b.path, options)
    let removal, second
    const first = f.retained(a, () => {
      if (removal === undefined) {
        second.retire('closed')
        removal = b1.remove()
        f.emit(2)
      }
    })
    second = f.retained(b)
    f.emit(1)
    await removal
    expect(values(first)).toEqual([['1', 1], ['2', 2]])
    expect(values(second)).toEqual([['1', 1]])
    await a1.remove()
    expect(f.boundary.notificationStarts).toBe(1)
    expect(f.boundary.notificationStops).toBe(1)
    await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a retained listener from a replaced physical owner cannot deliver to the new owner', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const characteristic = [...a.database.characteristicBoundaries.values()][0]
    const first = await a.database.subscribe(a.path, options)
    const stale = [...characteristic.listeners][0]
    await first.remove()
    const replacement = await b.database.subscribe(b.path, options)
    const retained = f.retained(b)
    stale(new Uint8Array([9]))
    f.emit(1)
    expect(values(retained)).toEqual([['1', 1]])
    await replacement.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a detached listener cannot deliver while its physical stop is pending', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a')
    const subscription = await a.database.subscribe(a.path, options)
    const characteristic = [...a.database.characteristicBoundaries.values()][0]
    const stale = [...characteristic.listeners][0]
    const source = f.retained(a)
    let finishStop
    characteristic.stopNotifications = () => new Promise(resolve => { finishStop = resolve })
    const removal = subscription.remove()
    for (let i = 0; i < 8 && finishStop === undefined; i++) await Promise.resolve()
    expect(finishStop).toBeDefined()
    stale(new Uint8Array([9]))
    expect(values(source)).toEqual([])
    finishStop(); await removal; await a.lease.release()
  } finally { await f.backend.destroy() }
})

test('a throwing wake does not leave another graph callback hold stuck', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    let firstWake = true
    f.retained(a, () => { if (firstWake) { firstWake = false; throw new Error('wake failed') } })
    const wake = jest.fn()
    const second = f.retained(b, wake)
    expect(() => f.emit(1)).toThrow('wake failed')
    expect(wake).toHaveBeenCalledTimes(1)
    expect(values(second)).toEqual([['1', 1]])
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('joined graphs survive one aborted pending start and clean up after its late completion', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const characteristic = [...a.database.characteristicBoundaries.values()][0]
    let finishStart
    const originalStart = characteristic.startNotifications
    characteristic.startNotifications = () => new Promise(resolve => { finishStart = resolve })
    const abort = new AbortController()
    const openingA = a.database.subscribe(a.path, { ...options, signal: abort.signal })
    const openingB = b.database.subscribe(b.path, options)
    await Promise.resolve(); await Promise.resolve()
    const first = f.retained(a); const second = f.retained(b)
    f.emit(1)
    abort.abort()
    await expect(openingA).rejects.toBeDefined()
    first.retire('operation-aborted')
    f.emit(2)
    finishStart()
    const subscription = await openingB
    expect(values(first)).toEqual([['1', 1]])
    expect(values(second)).toEqual([['1', 1], ['2', 2]])
    await subscription.remove()
    expect(f.boundary.notificationStops).toBe(1)
    characteristic.startNotifications = originalStart
    await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('an installed group receives shared observations before its member enable and until retirement', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const b1 = await b.database.subscribe(b.path, options)
    const source = f.retained(a); const sibling = f.retained(a)
    f.emit(1)
    const a1 = await a.database.subscribe(a.path, options)
    f.emit(2)
    expect(values(source)).toEqual([['1', 1], ['2', 2]])
    expect(values(sibling)).toEqual([['1', 1], ['2', 2]])
    source.retire('closed')
    f.emit(3)
    expect(values(source)).toEqual([])
    expect(values(sibling)).toEqual([['3', 3]])
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a later ordered member receives values while the first member enable is pending', async () => {
  const f = await fixture()
  try {
    const later = f.boundary.createCharacteristic({ uuid: '00002a1a-0000-1000-8000-00805f9b34fb', readValue: () => new Uint8Array(), initialNotification: null, descriptors: [] })
    f.boundary.service.getCharacteristics = async () => [f.boundary.characteristic, later]
    const a = await f.graph('a'); const b = await f.graph('b')
    const aPaths = (await a.database.snapshot()).characteristics.map(characteristic => characteristic.path)
    const bPaths = (await b.database.snapshot()).characteristics.map(characteristic => characteristic.path)
    const b1 = await b.database.subscribe(bPaths[1], options)
    let finishStart
    f.boundary.characteristic.startNotifications = () => new Promise(resolve => { finishStart = resolve })
    const opening = f.backend.gattRuntime.acquireOrdered(a.database, [
      { memberId: 'first', path: aPaths[0], deliveryMode: 'prefer-notification' },
      { memberId: 'later', path: aPaths[1], deliveryMode: 'prefer-notification' }
    ], options)
    for (let i = 0; i < 8 && finishStart === undefined; i++) await Promise.resolve()
    expect(finishStart).toBeDefined()
    f.boundary.deliverNotification(later, new Uint8Array([7]))
    finishStart()
    const setup = await opening
    expect(setup.state).toBe('ready')
    const iterator = setup.owner.stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', memberId: 'later', ordinal: '1', value: new Uint8Array([7]) })
    await setup.owner.close(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a throwing wake does not strand callbacks on the same ingress', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress(); const c = {}; const error = new Error('wake failed')
  const first = ingress.registerRetained(new Map([[c, 'a']]), 4, 8, { wake: () => { throw error } })
  const wake = jest.fn()
  const second = ingress.registerRetained(new Map([[c, 'b']]), 4, 8, { wake })
  expect(() => ingress.admit(c, new Uint8Array([1]))).toThrow(error)
  expect(wake).toHaveBeenCalledTimes(1)
  expect(values(first)).toEqual([['1', 1]])
  expect(values(second)).toEqual([['1', 1]])
})

test('multiple throwing wakes are preserved while all terminal callbacks run', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress(); const c = {}
  const firstError = new Error('first wake'); const secondError = new Error('second wake')
  const first = ingress.registerRetained(new Map([[c, 'a']]), 4, 8, { wake: () => { throw firstError } })
  const second = ingress.registerRetained(new Map([[c, 'b']]), 4, 8, { wake: () => { throw secondError } })
  const firstTerminal = jest.fn(); const secondTerminal = jest.fn()
  first.onTerminal(firstTerminal); second.onTerminal(secondTerminal)
  let failure
  try { ingress.retire('connection-lost') } catch (error) { failure = error }
  expect(failure).toBeInstanceOf(AggregateError)
  expect(failure.errors).toEqual([firstError, secondError])
  expect(firstTerminal).toHaveBeenCalledTimes(1)
  expect(secondTerminal).toHaveBeenCalledTimes(1)
  expect(first.peek().record).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
  expect(second.peek().record).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
})

test('failed membership removals preserve every failure, deliver all terminals, and retry exact groups', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const characteristic = {}; const errors = [new Error('first removal'), new Error('second removal')]
  const registrations = new Set(); let attempts = 0
  const ingress = new WebNotificationIngress((_member, added, registration) => {
    if (added) registrations.add(registration)
    else if (attempts++ < 2) throw errors[attempts - 1]
    else registrations.delete(registration)
  }, true)
  const first = ingress.register(new Map([[characteristic, 'ordinary']]), 4, 8)
  const second = ingress.registerRetained(new Map([[characteristic, 'v1']]), 4, 8)
  const callbacks = [jest.fn(), jest.fn()]
  first.onTerminal(callbacks[0]); second.onTerminal(callbacks[1])
  let failure
  try { ingress.retire('connection-lost') } catch (error) { failure = error }
  expect(failure).toBeInstanceOf(AggregateError)
  expect(failure.errors).toEqual(errors)
  expect(callbacks.map(callback => callback.mock.calls.length)).toEqual([1, 1])
  expect(first.drain()).toEqual([{ kind: 'terminal', reason: 'connection-lost', uncertainTail: true }])
  expect(second.peek().record).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
  expect(registrations.size).toBe(2)
  ingress.retire('connection-lost')
  expect(registrations.size).toBe(0)
  expect(attempts).toBe(4)
  expect(first.drain()).toEqual([])
})

test('overflow removal failure does not skip another group and the failed group retries independently', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const characteristic = {}; const error = new Error('remove failed'); let fail = true
  const ingress = new WebNotificationIngress((_member, added) => {
    if (!added && fail) { fail = false; throw error }
  }, true)
  const first = ingress.register(new Map([[characteristic, 'first']]), 1, 8)
  const second = ingress.register(new Map([[characteristic, 'second']]), 4, 8)
  ingress.admit(characteristic, new Uint8Array([1]))
  expect(() => ingress.admit(characteristic, new Uint8Array([2]))).toThrow(error)
  expect(first.drain().map(record => record.kind)).toEqual(['value', 'loss', 'terminal'])
  expect(second.drain().map(record => [...record.value])).toEqual([[1], [2]])
  expect(() => first.retire('closed')).not.toThrow()
  expect(first.drain()).toEqual([])
  second.retire('closed')
})

test('public ordered streams both receive terminal when one ingress removal fails', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('failure-multi')
    const first = await a.database.subscribeOrdered([{ memberId: 'one', path: a.path, deliveryMode: 'prefer-notification' }], options)
    const second = await a.database.subscribeOrdered([{ memberId: 'two', path: a.path, deliveryMode: 'prefer-notification' }], options)
    const ingress = f.backend.gattRuntime.notificationIngress(a.database)
    const changed = ingress.membershipChanged; const error = new Error('remove failed'); let failed = false
    ingress.membershipChanged = (member, added, registration) => {
      if (!added && !failed) { failed = true; throw error }
      changed(member, added, registration)
    }
    expect(() => ingress.retire('connection-lost')).toThrow(error)
    const terminal = async stream => {
      const iterator = stream[Symbol.asyncIterator]()
      for (;;) {
        const item = await iterator.next()
        if (item.done) throw new Error('stream ended without terminal')
        if (item.value.kind === 'terminal') return item.value
      }
    }
    expect(await terminal(first.stream)).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
    expect(await terminal(second.stream)).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
    ingress.retire('connection-lost')
    expect(f.backend.gattRuntime.ingressMemberships.has(f.boundary.characteristic)).toBe(false)
    await first.stream.close(); await second.stream.close(); await a.lease.release()
  } finally { await f.backend.destroy() }
})

test('a throwing wake during connection fencing is reported without interrupting teardown', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const error = new Error('terminal wake failed')
    const first = f.retained(a, () => { throw error }); const wake = jest.fn(); const second = f.retained(a, wake)
    await a.lease.release()
    expect(first.terminalReason).toBe('owner-released')
    expect(second.terminalReason).toBe('owner-released')
    expect(wake).toHaveBeenCalledTimes(1)
    expectConsoleError('[WebBluetoothGattRuntime.fenceConnection] Ingress callback failed:', { errors: [{ name: 'Error' }] })
    expect(a.database.record.valid).toBe(false)
    expect(f.backend.gattRuntime.subscriptionCount()).toBe(1)
    expect(f.backend.gattRuntime.physicalNotificationCount()).toBe(1)
    await a1.remove(); await b1.remove(); await b.lease.release()
    expect(f.backend.gattRuntime.physicalNotificationCount()).toBe(0)
  } finally { await f.backend.destroy() }
})

test('a throwing wake during rediscovery is reported without preserving the old graph', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const b1 = await b.database.subscribe(b.path, options)
    const error = new Error('rediscovery wake failed')
    const old = f.retained(a, () => { throw error }); const wake = jest.fn(); const sibling = f.retained(a, wake)
    const replacement = await f.backend.gatt.discover(a.lease.connection, operation)
    expectConsoleError('[WebBluetoothGattRuntime.discover] Ingress callback failed:', { errors: [{ name: 'Error' }] })
    expect(old.terminalReason).toBe('service-changed')
    expect(sibling.terminalReason).toBe('service-changed')
    expect(wake).toHaveBeenCalledTimes(1)
    expect(a.database.record.database).toBe(replacement)
    await expect(a.database.snapshot()).rejects.toBeDefined()
    await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('synchronous sink reentry preserves order for all groups', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress(); const c = {}; const seen = []
  let nested = false
  ingress.register(new Map([[c, 'a']]), 4, 8, () => {
    if (!nested) { nested = true; ingress.admit(c, new Uint8Array([2])) }
    return true
  })
  ingress.register(new Map([[c, 'b']]), 4, 8, (_id, ordinal) => { seen.push(ordinal); return true })
  const retained = ingress.registerRetained(new Map([[c, 'c']]), 4, 8)
  ingress.admit(c, new Uint8Array([1]))
  expect(seen).toEqual(['1', '2'])
  expect(values(retained)).toEqual([['1', 1], ['2', 2]])
})

test('synchronous sink reentry preserves cross-graph and ordinary stream order', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const firstIterator = a1.values[Symbol.asyncIterator](); await firstIterator.next()
    const characteristic = [...a.database.characteristicBoundaries.values()][0]
    let nested = false
    const sink = f.backend.gattRuntime.notificationIngress(a.database).register(new Map([[characteristic, 'a']]), 4, 8, () => {
      if (!nested) { nested = true; f.emit(2) }
      return true
    })
    const source = f.retained(b)
    f.emit(1)
    expect(values(source)).toEqual([['1', 1], ['2', 2]])
    for (const iterator of [firstIterator, b1.values[Symbol.asyncIterator]()]) {
      expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([1]))
      expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([2]))
    }
    sink.retire('closed')
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a throwing sink does not prevent delivery to later recipients', () => {
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const ingress = new WebNotificationIngress(); const c = {}; const error = new Error('sink failed')
  const bad = ingress.register(new Map([[c, 'a']]), 4, 8, () => { throw error })
  const source = ingress.registerRetained(new Map([[c, 'b']]), 4, 8)
  expect(() => ingress.admit(c, new Uint8Array([1]))).toThrow(error)
  expect(bad.terminalReason).toBe('source-failed')
  expect(values(source)).toEqual([['1', 1]])
})

test('an ingress admission failure fences that ingress and still delivers to the other graph and ordinary streams', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const broken = f.retained(a); const other = f.retained(b)
    const ingress = f.backend.gattRuntime.notificationIngress(a.database)
    const original = ingress.admit; const error = new Error('admission failed')
    ingress.admit = () => { throw error }
    expect(() => f.emit(1)).toThrow(error)
    ingress.admit = original
    expect(broken.terminalReason).toBe('source-failed')
    expect(values(other)).toEqual([['1', 1]])
    for (const [index, subscription] of [a1, b1].entries()) {
      const iterator = subscription.values[Symbol.asyncIterator]()
      if (index === 0) await iterator.next()
      expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([1]))
    }
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('an ordinary listener failure does not skip later listeners or retained groups', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const source = f.retained(b); const error = new Error('listener failed')
    const member = [...f.backend.gattRuntime.subscriptions.values()].find(entry => entry.database === a.database)
    const original = member.listener
    member.listener = () => { throw error }
    expect(() => f.emit(1)).toThrow(error)
    expect(member.state).not.toBe('ready')
    member.listener = original
    expect(values(source)).toEqual([['1', 1]])
    const iterator = b1.values[Symbol.asyncIterator]()
    expect((await iterator.next()).value.value.value).toEqual(new Uint8Array([1]))
    await a1.remove(); await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})

test('a registered graph on another device cannot receive a shared characteristic callback', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('a'); const b = await f.graph('b')
    const b1 = await b.database.subscribe(b.path, options)
    const source = f.retained(a)
    const device = a.database.record.device
    a.database.record.device = {}
    f.emit(1)
    a.database.record.device = device
    expect(values(source)).toEqual([])
    f.emit(2)
    expect(values(source)).toEqual([['1', 2]])
    await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})


test('physical callback reaches V2 retained group after effectful failed membership removal', async () => {
  const f = await fixture()
  try {
    const a = await f.graph('detach-a'); const b = await f.graph('detach-b')
    const a1 = await a.database.subscribe(a.path, options); const b1 = await b.database.subscribe(b.path, options)
    const ingress = f.backend.gattRuntime.notificationIngress(a.database)
    const c = [...a.database.characteristicBoundaries.values()][0]
    const source = ingress.reserveRetainedMemberships(new Map([[c, 'member']]), 8, 128, 2).install(() => {})
    const changed = ingress.membershipChanged
    let failures = 0
    ingress.membershipChanged = (member, added, registration) => {
      changed(member, added, registration)
      if (!added && failures++ === 0) { f.emit(6); throw new Error('effectful removal failure') }
    }
    source.retire('closed')
    await a1.remove()
    expect(() => source.seal()).toThrow('effectful removal failure')
    f.emit(7)
    expect(source.peek().record).toMatchObject({ kind: 'failure-observation', sourceRejection: { bytes: '1' } })
    source.ackExact(source.peek().id)
    expect(source.peek().record).toMatchObject({ kind: 'failure-observation', sourceRejection: { bytes: '1' } })
    source.ackExact(source.peek().id)
    source.seal()
    expect(source.peek().record.kind).toBe('terminal')
    await b1.remove(); await a.lease.release(); await b.lease.release()
  } finally { await f.backend.destroy() }
})
