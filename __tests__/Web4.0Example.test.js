// __tests__/Web4.0Example.test.js

'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

describe('4.0 Web Bluetooth public example', () => {
  test('uses only packed clean-baseline public entrypoints', () => {
    const app = read('example-web/app.js')
    expect(app).not.toContain("from 'unified-ble-manager'")
    expect(app).toContain("from 'unified-ble-manager/web'")
    expect(app).toContain("from 'unified-ble-manager/profiles/battery-service'")
    expect(app).toContain("from 'unified-ble-manager/profiles/heart-rate'")
    expect(app).not.toContain('react-native-ble-plx')
    expect(app).not.toContain('/src/')
  })

  test('uses the current single public Web manager and diagnostics API', () => {
    const app = read('example-web/app.js')
    const packedWeb = read('fixtures/g6a-packed-consumer/web-heart-rate-protocol.mjs')
    for (const source of [app, packedWeb]) {
      expect(source).not.toContain('session.chooser')
      expect(source).not.toContain('session.manager')
      expect(source).not.toContain('localResourceCounters')
      expect(source).not.toContain('identity.runtime')
      expect(source).not.toContain('deadline: null')
    }

    expect(app).toContain('manager.choose(')
    expect(app).toContain('manager.connect(')
    expect(app).toContain('selectedPeerId = selection.id')
    expect(packedWeb).toContain('manager.connect(selection.id')
    expect(app).toContain('manager.diagnostics.resourceCounters()')
    expect(app).not.toMatch(/createWebBleManager\(\s*\{/u)

    expect(packedWeb).toContain("createWebBleManagerWithEnvironment({")
    expect(packedWeb).toContain('manager.choose(')
    expect(packedWeb).toContain('manager.connect(')
    expect(packedWeb).toContain('manager.diagnostics.resourceCounters()')
    expect(packedWeb).not.toMatch(/createWebBleManager\(\s*\{/u)
    for (const retiredKey of ['provider:', 'clientId:', 'managerId:']) {
      expect(packedWeb).not.toContain(retiredKey)
    }
  })

  test('proves chooser, connect, discovery, read, notification, reconnect, and cleanup controls', () => {
    const app = read('example-web/app.js')
    const html = read('example-web/index.html')
    for (const operation of [
      'manager.choose',
      'manager.connect',
      'connection.discover',
      'batteryCharacteristic.read',
      'measurementCharacteristic.subscribe',
      'subscription.remove',
      'connection.disconnect',
      'manager.destroy'
    ]) {
      expect(app).toContain(operation)
    }
    for (const control of ['choose-connect', 'reconnect', 'disconnect', 'destroy']) {
      expect(html).toContain(`id="${control}"`)
    }
  })

  test('uses public GATT characteristic operations and public subscription options', () => {
    const app = read('example-web/app.js')
    expect(app).not.toContain("from 'unified-ble-manager/profiles/standard-commands'")
    expect(app).not.toMatch(/\b(?:readBatteryLevel|subscribeHeartRateMeasurements|resetHeartRateEnergyExpended)\s*\(/u)
    expect(app).not.toMatch(/\bdatabase\.(?:read|subscribe|write)\s*\(/u)
    expect(app).not.toContain("preset: 'balanced'")
    expect(app).not.toContain('localNamePrefix: null')
    expect(app).toContain('database.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)')
    expect(app).toContain('batteryCharacteristic.read(operationOptions)')
    expect(app).toContain('measurementCharacteristic.subscribe(notificationOptions)')
    expect(app).toContain("stream: 'balanced'")
    expect(app).toContain("delivery: 'prefer-notification'")

    const packedWeb = read('fixtures/g6a-packed-consumer/web-heart-rate-protocol.mjs')
    expect(packedWeb).not.toContain("from 'unified-ble-manager/profiles/standard-commands'")
    expect(packedWeb).not.toMatch(/\b(?:readBatteryLevel|subscribeHeartRateMeasurements|resetHeartRateEnergyExpended)\s*\(/u)
    expect(packedWeb).not.toMatch(/\bdatabase\.(?:read|subscribe|write)\s*\(/u)
    expect(packedWeb).not.toContain("preset: 'balanced'")
    expect(packedWeb).toContain('database.characteristic(')
    expect(packedWeb).toContain('HEART_RATE_MEASUREMENT_CHARACTERISTIC')
    expect(packedWeb).toContain('measurementCharacteristic.subscribe(notificationOptions)')
    expect(packedWeb).toContain('.write(encodeResetEnergyExpended()')
    expect(packedWeb).toContain('stream: {')
    expect(packedWeb).toContain("preset: 'custom'")
    expect(packedWeb).toContain("delivery: 'prefer-notification'")
  })

  test('is built with the repository-owned bundler and documented as physical evidence only when retained', () => {
    const build = read('scripts/examples/build-web-example.js')
    const readme = read('example-web/README.md')
    const continuousIntegrationWorkflow = read('.github/workflows/ci.yml')
    const publishWorkflow = read('.github/workflows/publish.yml')
    expect(build).toContain("require('webpack')")
    expect(continuousIntegrationWorkflow).toContain('run: pnpm build:example:web')
    expect(publishWorkflow).toContain('run: pnpm build:example:web')
    expect(readme).toContain('4.0 clean-baseline Web Bluetooth example')
    expect(readme).toMatch(/does not itself create a\s+release evidence receipt/u)
    expect(readme).not.toContain('Historical Web Bluetooth example')
  })
})
