// scripts/ci/prepare-expo-sdk.js

'use strict'

const fs = require('node:fs')
const path = require('node:path')

const USAGE = 'Usage: node scripts/ci/prepare-expo-sdk.js --sdk 57|58'
const EXAMPLE_NAME = 'example-expo'
const MANIFEST_PATH = path.resolve(__dirname, '../../example-expo/package.json')

// SDK57 values are the checked-in example-expo baseline. SDK58 Expo package
// values come from expo@58.0.16 bundledNativeModules.json; babel-preset-expo
// comes from expo@58.0.16 package.json, and the RN toolchain follows RN 0.88.
const SDK_PROFILES = {
  '57': {
    dependencies: {
      '@expo/dom-webview': '57.0.1',
      '@react-native-async-storage/async-storage': '2.2.0',
      expo: '~57.0.23',
      'expo-file-system': '~57.0.7',
      'expo-sharing': '~57.0.22',
      'expo-status-bar': '^57.0.0',
      'expo-system-ui': '^57.0.4',
      react: '~19.2.0',
      'react-dom': '19.2.3',
      'react-native': '~0.86.3',
      'react-native-safe-area-context': '^5.0.0',
      'react-native-screens': '~4.26.0'
    },
    devDependencies: {
      '@react-native/babel-preset': '~0.86.3',
      '@react-native/metro-config': '~0.86.3',
      '@react-native/typescript-config': '~0.86.3',
      '@types/react': '~19.2.0',
      'babel-preset-expo': '~57.0.12'
    }
  },
  '58': {
    dependencies: {
      '@expo/dom-webview': '~58.0.2',
      '@react-native-async-storage/async-storage': '2.2.0',
      expo: '~58.0.16',
      'expo-file-system': '~58.0.10',
      'expo-sharing': '~58.0.19',
      'expo-status-bar': '~58.0.3',
      'expo-system-ui': '~58.0.6',
      react: '19.3.0',
      'react-dom': '19.3.0',
      'react-native': '0.88.0',
      'react-native-safe-area-context': '~5.10.1',
      'react-native-screens': '~4.30.0'
    },
    devDependencies: {
      '@react-native/babel-preset': '~0.88.0',
      '@react-native/metro-config': '~0.88.0',
      '@react-native/typescript-config': '~0.88.0',
      '@types/react': '~19.3.0',
      'babel-preset-expo': '~58.0.18'
    }
  }
}

function freezeProfile(profile) {
  Object.freeze(profile.dependencies)
  Object.freeze(profile.devDependencies)
  return Object.freeze(profile)
}

for (const sdk of Object.keys(SDK_PROFILES)) freezeProfile(SDK_PROFILES[sdk])
Object.freeze(SDK_PROFILES)

function isDependencyMap(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function assertSupportedSdk(sdk) {
  if (sdk !== '57' && sdk !== '58') throw new Error(USAGE)
}

function assertExampleManifest(manifest) {
  if (manifest === null || typeof manifest !== 'object' || Array.isArray(manifest)) {
    throw new Error('Expected example-expo package.json to contain a JSON object')
  }
  if (manifest.name !== EXAMPLE_NAME) {
    throw new Error(`Expected package name ${EXAMPLE_NAME}; refusing to modify another manifest`)
  }
  if (!isDependencyMap(manifest.dependencies)) {
    throw new Error('Expected example-expo manifest to contain a dependencies map')
  }
  if (!isDependencyMap(manifest.devDependencies)) {
    throw new Error('Expected example-expo manifest to contain a devDependencies map')
  }
}

function applySdkProfile(manifest, sdk) {
  assertSupportedSdk(sdk)
  assertExampleManifest(manifest)
  const profile = SDK_PROFILES[sdk]

  return {
    ...manifest,
    dependencies: { ...manifest.dependencies, ...profile.dependencies },
    devDependencies: { ...manifest.devDependencies, ...profile.devDependencies }
  }
}

function parseArguments(argv) {
  if (!Array.isArray(argv) || argv.length !== 2 || argv[0] !== '--sdk') throw new Error(USAGE)
  assertSupportedSdk(argv[1])
  return { sdk: argv[1] }
}

function runCli(argv = process.argv.slice(2), manifestPath = MANIFEST_PATH) {
  // Parse before reading or writing anything so malformed invocations are
  // guaranteed to leave the checkout untouched.
  const { sdk } = parseArguments(argv)
  const source = fs.readFileSync(manifestPath, 'utf8')
  let manifest
  try {
    manifest = JSON.parse(source)
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    throw new Error(`Could not parse ${manifestPath}: ${message}`)
  }

  const selected = applySdkProfile(manifest, sdk)
  const changed = JSON.stringify(selected) !== JSON.stringify(manifest)
  if (changed) fs.writeFileSync(manifestPath, `${JSON.stringify(selected, null, 2)}\n`)

  const state = changed ? 'updated' : 'already selected'
  console.log(`Expo SDK ${sdk} profile ${state} in ${path.relative(process.cwd(), manifestPath)}`)
  console.log('Next: node examples-shared/dev/install-example-dependencies.js example-expo --no-frozen-lockfile')
  console.log('Then: node examples-shared/dev/install-example-dependencies.js --expo-fix example-expo')
  return { changed, sdk, manifest: selected }
}

if (require.main === module) {
  try {
    runCli()
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    console.error(`prepare-expo-sdk: ${message}`)
    process.exitCode = 1
  }
}

module.exports = {
  EXAMPLE_NAME,
  SDK_PROFILES,
  applySdkProfile,
  assertExampleManifest,
  parseArguments,
  runCli
}
