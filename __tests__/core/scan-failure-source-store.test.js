const { ScanFailureSourceStore } = require('../../src/core/scan-failure-source-store')

test('reserves source capacity before capture and reconstructs immutable evidence for independent recipients', () => {
  const store = new ScanFailureSourceStore({ byteCapacity: 16, occurrenceCapacity: 1, recipientCapacity: 2, chunkCapacity: 3 })
  const identity = { source: 'adapter', generation: '7' }
  const reservation = store.reserve(identity, 8, 2)
  expect(reservation).not.toBeNull()
  expect(store.inspect()).toMatchObject({ reservedBytes: 8, occurrences: 1, recipients: 2 })
  expect(store.reserve(identity, 1, 1)).toBeNull()
  identity.generation = 'changed'
  const input = Uint8Array.from([0, 1, 2, 3, 4, 5, 6, 7])
  const [first, second] = reservation.capture(input)
  expect(() => reservation.close()).toThrow()
  input.fill(99)
  expect(first.identity).toEqual({ source: 'adapter', generation: '7' })
  expect(Object.isFrozen(first.identity)).toBe(true)
  expect([...first.read(0, 3), ...first.read(3, 3), ...first.read(6, 3)]).toEqual([0, 1, 2, 3, 4, 5, 6, 7])
  expect([...first.read(3, 3)]).toEqual([3, 4, 5])
  const chunk = first.read(0, 3)
  chunk[0] = 44
  expect([...first.read(0, 3)]).toEqual([0, 1, 2])
  expect(() => first.read(0, 4)).toThrow()
  first.release()
  first.release()
  expect(() => first.read(0, 1)).toThrow()
  expect([...second.read(0, 2)]).toEqual([0, 1])
  expect(store.inspect()).toMatchObject({ reservedBytes: 8, occurrences: 1, recipients: 1 })
  second.release()
  expect(store.inspect()).toMatchObject({ reservedBytes: 0, occurrences: 0, recipients: 0 })
})

test('oversize capture preserves the reservation for retry and close releases unused capacity', () => {
  const store = new ScanFailureSourceStore({ byteCapacity: 4, occurrenceCapacity: 2, recipientCapacity: 2, chunkCapacity: 2 })
  const reservation = store.reserve({ source: 'adapter', generation: '1' }, 4, 1)
  expect(() => reservation.capture(new Uint8Array(5))).toThrow()
  expect(store.inspect()).toMatchObject({ reservedBytes: 4, occurrences: 1, recipients: 1 })
  const [handle] = reservation.capture(Uint8Array.from([7, 8]))
  expect([...handle.read(0, 2)]).toEqual([7, 8])
  expect(() => reservation.capture(new Uint8Array(1))).toThrow()
  handle.release()
  const unused = store.reserve({ source: 'adapter', generation: '2' }, 4, 1)
  unused.close()
  unused.close()
  expect(store.inspect()).toMatchObject({ reservedBytes: 0, occurrences: 0, recipients: 0 })
  expect(() => unused.capture(new Uint8Array(1))).toThrow()
})

test('changing caller configuration cannot enlarge the reserved store or chunk limit', () => {
  const options = { byteCapacity: 4, occurrenceCapacity: 1, recipientCapacity: 1, chunkCapacity: 2 }
  const store = new ScanFailureSourceStore(options)
  options.byteCapacity = 100
  options.chunkCapacity = 100
  expect(store.reserve({ source: 'adapter', generation: '1' }, 5, 1)).toBeNull()
  const reservation = store.reserve({ source: 'adapter', generation: '1' }, 4, 1)
  const [handle] = reservation.capture(new Uint8Array(4))
  expect(() => handle.read(0, 3)).toThrow()
  handle.release()
})

test('chunked evidence reconstructs duplicate metadata and long failure detail exactly', () => {
  const original = { source: 'adapter', generation: '1', error: { detail: 'failure '.repeat(150), platform: { metadataEntries: [
    { key: 'duplicate', value: { kind: 'text', value: 'first' } },
    { key: 'duplicate', value: { kind: 'text', value: 'second' } }
  ] } } }
  const encoded = Buffer.from(JSON.stringify(original))
  const store = new ScanFailureSourceStore({ byteCapacity: encoded.length, occurrenceCapacity: 1, recipientCapacity: 1, chunkCapacity: 127 })
  const reservation = store.reserve({ source: original.source, generation: original.generation }, encoded.length, 1)
  const [handle] = reservation.capture(encoded)
  const chunks = []
  for (let offset = 0; offset < handle.byteLength; offset += 127) chunks.push(handle.read(offset, 127))
  expect(JSON.parse(Buffer.concat(chunks).toString())).toEqual(original)
  handle.release()
})

test('only declared limits are retained from caller configuration', () => {
  const store = new ScanFailureSourceStore({ byteCapacity: 4, occurrenceCapacity: 1, recipientCapacity: 1, chunkCapacity: 2, unrelated: 'large caller-owned data' })
  expect(store.inspect().limits).toEqual({ byteCapacity: 4, occurrenceCapacity: 1, recipientCapacity: 1, chunkCapacity: 2 })
})
