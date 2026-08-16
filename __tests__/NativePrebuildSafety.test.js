'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')

const {
  SOURCE_BUILD_ENV,
  nodeApiAddonCandidates
} = require('../native/load-node-api-addon')
const {
  LOCAL_BUILD_DIRECTORIES,
  listNativeBinaries
} = require('../scripts/native-prebuilds/verify')

describe('native prebuild source override and verification safety', () => {
  test('prefers bundled prebuilds by default and source builds only when explicitly requested', () => {
    const options = {
      moduleDirectory: '/tmp/unified-ble-manager/native/electron/corebluetooth',
      addonName: 'unified_ble_corebluetooth',
      platform: 'darwin',
      arch: 'arm64'
    }

    const packaged = nodeApiAddonCandidates(options)
    const source = nodeApiAddonCandidates({ ...options, preferSourceBuild: true })

    expect(SOURCE_BUILD_ENV).toBe('UNIFIED_BLE_MANAGER_NATIVE_SOURCE')
    expect(packaged[0]).toContain(
      path.join('prebuilds', 'darwin-arm64', 'unified_ble_corebluetooth.node')
    )
    expect(source[0]).toContain(
      path.join('build', 'Release', 'unified_ble_corebluetooth.node')
    )
    expect(source[1]).toContain(
      path.join('build', 'Debug', 'unified_ble_corebluetooth.node')
    )
    expect(source[2]).toBe(packaged[0])
  })

  test('prebuild verification ignores local node-gyp and Cargo output trees', () => {
    const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-native-prebuild-'))
    try {
      const packaged = path.join(
        fixture,
        'electron',
        'corebluetooth',
        'prebuilds',
        'darwin-arm64',
        'unified_ble_corebluetooth.node'
      )
      const nodeGyp = path.join(
        fixture,
        'electron',
        'corebluetooth',
        'build',
        'Release',
        'unified_ble_corebluetooth.node'
      )
      const cargo = path.join(
        fixture,
        'tauri',
        'target',
        'debug',
        'tauri_plugin_unified_ble_manager.node'
      )
      for (const filePath of [packaged, nodeGyp, cargo]) {
        fs.mkdirSync(path.dirname(filePath), { recursive: true })
        fs.writeFileSync(filePath, 'fixture')
      }

      const found = listNativeBinaries(fixture).map(filePath =>
        path.relative(fixture, filePath).split(path.sep).join('/')
      )

      expect([...LOCAL_BUILD_DIRECTORIES].sort()).toEqual([
        'build',
        'obj.target',
        'target'
      ])
      expect(found).toEqual([
        'electron/corebluetooth/prebuilds/darwin-arm64/unified_ble_corebluetooth.node'
      ])
    } finally {
      fs.rmSync(fixture, { recursive: true, force: true })
    }
  })
})
