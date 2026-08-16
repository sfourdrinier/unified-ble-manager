const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

describe('native Node-API prebuild distribution', () => {
  test('defines the complete maintained desktop prebuild matrix from one authority', () => {
    const targetsPath = path.join(root, 'scripts', 'native-prebuilds', 'targets.js')

    expect(fs.existsSync(targetsPath)).toBe(true)
    const { NODE_API_VERSION, NATIVE_PREBUILD_TARGETS } = require(targetsPath)

    expect(NODE_API_VERSION).toBe(8)
    expect(
      NATIVE_PREBUILD_TARGETS.map(({ backend, platform, arch, runner }) => ({
        backend,
        platform,
        arch,
        runner
      }))
    ).toEqual([
      { backend: 'corebluetooth', platform: 'darwin', arch: 'arm64', runner: 'macos-15' },
      { backend: 'corebluetooth', platform: 'darwin', arch: 'x64', runner: 'macos-15-intel' },
      { backend: 'winrt', platform: 'win32', arch: 'arm64', runner: 'windows-11-arm' },
      { backend: 'winrt', platform: 'win32', arch: 'x64', runner: 'windows-2025' }
    ])
    expect(new Set(NATIVE_PREBUILD_TARGETS.map(target => target.artifactName)).size).toBe(4)
    expect(new Set(NATIVE_PREBUILD_TARGETS.map(target => target.prebuildPath)).size).toBe(4)
  })

  test('loads platform prebuilds first while preserving an explicit source-build fallback', () => {
    const sharedLoader = read('native/load-node-api-addon.js')
    const coreBluetoothLoader = read('native/electron/corebluetooth/index.js')
    const winRtLoader = read('native/electron/winrt/index.js')

    expect(sharedLoader).toContain("'prebuilds', `${platform}-${arch}`")
    expect(sharedLoader).toContain("'build', 'Release'")
    expect(sharedLoader.indexOf("'prebuilds', `${platform}-${arch}`")).toBeLessThan(
      sharedLoader.indexOf("'build', 'Release'")
    )
    expect(coreBluetoothLoader).toContain("require('../../load-node-api-addon')")
    expect(winRtLoader).toContain("require('../../load-node-api-addon')")
    expect(coreBluetoothLoader).toContain("addonName: 'unified_ble_corebluetooth'")
    expect(winRtLoader).toContain("addonName: 'unified_ble_winrt'")
  })

  test('pins a compatible Node-API floor in both native build definitions', () => {
    for (const binding of [
      'native/electron/corebluetooth/binding.gyp',
      'native/electron/winrt/binding.gyp'
    ]) {
      expect(read(binding)).toContain('NAPI_VERSION=8')
    }
  })

  test('builds every prebuild before the trusted-publishing tarball is created', () => {
    const pkg = require('../package.json')
    const publish = read('.github/workflows/publish.yml')

    expect(pkg.files).not.toContain('!native/**/*.node')
    expect(pkg.scripts['native-prebuild:build']).toContain('scripts/native-prebuilds/build.js')
    expect(pkg.scripts['native-prebuild:verify']).toContain('scripts/native-prebuilds/verify.js')
    expect(publish).toContain('native-prebuild-plan:')
    expect(publish).toContain('native-prebuild:')
    expect(publish).toContain('actions/upload-artifact@v7')
    expect(publish).toContain('include-hidden-files: true')
    expect(publish).toContain('actions/download-artifact@v8')
    expect(publish).toContain("pattern: 'native-prebuild-*'")
    expect(publish).toContain('pnpm native-prebuild:verify --require-all --write-manifest')
    expect(publish.indexOf('pnpm native-prebuild:verify --require-all --write-manifest')).toBeLessThan(
      publish.indexOf('PACK_OUTPUT="$(npm pack --pack-destination .release-package)"')
    )
  })
})
