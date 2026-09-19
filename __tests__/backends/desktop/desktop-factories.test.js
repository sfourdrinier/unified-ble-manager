'use strict'

// PR210-02 / 20 / 28 / 29: every no-options desktop factory (Node BlueZ,
// CoreBluetooth, WinRT and Electron main) executes the shared Rust core; no
// public entrypoint reaches a legacy backend; rejected options fail before
// any core call (the option audit, docs/NODE.md).

const path = require('node:path')

const { realBinding, dispatchCalls } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

function withPlatform(platform, run) {
  const original = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: platform })
  const restore = () => Object.defineProperty(process, 'platform', original)
  let result
  try {
    result = run()
  } catch (error) {
    restore()
    throw error
  }
  if (result && typeof result.then === 'function') return result.finally(restore)
  restore()
  return result
}

const FACTORIES = [
  { platform: 'bluez', os: 'linux', module: 'node-bluez', factory: 'createBluezBleManager' },
  { platform: 'corebluetooth', os: 'darwin', module: 'node-corebluetooth', factory: 'createCoreBluetoothBleManager' },
  { platform: 'winrt', os: 'win32', module: 'node-winrt', factory: 'createWinRtBleManager' }
]

function load(module) {
  return require(path.join('..', '..', '..', 'src', module))
}

describe('no-options factories execute the shared Rust core', () => {
  test.each(FACTORIES)('$factory opens a $platform Rust central on $os', async ({ platform, os, module, factory }) => {
    const harness = realBinding(platform)
    const manager = await withPlatform(os, () => load(module)[factory]({ binding: harness.binding }))
    try {
      expect(harness.productionRequests.length).toBeGreaterThan(0)
      expect(harness.productionRequests.every(request => request.platform === platform)).toBe(true)
      expect((await manager.adapter.state()).availability).toBe('available')
    } finally {
      await manager.destroy()
    }
  })

  test.each([
    ['createElectronMainCoreBluetoothBackendProvider', 'corebluetooth', 'darwin'],
    ['createElectronMainWinRtBackendProvider', 'winrt', 'win32'],
    ['createElectronMainBluezBackendProvider', 'bluez', 'linux']
  ])('%s is the Rust provider with hostKind desktop-native', (factory, platform, os) => {
    const electronMain = load('electron-main')
    const provider = withPlatform(os, () => electronMain[factory]({ now: () => 1 }))
    expect(provider.descriptor).toMatchObject({
      providerId: {
        corebluetooth: 'unified-ble:corebluetooth-provider',
        winrt: 'unified-ble:winrt-provider',
        bluez: 'unified-ble:bluez-dbus-provider'
      }[platform],
      hostKind: 'desktop-native'
    })
  })

  test.each(FACTORIES)(
    '$factory on a foreign OS fails before anything loads',
    async ({ module, factory, platform }) => {
      const harness = realBinding(platform)
      const foreign = platform === 'bluez' ? 'darwin' : 'linux'
      await expect(
        withPlatform(foreign, () => load(module)[factory]({ binding: harness.binding }))
      ).rejects.toMatchObject({
        normalized: { code: 'capability.unavailable', domain: 'platform' }
      })
      expect(harness.calls).toHaveLength(0)
      expect(harness.productionRequests).toHaveLength(0)
    }
  )
})

describe('no public entrypoint reaches a legacy desktop backend', () => {
  const LEGACY_EXPORTS = [
    'createCoreBluetoothBackendProvider',
    'createNativeCoreBluetoothBoundary',
    'prepareNativeCoreBluetoothBoundary',
    'createWinRtBackendProvider',
    'createNativeWinRtBoundary',
    'createBluezBackendProvider',
    'DbusNextBluezBoundaryFactory',
    'createBluezRustCoreBackendProvider',
    'BLUEZ_RUST_CORE_BACKEND_ID'
  ]
  test.each(['node-bluez', 'node-corebluetooth', 'node-winrt', 'electron-main'])('%s exports none of them', module => {
    const exported = Object.keys(load(module))
    expect(exported.filter(name => LEGACY_EXPORTS.includes(name))).toEqual([])
    // D5/decision L: the production surface keeps the production factory;
    // the parity table and the seam-accepting factory live in /testing only.
    expect(exported).toEqual(expect.arrayContaining(['createDesktopRustCoreBackendProvider']))
    expect(exported).not.toContain('DESKTOP_RUST_CORE_PARITY')
    expect(exported).not.toContain('createTestDesktopRustCoreBackendProvider')
  })

  test.each(['node-bluez', 'node-corebluetooth', 'node-winrt', 'electron-main'])(
    '%s keeps test seams in /testing only',
    module => {
      expect(Object.keys(load(module))).not.toEqual(
        expect.arrayContaining(['radio', 'loadBinding', 'hostPlatform', 'firstStateTimeoutMs', 'openSynthetic'])
      )
      const testing = require('../../../src/testing')
      expect(typeof testing.createTestDesktopRustCoreBackendProvider).toBe('function')
      expect(Array.isArray(testing.DESKTOP_RUST_CORE_PARITY)).toBe(true)
    }
  )

  test.each([
    ['node-corebluetooth', 'COREBLUETOOTH_BACKEND_ID', 'corebluetooth'],
    ['node-winrt', 'WINRT_BACKEND_ID', 'winrt'],
    ['node-bluez', 'BLUEZ_BACKEND_ID', 'bluez'],
    ['electron-main', 'COREBLUETOOTH_BACKEND_ID', 'corebluetooth'],
    ['electron-main', 'WINRT_BACKEND_ID', 'winrt'],
    ['electron-main', 'BLUEZ_BACKEND_ID', 'bluez']
  ])('%s %s keeps its legacy value and names the Rust backend (LEGACY-AUDIT-1 #67)', (module, name, platform) => {
    const exported = load(module)
    expect(exported[name]).toBe(exported.DESKTOP_RUST_CORE_PROFILES[platform].backendId)
    expect(exported[name]).toBe(
      { corebluetooth: 'unified-ble:corebluetooth', winrt: 'unified-ble:winrt', bluez: 'unified-ble:bluez-dbus' }[
        platform
      ]
    )
  })
})

// A physical Linux run found the Electron example taking the BlueZ
// compatibility offer from `node/bluez`: Electron main owns the radio on every
// desktop OS, so it offers each backend's identity exactly as the Node
// entrypoint does, BlueZ included.
describe('Electron main offers every desktop backend identity', () => {
  test.each([
    [
      'node-corebluetooth',
      [
        'coreBluetoothCompatibility',
        'COREBLUETOOTH_BACKEND_ID',
        'COREBLUETOOTH_PLATFORM_ID',
        'COREBLUETOOTH_IMPLEMENTATION_VERSION'
      ]
    ],
    ['node-winrt', ['winRtCompatibility', 'WINRT_BACKEND_ID', 'WINRT_PLATFORM_ID', 'WINRT_IMPLEMENTATION_VERSION']],
    ['node-bluez', ['bluezCompatibility', 'BLUEZ_BACKEND_ID', 'BLUEZ_PLATFORM_ID', 'BLUEZ_IMPLEMENTATION_VERSION']]
  ])('the same %s identity exports', (module, names) => {
    const node = load(module)
    const electronMain = load('electron-main')
    for (const name of names) {
      expect(node[name]).toBeDefined()
      expect(electronMain[name]).toBe(node[name])
    }
  })
})

describe('BlueZ bus choice (PR210-20)', () => {
  test('busKind reaches the core open (system default, session carried, never ignored)', async () => {
    const { createBluezBleManager } = load('node-bluez')
    for (const [busKind, expected] of [
      [undefined, 'system'],
      ['system', 'system'],
      ['session', 'session']
    ]) {
      const harness = realBinding('bluez')
      const manager = await withPlatform('linux', () =>
        createBluezBleManager({ ...(busKind === undefined ? {} : { busKind }), binding: harness.binding })
      )
      await manager.destroy()
      expect(harness.productionRequests.map(request => request.bluezBus)).toEqual(
        harness.productionRequests.map(() => expected)
      )
      expect(harness.productionRequests.length).toBeGreaterThan(0)
    }
  })

  test('Electron main BlueZ carries busKind and pairingGeneration to the core like the Node factory (N10)', async () => {
    const electronMain = load('electron-main')
    const controller = { read: async () => 'enabled', set: async () => undefined }
    for (const [options, expected] of [
      [{}, { bluezBus: 'system', pairingGeneration: undefined }],
      [{ busKind: 'session' }, { bluezBus: 'session', pairingGeneration: undefined }],
      [
        { busKind: 'system', pairingGeneration: controller },
        { bluezBus: 'system', pairingGeneration: true }
      ]
    ]) {
      const harness = realBinding('bluez')
      const provider = withPlatform('linux', () =>
        electronMain.createElectronMainBluezBackendProvider({ now: () => 1, binding: harness.binding, ...options })
      )
      expect(provider.descriptor.hostKind).toBe('desktop-native')
      const [adapter] = await provider.listAdapters()
      const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
      await backend.destroy()
      expect(harness.productionRequests.length).toBeGreaterThan(0)
      for (const request of harness.productionRequests) {
        expect({ bluezBus: request.bluezBus, pairingGeneration: request.pairingGeneration }).toEqual(expected)
      }
    }
    expect(() =>
      withPlatform('linux', () =>
        electronMain.createElectronMainBluezBackendProvider({ now: () => 1, busKind: 'user' })
      )
    ).toThrow(expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) }))
  })

  test('an unknown busKind is invalid before anything loads', async () => {
    const { createBluezBleManager, createDbusNextBluezBackendProvider } = load('node-bluez')
    const harness = realBinding('bluez')
    await expect(
      withPlatform('linux', () => createBluezBleManager({ busKind: 'user', binding: harness.binding }))
    ).rejects.toMatchObject({ normalized: { code: 'argument.invalid', operation: 'bluez.provider.bus-kind' } })
    expect(() =>
      withPlatform('linux', () => createDbusNextBluezBackendProvider({ busKind: 'user', now: () => 1 }))
    ).toThrow(expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) }))
    expect(harness.productionRequests).toHaveLength(0)
  })

  test("a session-bus listing is the core's own answer (unsupported off Linux), never a system-bus stand-in", async () => {
    const { bindDesktopCore } = require('../../../src/desktop-core-addon')
    const { addonPath, loadAddon } = require('../../helpers/desktop-rust-core-harness')
    const binding = bindDesktopCore(
      { platform: 'bluez', operationPrefix: 'bluez' },
      { module: loadAddon(), path: addonPath, mode: 'source', sidecar: null }
    )
    if (process.platform === 'linux') {
      // The vendored btleplug reaches BlueZ on the session bus (patch
      // `bluez-session-bus`), so the listing is that bus's answer: its
      // adapters, or — where no session bus or no BlueZ on it is reachable,
      // as on a headless runner — the adapter's own failure with the BlueZ
      // D-Bus answer in `platform` (one vocabulary: the listing keeps its
      // own name), never the system bus's adapters in its place.
      const outcome = await binding.listAdapters('session').then(
        adapters => ({ adapters }),
        error => ({ error: error.normalized })
      )
      if (outcome.adapters !== undefined) {
        expect(outcome.adapters).toEqual(expect.any(Array))
      } else {
        expect(outcome.error).toMatchObject({ code: 'adapter.unavailable', platform: { domain: 'bluez-dbus' } })
      }
    } else {
      await expect(binding.listAdapters('session')).rejects.toMatchObject({
        normalized: { code: 'capability.unsupported', platform: { safeMessage: expect.stringMatching(/Linux only/) } }
      })
    }
  })
})

describe('option audit: every rejected option fails with zero core dispatch', () => {
  const {
    openBackend,
    connectAndDiscover,
    observePeer,
    scanOptions,
    subscribeOptions
  } = require('../../helpers/desktop-rust-core-harness')

  async function audited(platform, prepare, attempt, expected) {
    const opened = await openBackend(platform)
    try {
      const context = await prepare(opened)
      const before = dispatchCalls(opened.harness.calls).length
      await expect(Promise.resolve().then(() => attempt(opened, context))).rejects.toMatchObject({
        normalized: expected
      })
      expect(
        dispatchCalls(opened.harness.calls)
          .slice(before)
          .map(([name]) => name)
      ).toEqual([])
    } finally {
      await opened.backend.destroy()
    }
  }

  test('BlueZ-only options on another platform are invalid before anything loads', () => {
    const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const loadBinding = jest.fn()
    expect(() =>
      createTestDesktopRustCoreBackendProvider({
        platform: 'winrt',
        owner: 'audit',
        now: () => 1,
        loadBinding,
        hostPlatform: 'win32',
        bluezBus: 'session'
      })
    ).toThrow(expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) }))
    expect(loadBinding).not.toHaveBeenCalled()
  })

  test('scan platform options', () =>
    audited(
      'corebluetooth',
      async () => undefined,
      ({ backend }) =>
        backend.scanner.start(scanOptions({ platform: { kind: 'corebluetooth', mode: 'balanced' } }), 'c'),
      { code: 'capability.unsupported' }
    ))

  test('an empty local-name prefix is an invalid filter', () =>
    audited(
      'winrt',
      async () => undefined,
      ({ backend }) =>
        backend.scanner.start(
          scanOptions({ filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: '' } }),
          'c'
        ),
      { code: 'scan.filter-invalid' }
    ))

  test('connection intent when-available', () =>
    audited(
      'bluez',
      ({ backend, stage }) => observePeer(backend, stage),
      ({ backend }, peerId) =>
        backend.connections.connect(peerId, 'c', { signal: null, deadline: null, intent: 'when-available' }),
      { code: 'capability.unsupported' }
    ))

  test('connection preferred PHY', () =>
    audited(
      'winrt',
      ({ backend, stage }) => observePeer(backend, stage),
      ({ backend }, peerId) =>
        backend.connections.connect(peerId, 'c', { signal: null, deadline: null, preferredPhy: ['le-2m'] }),
      { code: 'capability.unsupported' }
    ))

  test('connection transport other than le/auto', () =>
    audited(
      'corebluetooth',
      ({ backend, stage }) => observePeer(backend, stage),
      ({ backend }, peerId) =>
        backend.connections.connect(peerId, 'c', { signal: null, deadline: null, transport: 'br-edr' }),
      { code: 'argument.invalid' }
    ))

  // W-R1: legacy WinRT forwarded the mode to the native boundary and wrapped
  // the native refusal as `gatt.write-failed` (never `capability.unsupported`);
  // the core takes no descriptor-write mode, so the new path fails closed
  // before dispatch with the same code and domain.
  test('descriptor write without response (open blocker gatt.descriptor-write-mode)', () =>
    audited(
      'winrt',
      ({ backend, stage }) => connectAndDiscover(backend, stage),
      (_opened, { database, snapshot }) =>
        database.writeDescriptor(snapshot.descriptors[0].path, new Uint8Array([1]), {
          signal: null,
          deadline: null,
          mode: 'without-response'
        }),
      { code: 'gatt.write-failed' }
    ))

  test('require-indication on a characteristic without indicate', () =>
    audited(
      'bluez',
      ({ backend, stage }) => connectAndDiscover(backend, stage),
      (_opened, { database, measurement }) =>
        database.subscribe(measurement.path, subscribeOptions({ deliveryMode: 'require-indication' })),
      { code: 'gatt.property-not-supported' }
    ))

  test('subscribe to a characteristic with neither notify nor indicate', () =>
    audited(
      'corebluetooth',
      ({ backend, stage }) => connectAndDiscover(backend, stage),
      (_opened, { database, control }) => database.subscribe(control.path, subscribeOptions()),
      { code: 'gatt.property-not-supported' }
    ))

  test('an unknown adapter id', async () => {
    const harness = realBinding('winrt')
    const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const provider = createTestDesktopRustCoreBackendProvider({
      platform: 'winrt',
      owner: 'audit',
      now: () => 1,
      binding: harness.binding,
      hostPlatform: 'win32'
    })
    await expect(provider.create({ selectedAdapterId: 'not-an-adapter' })).rejects.toMatchObject({
      normalized: { code: 'adapter.unavailable' }
    })
    // Only the listing's own power measurement opened a central; nothing
    // opened for the unknown id.
    expect(harness.productionRequests.map(request => request.adapterId)).toEqual(['synthetic-adapter'])
  })

  test('restoration options are refused by the Node host', async () => {
    const { createCoreBluetoothBleManager } = load('node-corebluetooth')
    const harness = realBinding('corebluetooth')
    await expect(
      withPlatform('darwin', () =>
        createCoreBluetoothBleManager({ binding: harness.binding, restoration: { key: 'restore', mode: 'apple' } })
      )
    ).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(harness.productionRequests).toHaveLength(0)
  })

  test('an empty owner', async () => {
    const { createWinRtBleManager } = load('node-winrt')
    const harness = realBinding('winrt')
    await expect(
      withPlatform('win32', () => createWinRtBleManager({ owner: '', binding: harness.binding }))
    ).rejects.toMatchObject({
      normalized: { code: 'argument.invalid' }
    })
    expect(harness.productionRequests).toHaveLength(0)
  })
})

describe('legacy native loaders under ESM (PR210-28)', () => {
  // The ESM build (lib/module, built by `pretest:package` -> prepack) has no
  // CommonJS `require`: the legacy loaders must name that cause, not mask it
  // as an unavailable artifact.
  test.each([
    ['backends/corebluetooth/corebluetooth-native-boundary.js', 'createNativeCoreBluetoothBoundary', 'darwin'],
    ['backends/winrt/winrt-native-boundary.js', 'createNativeWinRtBoundary', 'win32']
  ])('lib/module/%s throws capability.unsupported / esm-legacy-boundary', (file, loader, os) => {
    const { spawnSync } = require('node:child_process')
    const modulePath = path.join(__dirname, '..', '..', '..', 'lib', 'module', file)
    const script = [
      `Object.defineProperty(process, 'platform', { value: ${JSON.stringify(os)} })`,
      `const { ${loader} } = await import(${JSON.stringify(require('node:url').pathToFileURL(modulePath).href)})`,
      `try { ${loader}(); console.log(JSON.stringify({ outcome: 'loaded' })) }`,
      'catch (error) { console.log(JSON.stringify(error.normalized ?? { message: String(error) })) }'
    ].join('\n')
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], { encoding: 'utf8' })
    expect(result.stderr).toBe('')
    expect(JSON.parse(result.stdout.trim())).toMatchObject({
      code: 'capability.unsupported',
      platform: { code: 'esm-legacy-boundary' }
    })
  })
})
