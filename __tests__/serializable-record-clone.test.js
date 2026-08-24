const { snapshotSerializableRecord, serializableRecordsEqual } = require('../src/backend-contract/serializable')
const { encodeTauriWireValue, decodeTauriWireValue } = require('../src/tauri/transport')

function recordWithKey(key, value) {
  return JSON.parse(`{${JSON.stringify(key)}:${JSON.stringify(value)}}`)
}

describe('serializable record clone safety', () => {
  test('rejects __proto__, constructor, and prototype keys at the top level', () => {
    for (const key of ['__proto__', 'constructor', 'prototype']) {
      const record = recordWithKey(key, 'x')
      expect(() => snapshotSerializableRecord(record)).toThrow(/protocol\.malformed/)
      expect(() => encodeTauriWireValue(record)).toThrow(/protocol\.malformed/)
      expect(() => decodeTauriWireValue(record)).toThrow(/protocol\.malformed/)
    }
  })

  test('rejects those keys in nested bootstrap, route, event, and platform-error metadata', () => {
    const nested = {
      bootstrap: { attachment: { adapter: { state: recordWithKey('__proto__', { polluted: true }) } } },
      route: { payload: { constructor: { steal: 1 } } },
      event: { item: { prototype: { x: 1 } } },
      platform: { metadata: recordWithKey('__proto__', 'x') }
    }
    expect(() => snapshotSerializableRecord(nested.bootstrap)).toThrow(/protocol\.malformed/)
    expect(() => encodeTauriWireValue(nested.route)).toThrow(/protocol\.malformed/)
    expect(() => decodeTauriWireValue(nested.event)).toThrow(/protocol\.malformed/)
    expect(() => snapshotSerializableRecord(nested.platform)).toThrow(/protocol\.malformed/)
  })

  test('rejects custom prototypes while accepting Object.prototype and null-prototype records', () => {
    expect(() => snapshotSerializableRecord(Object.create({ inherited: 1 }))).toThrow(/protocol\.malformed/)
    const ordinary = { a: 1 }
    const nulled = Object.assign(Object.create(null), { a: 1 })
    expect(snapshotSerializableRecord(ordinary).value.a).toBe(1)
    expect(snapshotSerializableRecord(nulled).value.a).toBe(1)
    expect(Object.getPrototypeOf(snapshotSerializableRecord(ordinary).value)).toBe(null)
  })

  test('snapshot byteLength equals own enumerable data only', () => {
    const record = { a: 'x' }
    const snapshot = snapshotSerializableRecord(record)
    expect(snapshot.byteLength).toBe(new TextEncoder().encode(JSON.stringify(snapshot.value)).byteLength)
    expect(Object.getOwnPropertyNames(snapshot.value).sort()).toEqual(['a'])
  })

  test('Uint8Array byte wrappers remain siblings and are not walked as records', () => {
    const bytes = new Uint8Array([1, 2, 3])
    const encoded = encodeTauriWireValue({ bytes, label: 'ok' })
    expect(Object.keys(encoded).sort()).toEqual(['bytes', 'label'])
    expect(encoded.bytes).toEqual({ $__unifiedBleBytesV2: [1, 2, 3] })
    expect(decodeTauriWireValue(encoded).bytes).toEqual(bytes)
  })

  test('output snapshot prototype is null and Object.keys sees only own data', () => {
    const snapshot = snapshotSerializableRecord({ a: 1, b: 'two' })
    expect(Object.getPrototypeOf(snapshot.value)).toBe(null)
    expect(Object.keys(snapshot.value).sort()).toEqual(['a', 'b'])
    expect(Object.prototype.hasOwnProperty.call(snapshot.value, 'constructor')).toBe(false)
  })

  test('ordinary nested arrays null-prototype records bytes and serializable equality remain compatible', () => {
    const nested = { items: [1, { inner: 'ok' }, null], flag: true }
    const snapshot = snapshotSerializableRecord(nested)
    expect(Object.getPrototypeOf(snapshot.value)).toBe(null)
    expect(Object.getPrototypeOf(snapshot.value.items[1])).toBe(null)
    expect(serializableRecordsEqual(snapshot.value, snapshotSerializableRecord(nested).value)).toBe(true)
    expect(snapshot.byteLength).toBe(
      new TextEncoder().encode(JSON.stringify(snapshot.value)).byteLength
    )
  })

  test('rejects a byte-tag object with sibling keys including a forbidden key', () => {
    const tagged = JSON.parse('{"$__unifiedBleBytesV2":[1],"__proto__":{"polluted":true}}')
    expect(() => decodeTauriWireValue(tagged)).toThrow()
  })
})
