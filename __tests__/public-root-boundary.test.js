describe('application root boundary', () => {
  test('does not expose internal façade constructors or host identity machinery', () => {
    const root = require('../src/index')

    expect(root.ApplicationBleManager).toBeUndefined()
    expect(root.createPublicBleManager).toBeUndefined()
    expect(root.deriveRestorationIdentity).toBeUndefined()
    expect(root.createEphemeralHostIdentity).toBeUndefined()
    expect(root.normalizeBleManagerCreateOptions).toBeUndefined()
  })
})
