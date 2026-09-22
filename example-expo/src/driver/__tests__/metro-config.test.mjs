// Expo host adapter: Metro must watch the shared driver and resolve its
// `unified-ble-manager` imports from this app, so the bundle holds one package
// instance (the one the native module registers against).

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import path from 'node:path'

const require = createRequire(import.meta.url)
const projectRoot = path.resolve(import.meta.dirname, '../../..')
const sharedRoot = path.resolve(projectRoot, '../examples-shared')
const config = require(path.join(projectRoot, 'metro.config.js'))

function resolveFrom(originModulePath, moduleName) {
  const seen = []
  const context = { originModulePath, resolveRequest: (ctx, name) => (seen.push({ origin: ctx.originModulePath, name }), { type: 'empty' }) }
  config.resolver.resolveRequest(context, moduleName, 'ios')
  return seen[0]
}

test('metro watches the shared driver folder', () => {
  assert.ok(config.watchFolders.includes(sharedRoot))
})

test('unified-ble-manager imports from the shared driver resolve from the app, other imports are untouched', () => {
  const sharedFile = path.join(sharedRoot, 'driver/scenarios/heart-rate.ts')
  assert.equal(resolveFrom(sharedFile, 'unified-ble-manager').origin, path.join(projectRoot, 'package.json'))
  assert.equal(resolveFrom(sharedFile, 'unified-ble-manager/profiles/heart-rate').origin, path.join(projectRoot, 'package.json'))
  assert.equal(resolveFrom(sharedFile, '../protocol.ts').origin, sharedFile)
  assert.equal(resolveFrom(sharedFile, 'unified-ble-manager-other').origin, sharedFile)
  const appFile = path.join(projectRoot, 'src/driver/app-driver.ts')
  assert.equal(resolveFrom(appFile, 'unified-ble-manager').origin, appFile)
})
