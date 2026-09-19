#!/usr/bin/env node
// scripts/ci/electron-main-smoke.js
/**
 * Headless Electron main-process public-boundary smoke (L3 wiring, not radio L4).
 *
 * Run under the Electron binary (not plain Node) after a Node-API prebuild or
 * explicit local source build is present.
 *
 * Always: imports the public Electron-main boundary under the Electron runtime
 * and asserts its providers are the shared Rust core (no legacy export).
 *
 * Desktop core (PR210-03): when a desktop-core prebuild exists for this
 * platform (or UBM_NAPI_ADDON names a source build), loads it through the
 * package loader, verifies its build identity, and opens + closes one
 * central on the SYNTHETIC radio: the Rust core executes under the Electron
 * ABI with no Bluetooth permission and no radio claim.
 *
 * LEGACY (Phase 4 deletes this leg with native/electron/*): on darwin/win32,
 * unless UBM_SMOKE_BACKEND=desktop-core, also creates the legacy node-gyp
 * contract boundary (loaded through its internal module, unreachable from
 * any public entrypoint) so its prebuild keeps loading until deletion.
 *
 *   ./node_modules/.bin/electron scripts/ci/electron-main-smoke.js
 *
 * No branch starts a scan or claims live radio.
 */
'use strict'

const path = require('path')

const root = path.resolve(__dirname, '../..')

function loadElectronMain() {
  try {
    return require(path.join(root, 'lib/commonjs/electron-main'))
  } catch (e) {
    console.error('Could not load compiled Electron-main entrypoint. Run `pnpm prepack` first.\n', e && e.message)
    process.exit(1)
  }
}

/**
 * Load this platform's desktop core through the package loader, verify its
 * identity, and run one synthetic central under the Electron ABI.
 */
async function smokeDesktopCore() {
  const platform = { linux: 'bluez', darwin: 'corebluetooth', win32: 'winrt' }[process.platform]
  const prebuild = path.join(root, 'native', 'desktop-core', 'prebuilds', `${process.platform}-${process.arch}`, 'ubm_desktop_core.node')
  const sourceBuild = process.env.UBM_NAPI_ADDON
  if (platform === undefined || (!require('fs').existsSync(prebuild) && !sourceBuild)) {
    if (process.env.UBM_SMOKE_BACKEND === 'desktop-core') {
      throw new Error(`desktop-core smoke requested but no prebuild exists at ${prebuild}`)
    }
    console.log('Electron main-process desktop-core smoke skipped (no prebuild for this target)', { prebuild })
    return
  }
  const { loadDesktopCoreBinding } = require(path.join(root, 'lib/commonjs/desktop-core-addon'))
  const binding = await loadDesktopCoreBinding({ platform, operationPrefix: `${platform}-manager` })
  const central = await binding.openSynthetic('electron-main-smoke')
  const report = await central.close()
  if (report.state !== 'released') {
    throw new Error(`desktop-core synthetic central did not release: ${JSON.stringify(report)}`)
  }
  console.log('Electron main-process desktop-core smoke ok', {
    runtime: 'electron',
    electron: process.versions.electron,
    ...binding.diagnostics
  })
}

async function main() {
  // R3-F067: refuse plain Node — this script is the Electron-ABI gate.
  if (typeof process.versions.electron !== 'string') {
    throw new Error(
      'electron-main-smoke must run under the Electron binary (process.versions.electron missing). ' +
        'Use: ./node_modules/.bin/electron scripts/ci/electron-main-smoke.js'
    )
  }

  const electronMain = loadElectronMain()
  for (const factory of [
    'createElectronMainCoreBluetoothBackendProvider',
    'createElectronMainWinRtBackendProvider',
    'createElectronMainBluezBackendProvider'
  ]) {
    if (typeof electronMain[factory] !== 'function') {
      throw new Error(`Electron-main ${factory} is not a function under Electron`)
    }
  }
  for (const legacy of ['createNativeCoreBluetoothBoundary', 'createNativeWinRtBoundary', 'createCoreBluetoothBackendProvider']) {
    if (legacy in electronMain) {
      throw new Error(`Electron-main must not expose the legacy ${legacy}`)
    }
  }

  console.log('Electron main-process L3 public entrypoint smoke ok', {
    runtime: 'electron',
    electron: process.versions.electron
  })

  await smokeDesktopCore()

  // Electron's process.exit does not stop this function synchronously:
  // return instead, and exit once at the end.
  if (process.env.UBM_SMOKE_BACKEND !== 'desktop-core') {
    await smokeLegacyBoundary()
  }

  // Electron keeps the event loop alive until explicitly exited.
  process.exit(0)
}

/** LEGACY: the node-gyp boundary prebuilds, until Phase 4 deletes them. */
async function smokeLegacyBoundary() {
  const { createNativeCoreBluetoothBoundary } = require(path.join(
    root,
    'lib/commonjs/backends/corebluetooth/corebluetooth-native-boundary'
  ))
  const { createNativeWinRtBoundary } = require(path.join(root, 'lib/commonjs/backends/winrt/winrt-native-boundary'))

  // R3-F012: the direct Node-API boundary must load under Electron.
  if (process.platform === 'darwin') {
    if (typeof createNativeCoreBluetoothBoundary !== 'function') {
      throw new Error('createNativeCoreBluetoothBoundary missing from Electron-main entrypoint')
    }
    const boundary = createNativeCoreBluetoothBoundary()
    for (const method of [
      'adapterSnapshot',
      'startScan',
      'stopScan',
      'connect',
      'disconnect',
      'connectionState',
      'discover',
      'read',
      'write',
      'startNotify',
      'stopNotify',
      'onDisconnect',
      'onAdapterState',
      'destroy'
    ]) {
      if (typeof boundary[method] !== 'function') {
        throw new Error(`CoreBluetooth contract boundary is missing ${method}`)
      }
    }
    await boundary.destroy()
    console.log('Electron main-process L3 CoreBluetooth public boundary ok', {
      runtime: 'electron',
      electron: process.versions.electron
    })
  } else if (process.platform === 'win32') {
    if (typeof createNativeWinRtBoundary !== 'function') {
      throw new Error('createNativeWinRtBoundary missing from Electron-main entrypoint')
    }
    const boundary = createNativeWinRtBoundary()
    for (const method of [
      'listAdapters',
      'selectAdapter',
      'adapterSnapshot',
      'startScan',
      'stopScan',
      'connect',
      'disconnect',
      'discover',
      'read',
      'write',
      'readDescriptor',
      'writeDescriptor',
      'startNotify',
      'stopNotify',
      'onConnectionLost',
      'onDatabaseChanged',
      'onAdapterState',
      'onSecurityState',
      'onScanTerminal',
      'securityState',
      'pair',
      'cancelPairing',
      'unpair',
      'ingressTelemetry',
      'destroy'
    ]) {
      if (typeof boundary[method] !== 'function') {
        throw new Error(`WinRT contract boundary is missing ${method}`)
      }
    }
    const cleanup = boundary.destroy()
    await cleanup.completion
    console.log('Electron main-process L3 WinRT public boundary ok', {
      runtime: 'electron',
      electron: process.versions.electron
    })
  } else {
    console.log('Electron main-process L3 native boundary skipped (unsupported host; no radio claim)', {
      platform: process.platform
    })
  }
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
