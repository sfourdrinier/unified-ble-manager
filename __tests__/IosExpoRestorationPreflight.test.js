// __tests__/IosExpoRestorationPreflight.test.js
//
// Expo's config plugin writes the restoration contract into the generated
// Info.plist.  A stale generated project silently loses that contract, so the
// iOS launch command must reject it before Xcode builds an app that cannot be
// restored by CoreBluetooth.

'use strict'

const path = require('node:path')

const preflightPath = path.join(__dirname, '..', 'examples-shared', 'dev', 'verify-expo-ios-restoration.js')
const { configuredRestoration, inspectGeneratedIosRestoration } = require(preflightPath)

const expected = Object.freeze({
  restorationId: 'example-expo-primary',
  generation: '1'
})

function generatedInfo(overrides = {}) {
  return {
    UIBackgroundModes: ['bluetooth-central'],
    UnifiedBleProtocolRestorationId: expected.restorationId,
    UnifiedBleProtocolRestorationGeneration: expected.generation,
    ...overrides
  }
}

describe('Expo iOS generated restoration preflight', () => {
  test('reads the fixture restoration configuration from app.json', () => {
    const appConfig = require('../example-expo/app.json')
    expect(configuredRestoration(appConfig)).toEqual(expected)
  })

  test('accepts a generated Info.plist with the configured restoration contract', () => {
    expect(inspectGeneratedIosRestoration(expected, generatedInfo())).toEqual({ ok: true })
  })

  test('rejects a generated Info.plist that lost the restoration identifier', () => {
    expect(
      inspectGeneratedIosRestoration(expected, generatedInfo({ UnifiedBleProtocolRestorationId: undefined }))
    ).toEqual({
      ok: false,
      missing: ['UnifiedBleProtocolRestorationId']
    })
  })

  test('rejects a generated Info.plist that lost bluetooth-central background mode', () => {
    expect(inspectGeneratedIosRestoration(expected, generatedInfo({ UIBackgroundModes: [] }))).toEqual({
      ok: false,
      missing: ['UIBackgroundModes.bluetooth-central']
    })
  })
})
