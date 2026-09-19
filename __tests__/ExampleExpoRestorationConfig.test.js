'use strict'

// The example Expo app opts into the restoration paths so the `restoration`
// scenario can be tested physically: the plugin option for iOS
// restoreIdentifierKey and Android companion presence.

const fs = require('node:fs')
const path = require('node:path')

const { validateUnifiedBleExpoPluginOptions } = require('../plugin/src/expoPluginSchema')

function pluginOptions() {
  const app = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'example-expo', 'app.json'), 'utf8'))
  const entry = app.expo.plugins.find(plugin => Array.isArray(plugin) && plugin[0] === 'unified-ble-manager')
  expect(entry).toBeDefined()
  return entry[1]
}

describe('example-expo restoration opt-in', () => {
  test('the app config carries a valid iOS restoration identifier and Android companion presence', () => {
    const options = pluginOptions()
    // Schema-valid: throws on a malformed id, a missing central mode, or a
    // foreground-service mode without its notification.
    const validated = validateUnifiedBleExpoPluginOptions(options)
    expect(validated.background?.ios?.mode).toBe('central')
    expect(validated.background?.ios?.restoration?.id).toBe('example-expo-primary')
    expect(validated.background?.android?.mode).toBe('connected-device-foreground-service')
  })
})
