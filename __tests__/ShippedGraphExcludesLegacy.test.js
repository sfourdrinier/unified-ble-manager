// __tests__/ShippedGraphExcludesLegacy.test.js
//
// Deletion guard for the legacy desktop backends, the legacy React Native
// Native Protocol route and their providers. Every module reachable from a
// `package.json` export (value and type imports alike, since the published
// declarations resolve type imports too) must stay clear of them, so they can
// be deleted without touching the shipped graph.

const fs = require('fs')
const path = require('path')
const ts = require('typescript')

const root = path.resolve(__dirname, '..')

const LEGACY_PATTERNS = [
  /^src\/backends\/corebluetooth\//,
  /^src\/backends\/winrt\//,
  /^src\/backends\/bluez\//,
  /^src\/backends\/legacy-native-require\.ts$/,
  /^src\/native-protocol\//,
  /^src\/NativeUnifiedBleProtocolControl\.ts$/,
  /^src\/backends\/reactnative\/react-native-android-provider\.ts$/,
  /^src\/backends\/reactnative\/react-native-apple-provider\.ts$/,
  /^src\/backends\/reactnative\/react-native-android-security\.ts$/,
  /^src\/backends\/reactnative\/react-native-android-peer-directory\.ts$/,
  /^src\/backends\/reactnative\/react-native-provider-cleanup\.ts$/,
  /^src\/tck\/first-party\/(corebluetooth|winrt|bluez)-tck-registration\.ts$/,
  /^test-support\//
]

const SOURCE_EXTENSIONS = ['.ts', '.tsx', '/index.ts', '/index.tsx', '.js', '/index.js']

function exportEntrySources() {
  const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
  const entries = new Set()
  for (const [subpath, target] of Object.entries(packageJson.exports)) {
    if (typeof target === 'string') {
      continue
    }
    const declaration = target.import?.types
    if (typeof declaration !== 'string') {
      throw new Error(`Export ${subpath} has no import types declaration to map back to its source`)
    }
    const source = declaration.replace('./lib/typescript/module/', '').replace(/\.d\.ts$/, '.ts')
    entries.add(resolveFile(path.join(root, source)) ?? missing(subpath, source))
  }
  return [...entries]
}

function missing(from, specifier) {
  throw new Error(`${from}: cannot resolve ${specifier}`)
}

function resolveFile(candidateBase) {
  const candidates = [candidateBase, ...SOURCE_EXTENSIONS.map(extension => candidateBase + extension)]
  return candidates.find(candidate => fs.existsSync(candidate) && fs.statSync(candidate).isFile()) ?? null
}

function relativeSpecifiers(file) {
  const { importedFiles } = ts.preProcessFile(fs.readFileSync(file, 'utf8'), true, true)
  return importedFiles.map(imported => imported.fileName).filter(specifier => specifier.startsWith('.'))
}

function shippedGraph() {
  const importerOf = new Map(exportEntrySources().map(entry => [entry, null]))
  const queue = [...importerOf.keys()]
  while (queue.length > 0) {
    const file = queue.shift()
    for (const specifier of relativeSpecifiers(file)) {
      const resolved = resolveFile(path.resolve(path.dirname(file), specifier))
      if (resolved === null) {
        missing(path.relative(root, file), specifier)
      }
      if (!importerOf.has(resolved)) {
        importerOf.set(resolved, file)
        queue.push(resolved)
      }
    }
  }
  return importerOf
}

function importChain(importerOf, file) {
  const chain = []
  for (let current = file; current !== null; current = importerOf.get(current)) {
    chain.unshift(path.relative(root, current))
  }
  return chain.join(' -> ')
}

describe('shipped module graph', () => {
  const importerOf = shippedGraph()

  test('reaches the entrypoints of every host', () => {
    const reached = new Set([...importerOf.keys()].map(file => path.relative(root, file)))
    for (const expected of [
      'src/index.ts',
      'src/node-corebluetooth.ts',
      'src/node-winrt.ts',
      'src/node-bluez.ts',
      'src/electron-main.ts',
      'src/react-native.ts',
      'src/testing.ts',
      'src/backends/desktop/desktop-rust-core-provider.ts',
      'src/backends/desktop/platform-identity.ts',
      'src/backends/reactnative/react-native-rust-core-provider.ts'
    ]) {
      expect(reached).toContain(expected)
    }
  })

  test('imports nothing from the legacy backends, the Native Protocol route or legacy providers', () => {
    const violations = [...importerOf.keys()]
      .filter(file => LEGACY_PATTERNS.some(pattern => pattern.test(path.relative(root, file))))
      .map(file => importChain(importerOf, file))
    expect(violations).toEqual([])
  })
})

describe('relocated legacy values', () => {
  const schema = fs.readFileSync(path.join(root, 'src/native-protocol/generated/native-protocol-v2-schema.ts'), 'utf8')
  const limits = require('../src/backends/reactnative/react-native-protocol-limits')

  function schemaNumber(name) {
    const match = new RegExp(`export const ${name} = (\\d+)`).exec(schema)
    if (match === null) {
      throw new Error(`${name} missing from the generated schema`)
    }
    return Number(match[1])
  }

  test('React Native limits match the generated Native Protocol schema while it exists', () => {
    expect(limits.MAXIMUM_CONTROL_RECORD_BYTES).toBe(schemaNumber('MAXIMUM_CONTROL_RECORD_BYTES'))
    expect(limits.MAXIMUM_BINARY_PAYLOAD_BYTES).toBe(schemaNumber('MAXIMUM_BINARY_PAYLOAD_BYTES'))
    const outcomes = /export const restorationOutcomes = Object\.freeze\(\[([^\]]*)\]\)/.exec(schema)
    expect(outcomes).not.toBeNull()
    expect([...limits.restorationOutcomes]).toEqual([...outcomes[1].matchAll(/'([^']+)'/g)].map(match => match[1]))
  })

  test('legacy desktop modules re-export the relocated identities rather than redefining them', () => {
    const identity = require('../src/backends/desktop/platform-identity')
    expect(require('../src/backends/corebluetooth/corebluetooth-provider').coreBluetoothCompatibility).toBe(
      identity.coreBluetoothCompatibility
    )
    expect(require('../src/backends/winrt/winrt-provider').winRtCompatibility).toBe(identity.winRtCompatibility)
    expect(require('../src/backends/bluez/bluez-backend-provider').bluezCompatibility).toBe(
      identity.bluezCompatibility
    )
    expect(require('../src/backends/bluez/bluez-dbus-contract').BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON).toBe(
      identity.BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON
    )
    expect(identity.COREBLUETOOTH_BACKEND_ID).toBe('unified-ble:corebluetooth')
    expect(identity.WINRT_BACKEND_ID).toBe('unified-ble:winrt')
    expect(identity.BLUEZ_BACKEND_ID).toBe('unified-ble:bluez-dbus')
  })
})
