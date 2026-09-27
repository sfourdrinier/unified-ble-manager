import test from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { existsSync, readFileSync, realpathSync, statSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const require = createRequire(import.meta.url)
const ts = require('typescript')
const app = fileURLToPath(new URL('../../..', import.meta.url))
const config = ts.readConfigFile(resolve(app, 'tsconfig.json'), ts.sys.readFile)
const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, app)
const installed = resolve(app, 'node_modules/unified-ble-manager')
const manifest = JSON.parse(readFileSync(resolve(installed, 'package.json'), 'utf8'))
const specifiers = [
  'unified-ble-manager',
  'unified-ble-manager/backend-sdk',
  'unified-ble-manager/expo',
  'unified-ble-manager/react-native',
  'unified-ble-manager/profiles/heart-rate',
  'unified-ble-manager/profiles/battery-service',
  'unified-ble-manager/profiles/device-information'
]

test('actual Expo runtime resolver never redirects package imports through type-only aliases', () => {
  const appRequire = createRequire(resolve(app, 'package.json'))
  const expoRequire = createRequire(appRequire.resolve('expo/package.json'))
  const cli = dirname(expoRequire.resolve('@expo/cli/package.json'))
  const { createTypescriptResolver } = require(resolve(cli, 'build/src/start/server/metro/createTypescriptResolver.js'))
  const attempted = []
  const resolver = createTypescriptResolver({
    projectRoot: app,
    watch: false,
    getMetroBundler: () => ({
      _depGraph: {
        doesFileExist: name => existsSync(resolve(app, name)),
        _fileSystem: {
          lookup: path =>
            existsSync(path)
              ? { exists: true, type: statSync(path).isFile() ? 'f' : 'd', realPath: realpathSync(path) }
              : { exists: false },
          hierarchicalLookup: (directory, path) => {
            const from = createRequire(resolve(directory, 'package.json'))
            try {
              return { absolutePath: from.resolve(path.replace(/^node_modules\//, '')) }
            } catch (error) {
              if (error.code === 'MODULE_NOT_FOUND') return null
              throw error
            }
          }
        }
      }
    }),
    getStrictResolver: () => target => {
      attempted.push(target)
      return { type: 'sourceFile', filePath: target }
    }
  })
  for (const originModulePath of [
    resolve(app, 'src/driver/app-driver.ts'),
    resolve(app, '../examples-shared/driver/host.ts')
  ]) {
    for (const specifier of specifiers) {
      assert.equal(resolver({ originModulePath }, specifier, 'android'), null, specifier)
    }
  }
  assert.deepEqual(attempted, [])
})

test('shared and app types resolve to the same installed package as Metro', () => {
  assert.deepEqual(parsed.errors, [])
  for (const suffix of [
    '',
    '/backend-sdk',
    '/expo',
    '/react-native',
    '/profiles/heart-rate',
    '/profiles/battery-service',
    '/profiles/device-information'
  ]) {
    const specifier = `unified-ble-manager${suffix}`
    const target = manifest.exports[suffix === '' ? '.' : `.${suffix}`].import.types
    for (const origin of [
      resolve(app, 'src/driver/app-driver.ts'),
      resolve(app, '../examples-shared/driver/host.ts')
    ]) {
      const actual = ts.resolveModuleName(specifier, origin, parsed.options, ts.sys).resolvedModule
      assert.ok(actual, `${specifier} from ${origin}`)
      assert.equal(
        realpathSync(actual.resolvedFileName),
        realpathSync(resolve(installed, target)),
        `${specifier} from ${origin}`
      )
    }
  }
})
