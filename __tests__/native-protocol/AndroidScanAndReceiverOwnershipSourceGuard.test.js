const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')
const radioPath = path.join(
  root,
  'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'
)
const gradlePath = path.join(root, 'android/build.gradle')
const cmakePath = path.join(root, 'android/src/main/jni/CMakeLists.txt')

describe('Android scan and receiver ownership source guards', () => {
  const radio = fs.readFileSync(radioPath, 'utf8')
  const gradle = fs.readFileSync(gradlePath, 'utf8')
  const cmake = fs.readFileSync(cmakePath, 'utf8')

  test('owns the scan callback before native admission and retains failed compensation', () => {
    const startScan = radio.slice(radio.indexOf('fun startScan('), radio.indexOf('internal fun stopScan()'))
    const assign = startScan.indexOf('scanCallback = cb')
    const nativeStart = startScan.indexOf('scanner?.startScan')
    expect(nativeStart).toBeGreaterThan(-1)
    expect(assign).toBeGreaterThan(-1)
    expect(assign).toBeLessThan(nativeStart)
    expect(startScan).toContain('if (cleanupFailure == null)')
    expect(startScan).toContain('scanCallback = null')
    expect(startScan).toMatch(/catch\s*\(/)
    expect(startScan).toMatch(/stopScan\(cb\)/)
    expect(startScan).toMatch(/setDeviceAddress/)
  })

  test('commits adapter registration and retains the security receiver across partial admission', () => {
    const adapter = radio.slice(
      radio.indexOf('fun registerAdapterStateReceiver()'),
      radio.indexOf('internal fun unregisterAdapterStateReceiver()')
    )
    expect(adapter.indexOf('context.registerReceiver')).toBeGreaterThan(-1)
    expect(adapter.indexOf('adapterStateReceiver = receiver')).toBeGreaterThan(
      adapter.indexOf('context.registerReceiver')
    )

    const bond = radio.slice(
      radio.indexOf('internal fun registerBondStateReceiver()'),
      radio.indexOf('internal fun unregisterBondStateReceiver()')
    )
    expect(bond.indexOf('context.registerReceiver')).toBeGreaterThan(-1)
    expect(bond.indexOf('bondStateReceiver = receiver')).toBeGreaterThan(-1)
    expect(bond.indexOf('bondStateReceiver = receiver')).toBeLessThan(bond.indexOf('context.registerReceiver'))
    expect(bond).toContain('securityReceiverActive = false')
    expect(bond).toContain('throw error')
  })

  test('native protocol shared object is linked at 16 KB page size', () => {
    expect(cmake).toMatch(/-Wl,-z,max-page-size=16384/)
    expect(cmake).toMatch(/-Wl,-z,common-page-size=16384/)
    expect(gradle).toMatch(/ndkVersion/)
    expect(gradle).toMatch(/27\.1\.12297006/)
  })
})
