const fs = require('node:fs')
const path = require('node:path')
const root = path.resolve(__dirname, '..')

test('desktop prebuild authority contains only the production shared Rust backend', () => {
  const { NATIVE_PREBUILD_TARGETS } = require('../scripts/native-prebuilds/targets')
  expect(NATIVE_PREBUILD_TARGETS.map(target => `${target.backend}/${target.platform}/${target.arch}`)).toEqual([
    'desktop-core/linux/x64',
    'desktop-core/linux/arm64',
    'desktop-core/darwin/arm64',
    'desktop-core/win32/x64',
    'desktop-core/win32/arm64'
  ])
})

test('unreachable addon sources, loaders and exclusive published build dependencies are retired together', () => {
  for (const retired of [
    'native/electron/corebluetooth/index.js',
    'native/electron/winrt/index.js',
    'src/backends/corebluetooth/corebluetooth-native-boundary.ts',
    'src/backends/winrt/winrt-native-boundary.ts',
    'src/backends/legacy-native-require.ts'
  ]) {
    expect(fs.existsSync(path.join(root, retired))).toBe(false)
  }
  const manifest = require('../package.json')
  expect(manifest.files).toContain('!native/electron')
  expect(manifest.optionalDependencies ?? {}).not.toHaveProperty('node-addon-api')
  expect(manifest.optionalDependencies ?? {}).not.toHaveProperty('node-gyp')
  for (const workflow of ['ci.yml', 'publish.yml']) {
    expect(fs.readFileSync(path.join(root, '.github/workflows', workflow), 'utf8')).not.toContain('native/electron/')
  }
})

test('packed artifacts cannot resurrect retired desktop producers or their exclusive dependencies', () => {
  const { assertNoRetiredDesktopProducers } = require('../scripts/ci/verify-package-tarballs')
  expect(() => assertNoRetiredDesktopProducers(new Map(), {})).not.toThrow()
  for (const entry of [
    'package/native/electron/corebluetooth/src/addon.mm',
    'package/native/electron/winrt/index.js'
  ]) {
    expect(() => assertNoRetiredDesktopProducers(new Map([[entry, Buffer.from('fixture')]]), {})).toThrow(
      'Retired desktop producer'
    )
  }
  for (const dependency of ['node-addon-api', 'node-gyp']) {
    for (const field of ['dependencies', 'optionalDependencies']) {
      expect(() => assertNoRetiredDesktopProducers(new Map(), { [field]: { [dependency]: 'fixture' } })).toThrow(
        'Retired desktop build dependency'
      )
    }
  }
})

test('packed acceptance exercises maintained Rust producers rather than poisoning absent legacy loaders', () => {
  const proof = fs.readFileSync(path.join(root, 'scripts/ci/f01-packed-dispatch-proof.js'), 'utf8')
  expect(proof).toContain('native[/\\\\]desktop-core[/\\\\]prebuilds[/\\\\]')
  const acceptance = fs.readFileSync(path.join(root, 'scripts/ci/napi-clean-tarball-acceptance.js'), 'utf8')
  expect(acceptance).toContain("leg: 'retired producers absent'")
  expect(acceptance).not.toContain("leg: 'legacy loaders poisoned'")
})

test('current distribution and support guidance describes the maintained 5.x architecture', () => {
  const guide = fs.readFileSync(path.join(root, 'docs/ELECTRON.md'), 'utf8')
  expect(guide).not.toContain('remain in source until the Rust path')
  expect(guide).toContain('Only the shared Rust desktop core is produced')
  const support = fs.readFileSync(path.join(root, 'SUPPORT.md'), 'utf8')
  expect(support).toContain('support targets the current 5.x release line')
  expect(support).not.toContain('establishes the stable 4.x package/API contract')
  const changelog = fs.readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
  expect(changelog).toContain('### Before upgrading')
  expect(changelog).toContain('native prebuild alone does not install or configure the daemon')
})
