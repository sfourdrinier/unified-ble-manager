// __tests__/backends/reactnative/rn-production-graph.test.js
//
// FIX-PLAN directive (no legacy, no opt-in, no fallback): the React Native and
// Expo production entrypoints must not load the legacy protocol control, the
// legacy TypeScript providers, the Native Protocol v2 JSI boundaries or the
// TypeScript core. Walks the runtime (non-type) import graph of the sources.

const fs = require('node:fs')
const path = require('node:path')

const SRC = path.join(__dirname, '../../../src')

function resolve(from, specifier) {
  const base = path.resolve(path.dirname(from), specifier)
  for (const candidate of [`${base}.ts`, `${base}.tsx`, path.join(base, 'index.ts')]) {
    if (fs.existsSync(candidate)) return candidate
  }
  return null
}

function runtimeGraph(entry) {
  const visited = new Set()
  const pending = [path.join(SRC, entry)]
  while (pending.length > 0) {
    const file = pending.pop()
    if (visited.has(file)) continue
    visited.add(file)
    const source = fs.readFileSync(file, 'utf8')
    const specifiers = [
      ...source.matchAll(/^(?:import|export)\s+(?!type\b)(?:[^'";]*?\sfrom\s+)?['"](\.[^'"]+)['"]/gm),
      ...source.matchAll(/require\(['"](\.[^'"]+)['"]\)/g)
    ].map(match => match[1])
    for (const specifier of specifiers) {
      const target = resolve(file, specifier)
      if (target !== null) pending.push(target)
    }
  }
  return [...visited].map(file => path.relative(SRC, file).split(path.sep).join('/'))
}

const FORBIDDEN = [
  'NativeUnifiedBleProtocolControl.ts',
  'backends/reactnative/react-native-android-provider.ts',
  'backends/reactnative/react-native-apple-provider.ts',
  'backends/reactnative/react-native-android-security.ts',
  'backends/reactnative/react-native-android-peer-directory.ts',
  'native-protocol/rn-android-boundary.ts',
  'native-protocol/rn-apple-boundary.ts',
  'backends/corebluetooth/corebluetooth-backend.ts',
  'core/unified-ble-core.ts'
]

function exportedInterfaceBody(source, name) {
  const declaration = `export interface ${name}`
  const declarationOffset = source.indexOf(declaration)
  if (declarationOffset < 0) throw new Error(`Missing exported interface ${name}`)
  const bodyStart = source.indexOf('{', declarationOffset)
  if (bodyStart < 0) throw new Error(`Missing body for exported interface ${name}`)

  let depth = 0
  for (let offset = bodyStart; offset < source.length; offset += 1) {
    if (source[offset] === '{') depth += 1
    if (source[offset] === '}') {
      depth -= 1
      if (depth === 0) return source.slice(bodyStart + 1, offset)
    }
  }
  throw new Error(`Unclosed body for exported interface ${name}`)
}

describe.each(['react-native.ts', 'expo.ts', 'react-native-app-manager.ts', 'react-native-manager.ts'])(
  'production graph of src/%s',
  entry => {
    const graph = runtimeGraph(entry)

    test('reaches the Rust binding', () => {
      expect(graph).toContain('backends/reactnative/react-native-rust-core-binding.ts')
    })

    test.each(FORBIDDEN)('never loads %s', forbidden => {
      expect(graph).not.toContain(forbidden)
    })
  }
)

test('public React Native manager options omit the removed legacy route', () => {
  const managerOptions = exportedInterfaceBody(
    fs.readFileSync(path.join(SRC, 'react-native-manager.ts'), 'utf8'),
    'ReactNativeBleManagerOptions'
  )
  const applicationOptions = exportedInterfaceBody(
    fs.readFileSync(path.join(SRC, 'react-native-app-manager.ts'), 'utf8'),
    'CreateReactNativeBleManagerOptions'
  )
  const publicOptions = exportedInterfaceBody(
    fs.readFileSync(path.join(SRC, 'public/host-identity.ts'), 'utf8'),
    'BleManagerCreateOptions'
  )

  for (const options of [managerOptions, applicationOptions, publicOptions]) {
    expect(options).not.toMatch(/\breadonly\s+legacyTypeScriptCore\s*[?:]/)
    expect(options).not.toMatch(/\breadonly\s+control\s*[?:]/)
  }
})

test('allows a retained continuation control record without admitting it as a manager option', () => {
  const expo = fs.readFileSync(path.join(SRC, 'expo.ts'), 'utf8')
  const retainedPrefix = exportedInterfaceBody(expo, 'ExpoContinuationRetainedPrefixV1')

  expect(retainedPrefix).toMatch(/\breadonly\s+control\s*:\s*ContinuationRetainedPrefixV1\['control'\]/)
})
