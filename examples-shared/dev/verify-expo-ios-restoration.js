// examples-shared/dev/verify-expo-ios-restoration.js
//
// Expo's config plugin writes the restoration contract into the generated
// Info.plist.  Check that generated output immediately before Xcode builds it.

'use strict'

const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

function configuredRestoration(appConfig) {
  const plugins = appConfig?.expo?.plugins
  if (!Array.isArray(plugins)) throw new Error('verify-expo-ios-restoration: app.json has no Expo plugin configuration')
  const entry = plugins.find(candidate => Array.isArray(candidate) && candidate[0] === 'unified-ble-manager')
  const restoration = entry?.[1]?.background?.ios?.restoration
  if (typeof restoration?.id !== 'string' || restoration.id.length === 0) {
    throw new Error('verify-expo-ios-restoration: app.json does not configure background.ios.restoration.id')
  }
  const generation = restoration.generation ?? '1'
  if (typeof generation !== 'string' || generation.length === 0) {
    throw new Error('verify-expo-ios-restoration: app.json has an invalid background.ios.restoration.generation')
  }
  return Object.freeze({ restorationId: restoration.id, generation })
}

function inspectGeneratedIosRestoration(expected, info) {
  const missing = []
  const modes = Array.isArray(info.UIBackgroundModes) ? info.UIBackgroundModes : []
  if (!modes.includes('bluetooth-central')) missing.push('UIBackgroundModes.bluetooth-central')
  if (info.UnifiedBleProtocolRestorationId !== expected.restorationId) missing.push('UnifiedBleProtocolRestorationId')
  if (info.UnifiedBleProtocolRestorationGeneration !== expected.generation) {
    missing.push('UnifiedBleProtocolRestorationGeneration')
  }
  return missing.length === 0 ? { ok: true } : { ok: false, missing }
}

function generatedInfoPlist(exampleDirectory) {
  const iosDirectory = path.join(exampleDirectory, 'ios')
  if (!fs.existsSync(iosDirectory)) {
    throw new Error('verify-expo-ios-restoration: generated ios/ is missing; run expo prebuild before the iOS build')
  }
  const candidates = fs
    .readdirSync(iosDirectory, { withFileTypes: true })
    .filter(entry => entry.isDirectory())
    .map(entry => path.join(iosDirectory, entry.name, 'Info.plist'))
    .filter(candidate => fs.existsSync(candidate))
  if (candidates.length !== 1) {
    throw new Error(
      `verify-expo-ios-restoration: expected one generated app Info.plist, found ${String(candidates.length)}`
    )
  }
  return candidates[0]
}

function readPlist(plistPath) {
  const result = spawnSync('plutil', ['-convert', 'json', '-o', '-', plistPath], { encoding: 'utf8' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0)
    throw new Error(`verify-expo-ios-restoration: plutil failed for ${plistPath}: ${result.stderr}`)
  return JSON.parse(result.stdout)
}

function main(argv) {
  const exampleDirectory = path.resolve(argv[2] ?? '.')
  const appConfig = JSON.parse(fs.readFileSync(path.join(exampleDirectory, 'app.json'), 'utf8'))
  const expected = configuredRestoration(appConfig)
  const plistPath = generatedInfoPlist(exampleDirectory)
  const outcome = inspectGeneratedIosRestoration(expected, readPlist(plistPath))
  if (!outcome.ok) {
    throw new Error(
      `verify-expo-ios-restoration: generated ${path.relative(exampleDirectory, plistPath)} is missing ${outcome.missing.join(', ')}; run expo prebuild --clean --no-install and rebuild`
    )
  }
  process.stdout.write(
    `verify-expo-ios-restoration: ${path.relative(exampleDirectory, plistPath)} matches app.json restoration configuration.\n`
  )
}

if (require.main === module) main(process.argv)

module.exports = { configuredRestoration, inspectGeneratedIosRestoration }
