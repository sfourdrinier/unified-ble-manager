const fs = require('fs')
const path = require('path')
const YAML = require('yaml')
const { execFileSync } = require('node:child_process')

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
      { backend: 'desktop-core', platform: 'linux', arch: 'x64', runner: 'ubuntu-22.04' },
      { backend: 'desktop-core', platform: 'linux', arch: 'arm64', runner: 'ubuntu-22.04-arm' },
      { backend: 'desktop-core', platform: 'darwin', arch: 'arm64', runner: 'macos-15' },
      { backend: 'desktop-core', platform: 'win32', arch: 'x64', runner: 'windows-2025' },
      { backend: 'desktop-core', platform: 'win32', arch: 'arm64', runner: 'windows-11-arm' }
    ])
    expect(NATIVE_PREBUILD_TARGETS).toHaveLength(5)
    expect(new Set(NATIVE_PREBUILD_TARGETS.map(target => target.artifactName)).size).toBe(
      NATIVE_PREBUILD_TARGETS.length
    )
    expect(new Set(NATIVE_PREBUILD_TARGETS.map(target => target.prebuildPath)).size).toBe(
      NATIVE_PREBUILD_TARGETS.length
    )
    for (const target of NATIVE_PREBUILD_TARGETS.filter(entry => entry.backend === 'desktop-core')) {
      expect(target.rustTarget).toEqual(expect.any(String))
      expect(target.sidecarPath).toEqual(expect.any(String))
    }
  })

  test('GitHub matrix derives its maintained targets without Intel macOS runners', () => {
    const { NATIVE_PREBUILD_TARGETS } = require('../scripts/native-prebuilds/targets')
    const matrix = JSON.parse(
      execFileSync(process.execPath, [path.join(root, 'scripts/native-prebuilds/print-github-matrix.js')], {
        encoding: 'utf8'
      })
    )
    expect(matrix.include).toHaveLength(NATIVE_PREBUILD_TARGETS.length)
    expect(matrix.include.map(row => row.artifactName)).toEqual(NATIVE_PREBUILD_TARGETS.map(row => row.artifactName))
    expect(matrix.include.every(row => row.platform !== 'darwin' || row.arch === 'arm64')).toBe(true)
    expect(matrix.include.some(row => row.runner === 'macos-15-intel')).toBe(false)
    expect(matrix.include.filter(row => row.arch === 'x64').map(row => row.platform)).toEqual(['linux', 'win32'])
  })

  test.each([
    'native/desktop-core/prebuilds/darwin-x64/ubm_desktop_core.node',
    'native/electron/corebluetooth/prebuilds/darwin-x64/unified_ble_corebluetooth.node'
  ])('release verification rejects a staged retired Intel macOS artifact: %s', retired => {
    const { main } = require('../scripts/native-prebuilds/verify')
    const segments = retired.split('/').slice(1)
    const entries = new Map()
    let directory = path.join(root, 'native')
    for (const [index, name] of segments.entries()) {
      const isDirectory = index !== segments.length - 1
      entries.set(directory, [{ name, isDirectory: () => isDirectory, isFile: () => !isDirectory }])
      directory = path.join(directory, name)
    }
    const existing = jest.spyOn(fs, 'existsSync').mockImplementation(file => entries.has(file))
    const listing = jest.spyOn(fs, 'readdirSync').mockImplementation(file => entries.get(file))
    try {
      expect(() => main([])).toThrow(`Unexpected native prebuilds: ${retired}`)
    } finally {
      existing.mockRestore()
      listing.mockRestore()
    }
  })

  test.each(['desktop-core'])('producer refuses retired Intel macOS target before compiling %s', backend => {
    const { main } = require('../scripts/native-prebuilds/build')
    const platform = Object.getOwnPropertyDescriptor(process, 'platform')
    const arch = Object.getOwnPropertyDescriptor(process, 'arch')
    Object.defineProperty(process, 'platform', { ...platform, value: 'darwin' })
    Object.defineProperty(process, 'arch', { ...arch, value: 'x64' })
    try {
      expect(() => main(['--backend', backend])).toThrow(
        `No maintained ${backend} prebuild target exists for darwin-x64`
      )
    } finally {
      Object.defineProperty(process, 'arch', arch)
      Object.defineProperty(process, 'platform', platform)
    }
  })

  test('production loader uses the shared exact native prebuild and explicit source override', () => {
    const loader = read('native/desktop-core/index.js')
    expect(loader).toContain("require('../load-node-api-addon')")
    expect(loader).toContain('loadExactPrebuild')
    expect(loader).toContain('loadExplicitAddon')
    expect(loader).not.toContain('native/electron')
  })

  test('maintained producer only executes the shared Rust build owner', () => {
    const build = read('scripts/native-prebuilds/build.js')
    expect(build).toContain('build-napi-addon.js')
    expect(build).not.toContain('node-gyp')
    expect(read('bindings/napi/Cargo.toml')).toContain('"napi4"')
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
    const jobs = YAML.parse(publish).jobs
    const assembler = jobs['canonical-package']
    expect(assembler.needs).toContain('native-prebuild')
    expect(
      assembler.steps.some(
        step => step.uses === 'actions/download-artifact@v8' && step.with.pattern === 'native-prebuild-*'
      )
    ).toBe(true)
    expect(publish).toContain('pnpm native-prebuild:verify --require-all --write-manifest')
    expect(publish.indexOf('pnpm native-prebuild:verify --require-all --write-manifest')).toBeLessThan(
      publish.indexOf('PACK_OUTPUT="$(npm pack --pack-destination .release-package)"')
    )
  })

  test('accepts pnpm extra -- before --backend', () => {
    const { parseBackend } = require('../scripts/native-prebuilds/build.js')

    expect(parseBackend(['--backend', 'desktop-core'])).toBe('desktop-core')
    expect(parseBackend(['--', '--backend', 'desktop-core'])).toBe('desktop-core')
    expect(() => parseBackend([])).toThrow(/Usage/)
    expect(() => parseBackend(['--backend'])).toThrow(/Usage/)
  })
})
