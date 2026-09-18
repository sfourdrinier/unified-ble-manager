'use strict'

// PR210-03 (distribution) + PR210-18 (runtime identity): the desktop core is
// loaded only from this package's own native/desktop-core/prebuilds/<p>-<a>
// (or an explicit absolute UBM_NAPI_ADDON source build), hash-checked
// against its sidecar, and identity-checked against the sealed expectation
// before any radio call. No cwd lookup, no other-platform fallback.

const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const { EXPECTED_NATIVE_BUILD_IDENTITY } = require('../../../src/generated/native-build-identity')
const {
  bindDesktopCore,
  loadDesktopCoreBinding,
  verifyDesktopCoreIdentity
} = require('../../../src/desktop-core-addon')
const {
  nativeBuildIdentityMismatches,
  parseNativeBuildIdentityText
} = require('../../../src/native-build-identity-check')
const { addonPath, loadAddon } = require('../../helpers/desktop-rust-core-harness')

const ROOT = path.join(__dirname, '..', '..', '..')
const HOST = Object.freeze({ platform: 'corebluetooth', operationPrefix: 'direct-gatt' })

function sealedIdentity(overrides = {}) {
  return {
    schema: 'ubm-native-build-identity/1',
    binding: 'napi',
    contractRevision: EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision,
    sourceDigest: EXPECTED_NATIVE_BUILD_IDENTITY.bindings.napi.sourceDigest,
    bindingSchema: EXPECTED_NATIVE_BUILD_IDENTITY.bindings.napi.bindingSchema,
    target: 'aarch64-apple-darwin',
    profile: 'release',
    features: [],
    rustc: 'rustc 1.98.1',
    ...overrides
  }
}

function fakeModule(identity) {
  const text = typeof identity === 'string' ? identity : JSON.stringify(identity)
  return {
    nativeBuildIdentity: () => text,
    UbmCentral: { open: async () => null, openSynthetic: async () => null, listAdapters: async () => [] }
  }
}

/** A throwaway copy of native/desktop-core + its loader helper, anchored at a temp dir. */
function stagePackageCopy() {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-core-loader-')))
  fs.mkdirSync(path.join(root, 'native', 'desktop-core'), { recursive: true })
  fs.copyFileSync(
    path.join(ROOT, 'native', 'load-node-api-addon.js'),
    path.join(root, 'native', 'load-node-api-addon.js')
  )
  fs.copyFileSync(
    path.join(ROOT, 'native', 'desktop-core', 'index.js'),
    path.join(root, 'native', 'desktop-core', 'index.js')
  )
  const prebuildDir = path.join(root, 'native', 'desktop-core', 'prebuilds', `${process.platform}-${process.arch}`)
  return { root, prebuildDir, loader: path.join(root, 'native', 'desktop-core', 'index.js') }
}

function writeSidecar(prebuildDir, file, identity) {
  const sha256 = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
  fs.writeFileSync(
    path.join(prebuildDir, 'ubm_desktop_core.identity.json'),
    JSON.stringify({ schema: 'ubm-desktop-core-prebuild/1', sha256, identity })
  )
}

function captureCode(run) {
  try {
    run()
  } catch (error) {
    return error.code
  }
  return 'no-error'
}

describe('native/desktop-core loader (PR210-03)', () => {
  test('exact prebuild only: a missing target is no-prebuilt-for-target, never another binary', () => {
    const copy = stagePackageCopy()
    const { loadDesktopCore } = require(copy.loader)
    expect(captureCode(() => loadDesktopCore({}))).toBe('no-prebuilt-for-target')
    // A build/ artifact or another arch's prebuild is never consulted.
    fs.mkdirSync(path.join(copy.root, 'native', 'desktop-core', 'build', 'Release'), { recursive: true })
    fs.copyFileSync(
      addonPath,
      path.join(copy.root, 'native', 'desktop-core', 'build', 'Release', 'ubm_desktop_core.node')
    )
    const otherArch = path.join(copy.root, 'native', 'desktop-core', 'prebuilds', 'linux-x64')
    fs.mkdirSync(otherArch, { recursive: true })
    fs.copyFileSync(addonPath, path.join(otherArch, 'ubm_desktop_core.node'))
    expect(captureCode(() => loadDesktopCore({}))).toBe('no-prebuilt-for-target')
  })

  test('a prebuild is hash-checked against its sidecar before it is loaded', () => {
    const copy = stagePackageCopy()
    fs.mkdirSync(copy.prebuildDir, { recursive: true })
    const staged = path.join(copy.prebuildDir, 'ubm_desktop_core.node')
    fs.copyFileSync(addonPath, staged)
    const { loadDesktopCore } = require(copy.loader)
    expect(captureCode(() => loadDesktopCore({}))).toBe('prebuild-sidecar-missing')
    writeSidecar(copy.prebuildDir, staged, 'identity')
    fs.appendFileSync(staged, Buffer.from([0]))
    expect(captureCode(() => loadDesktopCore({}))).toBe('prebuild-digest-mismatch')
    // A file whose sidecar matches but that the runtime refuses is load-failed.
    fs.writeFileSync(staged, Buffer.from('not a mach-o / elf / pe image'))
    writeSidecar(copy.prebuildDir, staged, 'identity')
    expect(captureCode(() => loadDesktopCore({}))).toBe('load-failed')
  })

  test('a staged, hash-matching prebuild loads from the module location regardless of cwd', () => {
    const copy = stagePackageCopy()
    fs.mkdirSync(copy.prebuildDir, { recursive: true })
    const staged = path.join(copy.prebuildDir, 'ubm_desktop_core.node')
    fs.copyFileSync(addonPath, staged)
    const identity = loadAddon().nativeBuildIdentity()
    writeSidecar(copy.prebuildDir, staged, identity)
    const cwd = process.cwd()
    process.chdir(os.tmpdir())
    try {
      const { loadDesktopCore } = require(copy.loader)
      const loaded = loadDesktopCore({})
      expect(loaded).toMatchObject({ mode: 'prebuilt', path: staged, sidecar: { identity } })
      expect(typeof loaded.module.UbmCentral.open).toBe('function')
    } finally {
      process.chdir(cwd)
    }
  })

  test('UBM_NAPI_ADDON must be absolute and is used exclusively (source mode)', () => {
    const { loadDesktopCore } = require('../../../native/desktop-core')
    expect(captureCode(() => loadDesktopCore({ UBM_NAPI_ADDON: 'bindings/napi/addon.node' }))).toBe('argument-invalid')
    expect(captureCode(() => loadDesktopCore({ UBM_NAPI_ADDON: path.join(os.tmpdir(), 'absent.node') }))).toBe(
      'no-prebuilt-for-target'
    )
    expect(loadDesktopCore({ UBM_NAPI_ADDON: addonPath })).toMatchObject({
      mode: 'source',
      path: addonPath,
      sidecar: null
    })
  })

  test('exactPrebuildPath refuses a relative module directory (no cwd anchoring)', () => {
    const { exactPrebuildPath, detectLibc } = require('../../../native/load-node-api-addon')
    expect(() => exactPrebuildPath({ moduleDirectory: 'native/desktop-core', addonName: 'ubm_desktop_core' })).toThrow(
      /absolute/
    )
    expect(detectLibc('darwin')).toBeNull()
  })
})

describe('runtime identity before any radio call (PR210-18)', () => {
  test('the sealed identity passes; each differing field is named', () => {
    const sealed = parseNativeBuildIdentityText(JSON.stringify(sealedIdentity()))
    expect(nativeBuildIdentityMismatches(sealed, 'napi', EXPECTED_NATIVE_BUILD_IDENTITY, true)).toEqual([])
    const skewed = parseNativeBuildIdentityText(
      JSON.stringify(
        sealedIdentity({ sourceDigest: 'f'.repeat(64), profile: 'debug', target: 'riscv64gc-unknown-linux-gnu' })
      )
    )
    expect(nativeBuildIdentityMismatches(skewed, 'napi', EXPECTED_NATIVE_BUILD_IDENTITY, true)).toEqual([
      'sourceDigest',
      'target',
      'profile'
    ])
    expect(nativeBuildIdentityMismatches(skewed, 'napi', EXPECTED_NATIVE_BUILD_IDENTITY, false)).toEqual([
      'sourceDigest',
      'target'
    ])
    expect(parseNativeBuildIdentityText('{"schema":1}')).toBeNull()
    expect(parseNativeBuildIdentityText('not json')).toBeNull()
  })

  test('a binary identity with an unexpected extra key is refused before any radio call', () => {
    const loaded = {
      module: fakeModule({ ...sealedIdentity(), injected: 'x' }),
      path: '/abs/addon.node',
      mode: 'source',
      sidecar: null
    }
    expect(() => verifyDesktopCoreIdentity(HOST, loaded)).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'protocol.incompatible' }) })
    )
  })

  test('an unsealed build is refused', () => {
    const loaded = {
      module: fakeModule(sealedIdentity({ sourceDigest: 'unsealed', bindingSchema: 'unsealed' })),
      path: '/abs/addon.node',
      mode: 'source',
      sidecar: null
    }
    expect(() => verifyDesktopCoreIdentity(HOST, loaded)).toThrow(
      expect.objectContaining({
        normalized: expect.objectContaining({
          code: 'protocol.incompatible',
          domain: 'core',
          operation: 'direct-gatt.native-boundary.version',
          platform: expect.objectContaining({ metadata: { fields: ['sourceDigest', 'bindingSchema'] } })
        })
      })
    )
  })

  test('a prebuilt binary must match the identity its sidecar recorded', () => {
    const identity = JSON.stringify(sealedIdentity())
    const good = { module: fakeModule(identity), path: '/abs/addon.node', mode: 'prebuilt', sidecar: { identity } }
    expect(verifyDesktopCoreIdentity(HOST, good).profile).toBe('release')
    const swapped = { ...good, sidecar: { identity: JSON.stringify(sealedIdentity({ rustc: 'other' })) } }
    expect(() => verifyDesktopCoreIdentity(HOST, swapped)).toThrow(
      expect.objectContaining({
        normalized: expect.objectContaining({
          platform: expect.objectContaining({ metadata: { fields: ['sidecar'] } })
        })
      })
    )
  })

  test('a binding never opens a central when the identity check fails', () => {
    const open = jest.fn()
    const module = {
      ...fakeModule(sealedIdentity({ contractRevision: 'C-UBM.9' })),
      UbmCentral: { open, openSynthetic: open, listAdapters: open }
    }
    expect(() => bindDesktopCore(HOST, { module, path: '/abs/a.node', mode: 'source', sidecar: null })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'protocol.incompatible' }) })
    )
    expect(open).not.toHaveBeenCalled()
  })

  test('the checkout-built addon carries the sealed identity (source mode)', () => {
    const identity = verifyDesktopCoreIdentity(HOST, {
      module: loadAddon(),
      path: addonPath,
      mode: 'source',
      sidecar: null
    })
    expect(identity.sourceDigest).toBe(EXPECTED_NATIVE_BUILD_IDENTITY.bindings.napi.sourceDigest)
  })

  test.each([
    ['no-prebuilt-for-target', 'capability.unavailable', 'direct-gatt.native-boundary.load'],
    ['load-failed', 'capability.unavailable', 'direct-gatt.native-boundary.load'],
    ['argument-invalid', 'argument.invalid', 'direct-gatt.native-boundary.load'],
    ['prebuild-digest-mismatch', 'protocol.incompatible', 'direct-gatt.native-boundary.version'],
    ['prebuild-sidecar-missing', 'protocol.incompatible', 'direct-gatt.native-boundary.version']
  ])('loader cause %s maps to %s at %s, keeping the cause text', async (code, contractCode, operation) => {
    const cause = Object.assign(new Error(`dlopen text for ${code}`), { code })
    await expect(
      loadDesktopCoreBinding(HOST, async () => () => {
        throw cause
      })
    ).rejects.toMatchObject({
      normalized: { code: contractCode, operation, platform: { code, safeMessage: `dlopen text for ${code}` } }
    })
  })
})
