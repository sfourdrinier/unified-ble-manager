const { canonicalUuidInput } = require('../src/backend-contract/primitives')

describe('public numeric UUID input', () => {
  test('pads a numeric 32-bit UUID with a leading zero like its textual form', () => {
    expect(canonicalUuidInput(0x01234567)).toBe(canonicalUuidInput('01234567'))
    expect(canonicalUuidInput(0x180d)).toBe(canonicalUuidInput('180d'))
  })

  test.each([-1, 0x100000000, NaN, Infinity, 1.5])('rejects invalid numeric UUID %s', value => {
    expect(() => canonicalUuidInput(value)).toThrow()
  })
})
