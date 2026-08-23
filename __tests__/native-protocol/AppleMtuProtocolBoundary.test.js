const { ReactNativeAppleProtocolBoundary } = require('../../src/native-protocol/rn-apple-boundary')

describe('React Native Apple MTU protocol boundary', () => {
  test('rejects effective MTU requests without dispatching Android readMtu', async () => {
    const boundary = new ReactNativeAppleProtocolBoundary({}, 'test-owner')
    const dispatchSpy = jest.spyOn(boundary, 'dispatch')

    await expect(boundary.effectiveMtu('peer-id')).rejects.toMatchObject({
      normalized: {
        code: 'capability.unsupported',
        operation: 'rn-apple-boundary.effective-mtu'
      }
    })
    expect(dispatchSpy).not.toHaveBeenCalled()
    expect(boundary.connectionControlCapabilities.effectiveMtu).toBe('unavailable')
  })
})
