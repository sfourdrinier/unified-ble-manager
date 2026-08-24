const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { runUnifiedBleCli } = require('../src/cli')
const { TAURI_PLUGIN_COMPATIBILITY } = require('../src/tauri')
const { createTestBleEnvironment } = require('../src/testing')
const { UNIFIED_BLE_IMPLEMENTATION_VERSION } = require('../src/implementation-version')

describe('PR11 distribution tooling and CLI taxonomy', () => {
  test('ubm doctor without a backend reports the consumer package and labels the proof boundary', async () => {
    const result = await runUnifiedBleCli(['doctor'])
    expect(result.ok).toBe(true)
    expect(result.command).toBe('doctor')
    expect(result.failures).toEqual([])
    expect(result.data).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        proofBoundary: 'compile-config-loadability',
        liveRadio: false,
        package: expect.objectContaining({
          name: 'unified-ble-manager',
          version: UNIFIED_BLE_IMPLEMENTATION_VERSION
        }),
        runtime: expect.objectContaining({
          node: process.version,
          platform: process.platform,
          architecture: process.arch
        })
      })
    )
    expect(result.data.package.path).toEqual(expect.stringContaining('package.json'))
  })

  test('ubm doctor --json is the same structured result as doctor', async () => {
    const plain = await runUnifiedBleCli(['doctor'])
    const json = await runUnifiedBleCli(['doctor', '--json'])
    expect(json).toEqual(plain)
  })

  test('ubm inspect package reports the installed package identity', async () => {
    const result = await runUnifiedBleCli(['inspect', 'package'])
    expect(result.ok).toBe(true)
    expect(result.command).toBe('inspect')
    expect(result.data).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        name: 'unified-ble-manager',
        version: UNIFIED_BLE_IMPLEMENTATION_VERSION,
        proofBoundary: 'compile-config-loadability'
      })
    )
  })

  test('ubm doctor fails closed when unified-ble-manager is not installed in cwd', async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-doctor-missing-'))
    fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify({ name: 'consumer', version: '1.0.0' }))
    const result = await runUnifiedBleCli(['doctor'], {
      readTextFile: async () => '',
      loadBackendModule: async () => {
        throw new Error('doctor must not load a backend')
      },
      cwd: () => directory
    })
    expect(result.ok).toBe(false)
    expect(result.command).toBe('doctor')
    expect(result.failures[0].code).toBe('cli.execution-failed')
  })

  test('ubm init --host tauri writes a crates.io fragment and does not overwrite without --force', async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-init-'))
    const target = path.join(directory, 'Cargo.toml.fragment')
    const first = await runUnifiedBleCli(['init', '--host', 'tauri', '--dir', directory])
    expect(first.ok).toBe(true)
    expect(first.command).toBe('init')
    expect(fs.readFileSync(target, 'utf8')).toContain('tauri-plugin-unified-ble-manager')
    expect(fs.readFileSync(target, 'utf8')).toMatch(/tauri-plugin-unified-ble-manager\s*=\s*"4/)
    expect(fs.readFileSync(target, 'utf8')).not.toContain('node_modules/unified-ble-manager/native/tauri')

    const blocked = await runUnifiedBleCli(['init', '--host', 'tauri', '--dir', directory])
    expect(blocked.ok).toBe(false)
    expect(blocked.failures[0].code).toBe('cli.argument-invalid')

    const forced = await runUnifiedBleCli(['init', '--host', 'tauri', '--dir', directory, '--force'])
    expect(forced.ok).toBe(true)
  })

  test('ubm init --host tauri uses runtime cwd when --dir is omitted', async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-init-cwd-'))
    const result = await runUnifiedBleCli(['init', '--host', 'tauri'], {
      readTextFile: async () => '',
      loadBackendModule: async () => {
        throw new Error('init must not load a backend')
      },
      cwd: () => directory
    })
    expect(result.ok).toBe(true)
    expect(fs.existsSync(path.join(directory, 'Cargo.toml.fragment'))).toBe(true)
  })

  test('ubm backend tck routes the existing TCK command', async () => {
    const result = await runUnifiedBleCli(['backend', 'tck'], {
      readTextFile: async () => '',
      loadBackendModule: async () => {
        throw new Error('not loaded in this assertion')
      }
    })
    expect(result.ok).toBe(false)
    expect(result.command).toBe('tck')
    expect(result.failures[0].message).toMatch(/--backend/)
  })

  test('ubm inspect config --host tauri reports crates.io install and does not load a backend', async () => {
    const result = await runUnifiedBleCli(['inspect', 'config', '--host', 'tauri'], {
      readTextFile: async () => '',
      loadBackendModule: async () => {
        throw new Error('inspect config must not load a backend')
      }
    })
    expect(result.ok).toBe(true)
    expect(result.command).toBe('inspect')
    expect(result.data).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        host: 'tauri',
        documentedCrate: 'tauri-plugin-unified-ble-manager',
        documentedInstall: 'crates.io',
        pathDependency: 'checkout-fallback',
        liveRadio: false,
        proofBoundary: 'compile-config-loadability',
        cratePublished: false,
        compatibility: expect.objectContaining({
          npmRange: '^4.0.0',
          crateRange: '^4.0.0',
          ipcProtocol: 2
        })
      })
    )
  })

  test('ubm inspect capabilities --host tauri reports protocol compatibility without a backend', async () => {
    const result = await runUnifiedBleCli(['inspect', 'capabilities', '--host', 'tauri'], {
      readTextFile: async () => '',
      loadBackendModule: async () => {
        throw new Error('inspect capabilities must not load a backend')
      }
    })
    expect(result.ok).toBe(true)
    expect(result.command).toBe('inspect')
    expect(result.data).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        host: 'tauri',
        liveRadio: false,
        proofBoundary: 'compile-config-loadability',
        ipcProtocol: 2
      })
    )
  })

  test('ubm init writes public-API fragments for expo, electron, node, and web', async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-init-hosts-'))
    const expo = await runUnifiedBleCli(['init', '--host', 'expo', '--dir', directory])
    const electron = await runUnifiedBleCli(['init', '--host', 'electron', '--dir', directory])
    const node = await runUnifiedBleCli(['init', '--host', 'node', '--dir', directory])
    const web = await runUnifiedBleCli(['init', '--host', 'web', '--dir', directory])
    expect(expo.ok && electron.ok && node.ok && web.ok).toBe(true)

    const expoPlugin = JSON.parse(fs.readFileSync(path.join(directory, 'app.json.fragment'), 'utf8'))
    expect(expoPlugin).toEqual({
      expo: {
        plugins: [['unified-ble-manager', { requiredHardware: true }]]
      }
    })
    const expoFactory = fs.readFileSync(path.join(directory, 'expo-factory.fragment.ts'), 'utf8')
    expect(expoFactory).toContain('createExpoBleManager')
    expect(expoFactory).toContain("unified-ble-manager/expo")
    expect(expoFactory).not.toMatch(/Expo Go is supported/i)

    const electronText = fs.readFileSync(path.join(directory, 'electron-renderer.fragment.ts'), 'utf8')
    expect(electronText).toContain('createElectronRendererBleManager')
    expect(electronText).toContain("unified-ble-manager/electron/renderer")
    expect(electronText).toContain("unified-ble-manager/electron/main")
    expect(electronText).not.toContain('createElectronMainBleManager')
    expect(fs.existsSync(path.join(directory, 'electron-main.fragment.ts'))).toBe(false)

    const nodeText = fs.readFileSync(path.join(directory, 'node-factory.fragment.ts'), 'utf8')
    expect(nodeText).toContain('createCoreBluetoothBleManager')
    expect(nodeText).toContain('createWinRtBleManager')
    expect(nodeText).toContain('createBluezBleManager')
    expect(nodeText).toContain("unified-ble-manager/node/corebluetooth")
    expect(nodeText).toContain("unified-ble-manager/node/winrt")
    expect(nodeText).toContain("unified-ble-manager/node/bluez")

    const webText = fs.readFileSync(path.join(directory, 'web-chooser.fragment.ts'), 'utf8')
    expect(webText).toContain('createWebBleManager')
    expect(webText).toContain("unified-ble-manager/web")
    expect(webText).not.toContain('createNavigatorWebBleManager')
  })

  test('ubm support-bundle create writes a local redacted bundle and does not include home paths', async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-support-'))
    const output = path.join(directory, 'support-bundle.json')
    const result = await runUnifiedBleCli(['support-bundle', 'create', '--output', output])
    expect(result.ok).toBe(true)
    expect(result.command).toBe('support-bundle')
    expect(result.data).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        liveRadio: false,
        proofBoundary: 'compile-config-loadability'
      })
    )
    const document = JSON.parse(fs.readFileSync(output, 'utf8'))
    expect(document).toEqual(
      expect.objectContaining({
        schemaVersion: 1,
        liveRadio: false,
        proofBoundary: 'compile-config-loadability',
        package: expect.objectContaining({
          name: 'unified-ble-manager',
          version: UNIFIED_BLE_IMPLEMENTATION_VERSION
        })
      })
    )
    expect(JSON.stringify(document)).not.toContain(os.homedir())
    expect(document.package.path).toBeUndefined()
  })
})

describe('PR11 Tauri crate and testkit contracts', () => {
  test('exports machine-readable npm/crate/protocol compatibility', () => {
    expect(TAURI_PLUGIN_COMPATIBILITY).toEqual(
      expect.objectContaining({
        npmRange: expect.stringMatching(/\^4\.0\.0/),
        crateRange: expect.stringMatching(/\^4\.0\.0/),
        ipcProtocol: 2
      })
    )
  })

  test('createTestBleEnvironment returns a public manager without importing a native radio', async () => {
    const environment = await createTestBleEnvironment()
    expect(typeof environment.manager.scan).toBe('function')
    expect(typeof environment.manager.connect).toBe('function')
    expect(environment.manager).not.toHaveProperty('createNativeCoreBluetoothBoundary')
    expect(environment).not.toHaveProperty('fixture')
    expect(typeof environment.destroy).toBe('function')
    await environment.destroy()
  })

  test('documented Tauri install is crates.io and does not claim the crate is already published', () => {
    const docs = fs.readFileSync(path.join(__dirname, '../docs/TAURI.md'), 'utf8')
    const crateReadme = fs.readFileSync(path.join(__dirname, '../native/tauri/README.md'), 'utf8')
    const exampleReadme = fs.readFileSync(path.join(__dirname, '../example-tauri/README.md'), 'utf8')
    for (const text of [docs, crateReadme, exampleReadme]) {
      expect(text).toContain('tauri-plugin-unified-ble-manager@4.0.0')
      expect(text).toMatch(/not (yet )?published|until the crate is (published|on crates\.io)|once the crate exists/i)
    }
  })
})
