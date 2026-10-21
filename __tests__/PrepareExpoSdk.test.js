// __tests__/PrepareExpoSdk.test.js

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const {
  SDK_PROFILES,
  applySdkProfile,
  parseArguments,
  runCli
} = require('../scripts/ci/prepare-expo-sdk')

const root = path.join(__dirname, '..')
const checkedInManifest = JSON.parse(fs.readFileSync(path.join(root, 'example-expo', 'package.json'), 'utf8'))
// Keep this fixture stable when a qualification leg has selected SDK58 in the
// checkout: SDK57 is always reconstructed through the canonical profile.
const sdk57Fixture = applySdkProfile(checkedInManifest, '57')

describe('prepare-expo-sdk', () => {
  test('SDK57 profile is the example-expo baseline and is idempotent', () => {
    const selected = applySdkProfile(sdk57Fixture, '57')

    expect(selected).toEqual(sdk57Fixture)
    expect(applySdkProfile(selected, '57')).toEqual(selected)
    expect(SDK_PROFILES['57'].dependencies.expo).toBe('~57.0.23')
    expect(SDK_PROFILES['57'].dependencies['react-native']).toBe('~0.86.3')
  })

  test('SDK58 profile aligns Expo, React, RN and SDK-bound native modules', () => {
    const selected = applySdkProfile(sdk57Fixture, '58')

    expect(selected.dependencies.expo).toBe('~58.0.16')
    expect(selected.dependencies['@expo/dom-webview']).toBe('~58.0.2')
    expect(selected.dependencies['expo-file-system']).toBe('~58.0.10')
    expect(selected.dependencies['expo-sharing']).toBe('~58.0.19')
    expect(selected.dependencies['expo-status-bar']).toBe('~58.0.3')
    expect(selected.dependencies['expo-system-ui']).toBe('~58.0.6')
    expect(selected.dependencies.react).toBe('19.3.0')
    expect(selected.dependencies['react-dom']).toBe('19.3.0')
    expect(selected.dependencies['react-native']).toBe('0.88.0')
    expect(selected.dependencies['react-native-safe-area-context']).toBe('~5.10.1')
    expect(selected.dependencies['react-native-screens']).toBe('~4.30.0')
    expect(selected.devDependencies['@react-native/babel-preset']).toBe('~0.88.0')
    expect(selected.devDependencies['@react-native/metro-config']).toBe('~0.88.0')
    expect(selected.devDependencies['@react-native/typescript-config']).toBe('~0.88.0')
    expect(selected.devDependencies['babel-preset-expo']).toBe('~58.0.18')
    expect(selected.devDependencies['@types/react']).toBe('~19.3.0')
  })

  test('selection preserves unrelated manifest fields and dependency entries', () => {
    const fixture = {
      ...sdk57Fixture,
      description: 'qualification fixture',
      dependencies: {
        ...sdk57Fixture.dependencies,
        'unrelated-package': '^1.2.3'
      },
      devDependencies: {
        ...sdk57Fixture.devDependencies,
        'unrelated-tool': '^4.5.6'
      }
    }

    const selected = applySdkProfile(fixture, '58')

    expect(selected.description).toBe(fixture.description)
    expect(selected.dependencies['unrelated-package']).toBe('^1.2.3')
    expect(selected.devDependencies['unrelated-tool']).toBe('^4.5.6')
    expect(selected.scripts.start).toBe(fixture.scripts.start)
  })

  test('profiles can be selected in both directions without stale SDK constraints', () => {
    const sdk58 = applySdkProfile(sdk57Fixture, '58')
    const sdk57 = applySdkProfile(sdk58, '57')

    expect(sdk57).toEqual(sdk57Fixture)
    expect(sdk58.dependencies.expo.startsWith('~57.')).toBe(false)
    expect(sdk58.dependencies['react-native'].startsWith('~0.86.')).toBe(false)
    expect(sdk58.devDependencies['@react-native/babel-preset'].startsWith('~0.86.')).toBe(false)
    expect(sdk58.devDependencies['babel-preset-expo'].startsWith('~57.')).toBe(false)
  })

  test('argument parsing requires exactly --sdk 57 or --sdk 58', () => {
    expect(parseArguments(['--sdk', '57'])).toEqual({ sdk: '57' })
    expect(parseArguments(['--sdk', '58'])).toEqual({ sdk: '58' })

    for (const args of [[], ['--sdk'], ['--sdk', '56'], ['--sdk', '58', '--extra'], ['58']]) {
      expect(() => parseArguments(args)).toThrow(/Usage: node scripts\/ci\/prepare-expo-sdk\.js --sdk 57\|58/)
    }
  })

  test('valid CLI selections are idempotent and invalid arguments do not write', () => {
    const tempDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'prepare-expo-sdk-'))
    const manifestPath = path.join(tempDirectory, 'package.json')
    const prettySdk57 = `${JSON.stringify(sdk57Fixture, null, 2)}\n`
    fs.writeFileSync(manifestPath, prettySdk57)
    const logSpy = jest.spyOn(console, 'log').mockImplementation(() => {})

    try {
      expect(runCli(['--sdk', '57'], manifestPath).changed).toBe(false)
      expect(fs.readFileSync(manifestPath, 'utf8')).toBe(prettySdk57)

      expect(runCli(['--sdk', '58'], manifestPath).changed).toBe(true)
      const sdk58Bytes = fs.readFileSync(manifestPath, 'utf8')
      expect(runCli(['--sdk', '58'], manifestPath).changed).toBe(false)
      expect(fs.readFileSync(manifestPath, 'utf8')).toBe(sdk58Bytes)
      expect(logSpy).toHaveBeenCalledWith(expect.stringContaining('Expo SDK 58 profile'))

      expect(() => runCli(['--sdk', '56'], manifestPath)).toThrow(
        /Usage: node scripts\/ci\/prepare-expo-sdk\.js --sdk 57\|58/
      )
      expect(fs.readFileSync(manifestPath, 'utf8')).toBe(sdk58Bytes)
    } finally {
      logSpy.mockRestore()
      fs.rmSync(tempDirectory, { recursive: true, force: true })
    }
  })

  test('manifest identity and dependency maps are required', () => {
    expect(() => applySdkProfile({ ...sdk57Fixture, name: 'other-app' }, '58')).toThrow(/example-expo/)
    expect(() => applySdkProfile({ ...sdk57Fixture, dependencies: undefined }, '58')).toThrow(/dependencies map/)
    expect(() => applySdkProfile({ ...sdk57Fixture, devDependencies: [] }, '58')).toThrow(/devDependencies map/)
  })
})
