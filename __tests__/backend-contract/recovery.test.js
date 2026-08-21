const { BLE_ERROR_CODES } = require('../../src/backend-contract/errors')
const { recoveryForCode } = require('../../src/backend-contract/recovery')

describe('backend recovery catalog', () => {
  test('provides deterministic recovery metadata for representative contract codes', () => {
    expect(recoveryForCode('connection.failed', 'recovery.test')).toEqual({
      disposition: 'retry-with-backoff',
      actions: [{ kind: 'reconnect' }]
    })
    expect(recoveryForCode('capability.limited', 'recovery.test')).toEqual({
      disposition: 'none',
      actions: []
    })
    expect(recoveryForCode('permission.denied', 'recovery.test')).toEqual({
      disposition: 'after-user-action',
      actions: [
        { kind: 'request-permission', permission: 'recovery.test' },
        { kind: 'open-settings', target: 'app' }
      ]
    })
  })

  test('has a recovery entry for every canonical error code', () => {
    expect(BLE_ERROR_CODES).toHaveLength(67)
    for (const code of BLE_ERROR_CODES) {
      const recovery = recoveryForCode(code, 'recovery.test')
      expect(recovery).toHaveProperty('disposition')
      expect(Array.isArray(recovery.actions)).toBe(true)
    }
  })
})
