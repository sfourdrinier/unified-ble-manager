#!/usr/bin/env node
// scripts/ci/check-host-exports.js
/**
 * Post-prepack export smoke shared by CI, publish, and verify-release.
 *
 * The package export map is the authority for the public surface. Every
 * target is checked for presence, but host-bound entrypoints are not loaded
 * from this generic Node process because their React, React Native, Expo,
 * Tauri, Electron, or native-radio dependencies belong to their host runtime.
 */
'use strict'

const assert = require('assert')
const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function collectExportTargets(value, targets = []) {
  if (value === undefined) return targets
  if (typeof value === 'string') {
    targets.push(value)
    return targets
  }
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('package export must be a string or conditional export object')
  }
  for (const [condition, target] of Object.entries(value)) {
    if (condition !== 'types') collectExportTargets(target, targets)
  }
  return targets
}

function resolveCommonJsTarget(value) {
  if (typeof value === 'string') return value
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null

  for (const condition of ['require', 'node', 'default']) {
    if (Object.prototype.hasOwnProperty.call(value, condition)) {
      const target = resolveCommonJsTarget(value[condition])
      if (target !== null) return target
    }
  }
  for (const [condition, targetValue] of Object.entries(value)) {
    if (condition === 'types') continue
    const target = resolveCommonJsTarget(targetValue)
    if (target !== null) return target
  }
  return null
}

function derivePublicPackageExports(exportsMap) {
  assert.ok(
    exportsMap !== null && typeof exportsMap === 'object' && !Array.isArray(exportsMap),
    'package.exports must be an object'
  )
  return Object.entries(exportsMap)
    .map(([exportPath, exportValue]) => ({
      exportPath,
      targets: collectExportTargets(exportValue),
      commonJsTarget: resolveCommonJsTarget(exportValue)
    }))
    .sort((left, right) => left.exportPath.localeCompare(right.exportPath))
}

function isHostBoundExport(exportPath) {
  return (
    exportPath === './app.plugin.js' ||
    /^(?:\.\/react(?:-native)?|\.\/expo|\.\/tauri|\.\/node(?:\/|$)|\.\/electron(?:\/|$))/u.test(exportPath)
  )
}

function targetPathWithinPackage(packageRoot, exportTarget) {
  assert.ok(exportTarget.startsWith('./'), `package export target must be relative: ${exportTarget}`)
  const resolved = path.resolve(packageRoot, exportTarget)
  const relative = path.relative(packageRoot, resolved)
  assert.ok(
    relative !== '..' && !relative.startsWith(`..${path.sep}`),
    `package export target escapes package: ${exportTarget}`
  )
  return resolved
}

function selectCommonJsRuntimeExports(exportsMap, packageRoot) {
  return derivePublicPackageExports(exportsMap)
    .filter(entry => !isHostBoundExport(entry.exportPath))
    .filter(entry => entry.commonJsTarget !== null && entry.commonJsTarget.endsWith('.js'))
    .map(entry => ({
      ...entry,
      targetPath: targetPathWithinPackage(packageRoot, entry.commonJsTarget)
    }))
}

function assertPublicPackageExportTargets(exportsMap, packageRoot) {
  const entries = derivePublicPackageExports(exportsMap)
  for (const entry of entries) {
    assert.ok(entry.targets.length > 0, `package export has no runtime target: ${entry.exportPath}`)
    for (const exportTarget of entry.targets) {
      const targetPath = targetPathWithinPackage(packageRoot, exportTarget)
      assert.ok(fs.existsSync(targetPath), `missing package export target ${entry.exportPath}: ${exportTarget}`)
    }
  }
  return entries
}

function loadCommonJsRuntimeExports(entries) {
  return new Map(entries.map(entry => [entry.exportPath, require(entry.targetPath)]))
}

function main() {
  const pkg = require(path.join(root, 'package.json'))
  assert.strictEqual(pkg.name, 'unified-ble-manager')
  const publicExports = assertPublicPackageExportTargets(pkg.exports, root)
  const runtimeEntries = selectCommonJsRuntimeExports(pkg.exports, root)
  const loaded = loadCommonJsRuntimeExports(runtimeEntries)

  for (const unavailableHostExport of ['./electron', './node']) {
    assert.ok(!pkg.exports[unavailableHostExport], `host export must remain unavailable: ${unavailableHostExport}`)
  }

  const publicRoot = loaded.get('.')
  const backendSdk = loaded.get('./backend-sdk')
  const testing = loaded.get('./testing')
  const codecs = loaded.get('./codecs')
  const profileCommands = loaded.get('./profiles/commands')
  const standardProfileCommands = loaded.get('./profiles/standard-commands')
  const heartRate = loaded.get('./profiles/heart-rate')
  const battery = loaded.get('./profiles/battery-service')
  const deviceInformation = loaded.get('./profiles/device-information')
  const healthThermometer = loaded.get('./profiles/health-thermometer')
  const bloodPressure = loaded.get('./profiles/blood-pressure')
  const ieee11073 = loaded.get('./profiles/ieee-11073')
  const web = loaded.get('./web')
  const advanced = loaded.get('./advanced')
  const cli = loaded.get('./cli')

  assert.strictEqual(publicRoot.ApplicationBleManager, undefined, 'root ApplicationBleManager must remain internal')
  assert.strictEqual(publicRoot.createPublicBleManager, undefined, 'root createPublicBleManager must remain internal')
  assert.strictEqual(typeof backendSdk.runBackendTck, 'function', 'backend-sdk.runBackendTck must be a function')
  assert.strictEqual(
    typeof testing.createDeterministicTestBackend,
    'function',
    'testing.createDeterministicTestBackend must be a function'
  )
  assert.strictEqual(
    typeof testing.createDeterministicBackendTckFactory,
    'function',
    'testing.createDeterministicBackendTckFactory must be a function'
  )
  assert.strictEqual(typeof codecs.dataView, 'function', 'codecs.dataView must be a function')
  assert.strictEqual(
    typeof profileCommands.resolveCharacteristicPath,
    'function',
    'profiles/commands.resolveCharacteristicPath must be a function'
  )
  assert.strictEqual(
    typeof standardProfileCommands.readBatteryLevel,
    'function',
    'profiles/standard-commands.readBatteryLevel must be a function'
  )
  assert.strictEqual(
    typeof heartRate.parseHeartRateMeasurement,
    'function',
    'profiles/heart-rate parser must be a function'
  )
  assert.strictEqual(typeof battery.parseBatteryLevel, 'function', 'profiles/battery-service parser must be a function')
  assert.strictEqual(
    typeof deviceInformation.decodeDeviceInformationString,
    'function',
    'profiles/device-information decoder must be a function'
  )
  assert.strictEqual(
    typeof healthThermometer.parseTemperatureMeasurement,
    'function',
    'profiles/health-thermometer parser must be a function'
  )
  assert.strictEqual(
    typeof bloodPressure.parseBloodPressureMeasurement,
    'function',
    'profiles/blood-pressure parser must be a function'
  )
  assert.strictEqual(
    typeof ieee11073.decodeIeee11073Float,
    'function',
    'profiles/ieee-11073 decoder must be a function'
  )
  assert.strictEqual(
    typeof web.createNavigatorWebBluetoothProvider,
    'function',
    'web.createNavigatorWebBluetoothProvider must be a function'
  )
  assert.strictEqual(typeof advanced.createBleManager, 'function', 'advanced.createBleManager must be a function')
  assert.strictEqual(typeof cli.runUnifiedBleCli, 'function', 'cli.runUnifiedBleCli must be a function')

  console.log(
    `package exports ok: ${publicExports.map(entry => entry.exportPath).join(', ')}; loaded ${runtimeEntries.length} host-neutral CommonJS entries; host-bound entries checked without host loading`
  )
}

module.exports = {
  collectExportTargets,
  derivePublicPackageExports,
  isHostBoundExport,
  selectCommonJsRuntimeExports
}

if (require.main === module) main()
