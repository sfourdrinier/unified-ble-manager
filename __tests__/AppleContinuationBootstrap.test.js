const fs = require('node:fs')
const path = require('node:path')

const read = name => fs.readFileSync(path.join(__dirname, '..', name), 'utf8')

describe('Apple native continuation launch wiring', () => {
  it('packs the launch observer in the actual CocoaPods source selection', () => {
    expect(read('unified-ble-manager.podspec')).toContain('"ios/UnifiedBleContinuationBootstrap.mm"')
  })

  it('starts from the native launch notification without requiring a JS module', () => {
    const source = read('ios/UnifiedBleContinuationBootstrap.mm')
    expect(source).toContain('UIApplicationDidFinishLaunchingNotification')
    expect(source).toContain('bootstrapNativeContinuation')
    expect(source).not.toMatch(/RCT_EXPORT_MODULE|RCTBridge|startReactNative/)
  })

  it('compiles the real launch observer against the generated Swift interface in the Apple lane', () => {
    const lane = read('scripts/native-protocol/test-apple-native-protocol.js')
    expect(lane).toContain('iphonesimulator')
    expect(lane).toContain('BlePlx-Swift.h')
    expect(lane).toContain('ios/UnifiedBleContinuationBootstrap.mm')
    expect(lane).toContain('OBJC_CLASS_$_UnifiedBleContinuationBootstrap')
  })
})
