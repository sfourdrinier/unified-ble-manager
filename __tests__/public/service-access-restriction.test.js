'use strict'

const { serviceAccessRestriction } = require('../../src/backend-contract/gatt')

describe('service access restriction', () => {
  test('access denied stays distinct from a missing service', () => {
    expect(serviceAccessRestriction('access-denied')).toEqual({
      state: 'restricted',
      reason: 'access-denied',
      gattStatus: 'access-denied',
      attError: null
    })
  })

  test('a reserved service is recorded without an ATT status', () => {
    expect(serviceAccessRestriction('os-reserved')).toEqual({
      state: 'restricted',
      reason: 'os-reserved',
      gattStatus: null,
      attError: null
    })
  })

  test('an open service has no restriction', () => {
    expect(serviceAccessRestriction(undefined)).toBeUndefined()
    expect(serviceAccessRestriction(null)).toBeUndefined()
    expect(serviceAccessRestriction('open')).toBeUndefined()
  })
})
