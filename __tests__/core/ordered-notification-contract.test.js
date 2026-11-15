// __tests__/core/ordered-notification-contract.test.js

const { isCanonicalIngressOrdinal } = require('../../src/core/ordered-notification-contract')

describe('ordered notification ingress ordinal', () => {
  test.each(['0', '1', '9007199254740992', '18446744073709551615'])(
    'accepts canonical unsigned 64-bit decimal %s without numeric conversion',
    value => {
      expect(isCanonicalIngressOrdinal(value)).toBe(true)
    }
  )

  test.each([
    '',
    '00',
    '01',
    '-1',
    '+1',
    ' 1',
    '1 ',
    '1.0',
    '1e3',
    '１２',
    '18446744073709551616',
    '999999999999999999999',
    1,
    null,
    undefined,
    {}
  ])('rejects malformed or out-of-range input %p', value => {
    expect(isCanonicalIngressOrdinal(value)).toBe(false)
  })
})
