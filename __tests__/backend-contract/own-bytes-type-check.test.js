// __tests__/backend-contract/own-bytes-type-check.test.js

const { byteLimit, ownBytes } = require('../../src/backend-contract/primitives')

describe('ownBytes type check', () => {
  test('rejects non-Uint8Array inputs instead of coercing them', () => {
    const maximum = byteLimit(512)
    const coerced = [[1, 2, 3], 'hi', 3, { 0: 1, 1: 2 }, null, undefined, new DataView(new ArrayBuffer(2))]
    for (const input of coerced) {
      expect(() => ownBytes(input, maximum)).toThrow(
        expect.objectContaining({ normalized: expect.objectContaining({ code: 'bytes.invalid' }) })
      )
    }
  })

  test('reports the boundary domain and own-bytes operation', () => {
    try {
      ownBytes([1, 2, 3], byteLimit(512))
      throw new Error('unreachable')
    } catch (error) {
      expect(error.normalized).toMatchObject({
        code: 'bytes.invalid',
        domain: 'boundary',
        operation: 'primitives.own-bytes'
      })
    }
  })

  test('still copies valid inputs and enforces the byte limit', () => {
    const maximum = byteLimit(4)
    const source = new Uint8Array([1, 2, 3])
    const owned = ownBytes(source, maximum)
    expect(Array.from(owned)).toEqual([1, 2, 3])
    source[0] = 9
    expect(owned[0]).toBe(1)
    expect(() => ownBytes(new Uint8Array([1, 2, 3, 4, 5]), maximum)).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'bytes.too-large' }) })
    )
  })
})
