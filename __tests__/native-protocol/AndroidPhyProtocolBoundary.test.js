describe('Android PHY boundary mapping', () => {
  test('maps all public PHY values and rejects unknown native values', () => {
    const records = require('../../src/native-protocol/rn-android-protocol-records')

    expect(records.nativePhyFromPublic('le-1m')).toBe('le1m')
    expect(records.nativePhyFromPublic('le-2m')).toBe('le2m')
    expect(records.nativePhyFromPublic('le-coded')).toBe('leCoded')
    expect(records.publicPhyFromNative('le1m')).toBe('le-1m')
    expect(records.publicPhyFromNative('le2m')).toBe('le-2m')
    expect(records.publicPhyFromNative('leCoded')).toBe('le-coded')
    expect(() => records.publicPhyFromNative('legacy')).toThrow()
  })

  test('declares PHY only when the Android boundary has concrete methods', () => {
    const androidSource = require('../../src/native-protocol/rn-android-boundary')
    const appleSource = require('../../src/native-protocol/rn-apple-boundary')

    expect(androidSource.ReactNativeAndroidProtocolBoundary.prototype.readPhy).toBeDefined()
    expect(androidSource.ReactNativeAndroidProtocolBoundary.prototype.requestPhy).toBeDefined()
    expect(appleSource.ReactNativeAppleProtocolBoundary.toString()).toContain('requestMtu')
  })
})
