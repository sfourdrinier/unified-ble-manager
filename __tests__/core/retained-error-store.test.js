const { RetainedErrorStore } = require('../../src/core/bounded-stream')

describe('bounded retained error evidence', () => {
  test('retains an immutable snapshot, retries chunks, and releases its exact charge', () => {
    const store = new RetainedErrorStore(8, 2, 3)
    const input = Uint8Array.from([1, 2, 3, 4, 5])
    const handle = store.retain(input)
    expect(handle).not.toBeNull()
    expect(store.retainedBytes()).toBe(5)
    input[0] = 9
    const first = handle.read(0, 3)
    expect([...first]).toEqual([1, 2, 3])
    first[0] = 8
    expect([...handle.read(0, 3)]).toEqual([1, 2, 3])
    expect([...handle.read(3, 3)]).toEqual([4, 5])
    expect(handle.byteLength).toBe(5)
    handle.release()
    handle.release()
    expect(store.retainedBytes()).toBe(0)
    expect(() => handle.read(0, 1)).toThrow()
  })

  test('refuses before copying if byte or item capacity is unavailable and recovers after release', () => {
    const store = new RetainedErrorStore(6, 1, 4)
    const input = Uint8Array.from([4, 5, 6, 7])
    const first = store.retain(input)
    expect(store.retain(input)).toBeNull()
    expect(store.retainedBytes()).toBe(4)
    first.release()
    expect(store.retain(new Uint8Array(7))).toBeNull()
    expect(store.retainedBytes()).toBe(0)
    const second = store.retain(input)
    expect([...second.read(0, 4)]).toEqual([...input])
    second.release()
  })

  test('rejects invalid bounds and shared-memory input without retaining it', () => {
    const store = new RetainedErrorStore(6, 2, 4)
    const handle = store.retain(Uint8Array.from([1, 2]))
    for (const [offset, size] of [[-1, 1], [3, 1], [0, 0], [0, 5], [0.5, 1]]) {
      expect(() => handle.read(offset, size)).toThrow()
    }
    if (typeof SharedArrayBuffer !== 'undefined') {
      expect(() => store.retain(new Uint8Array(new SharedArrayBuffer(1)))).toThrow()
    }
    expect(store.retainedBytes()).toBe(2)
    handle.release()
  })

  test('recovers oversized evidence in bounded retryable chunks under a separate budget', () => {
    const evidence = Uint8Array.from({ length: 2881 }, (_, i) => (i * 73 + (i >> 4)) % 256)
    const store = new RetainedErrorStore(3000, 2, 128)
    const handle = store.retain(evidence)
    expect(store.retainedBytes()).toBe(2881)
    const chunks = []
    for (let offset = 0; offset < handle.byteLength; offset += 128) {
      const chunk = handle.read(offset, 128)
      expect(chunk.length).toBeLessThanOrEqual(128)
      if (offset === 128) expect(handle.read(offset, 128)).toEqual(chunk)
      chunks.push(...chunk)
    }
    expect(chunks).toEqual([...evidence])
    handle.release()
    expect(store.retainedBytes()).toBe(0)
  })
})
