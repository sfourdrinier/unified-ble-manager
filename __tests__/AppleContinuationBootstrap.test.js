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

  it('admits startup centrals from actual restoration launch identity rather than ASK declarations alone', () => {
    const launch = read('ios/UnifiedBleContinuationBootstrap.mm')
    expect(launch).toContain('UIApplicationLaunchOptionsBluetoothCentralsKey')
    expect(launch).toContain('recordNativeRestorationLaunchIdentifiers')
    const sessions = read('ios/UnifiedBleRustCoreSessions.swift')
    expect(sessions).toContain('shouldCreateStartupCentral')
    expect(sessions).toContain('resumeAuthorizedAccessoryStartup')
    expect(sessions.indexOf('if let failure = ensureHost() { return failure }')).toBeLessThan(
      sessions.indexOf('resumeAuthorizedAccessoryStartup(configuration)')
    )
    expect(sessions).toContain('accessoryStartupQueryInFlight')
    expect(sessions).toContain('if let startupFailure { return completion(nil, startupFailure) }')
    const support = read('ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift')
    expect(support).toContain('queryAuthorizedAccessories')
    expect(support).toContain('accessory.state == .authorized')
    expect(support).toContain('accessorySetupConfigured(info: Bundle.main.infoDictionary')
  })

  it('serializes startup central allocation with permission and radio work, without an on-queue sync deadlock', () => {
    const source = read('ios/UnifiedBleRustCoreSessions.swift')
    const install = source.slice(
      source.indexOf('static func installProductionHost('),
      source.indexOf('// MARK: - Sessions')
    )
    expect(install).toMatch(
      /dispatchPrecondition\(condition: \.notOnQueue\(radio\.queue\)\)[\s\S]*radio\.queue\.sync\s*\{\s*_ = radio\.ensureCentral\(\)\s*\}/
    )
    expect(install.match(/radio\.ensureCentral\(\)/g)).toHaveLength(1)
    expect(install).toMatch(
      /restorationConfigured\([\s\S]*?\)\s*&&\s*OwnedCoreBluetoothProtocolRadioSupport\.shouldCreateStartupCentral/
    )
    // Restored callbacks may run on the radio queue only after bind; the
    // installed host is returned before the installer can be entered again.
    const host = source.slice(
      source.indexOf('private func installedHost()'),
      source.indexOf('private func existingHost()')
    )
    expect(host.indexOf('if let host { return host }')).toBeLessThan(host.indexOf('try installer(self)'))
    expect(install.indexOf('adapter.bind(sink: host)')).toBeGreaterThan(
      install.indexOf('radio.attachDelegateIfAvailable(adapter)')
    )
  })

  it('compiles the real launch observer against the generated Swift interface in the Apple lane', () => {
    const lane = read('scripts/native-protocol/test-apple-native-protocol.js')
    expect(lane).toContain('iphonesimulator')
    expect(lane).toContain('BlePlx-Swift.h')
    expect(lane).toContain('ios/UnifiedBleContinuationBootstrap.mm')
    expect(lane).toContain('OBJC_CLASS_$_UnifiedBleContinuationBootstrap')
  })
})
