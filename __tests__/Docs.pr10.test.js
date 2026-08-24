const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

describe('PR10 Expo documentation', () => {
  test('documents Expo v2 as the published RC4 installation path', () => {
    const gettingStarted = read('docs/GETTING_STARTED.md')
    const expoPlugin = read('docs/EXPO_PLUGIN.md')
    const documents = `${gettingStarted}\n${expoPlugin}`

    expect(documents).toContain('4.0.0-rc.4')
    expect(documents).toContain('immutable')
    expect(documents).toContain('pnpm add unified-ble-manager@4.0.0-rc.4')
    expect(documents).not.toContain('PR10 branch work')
    expect(documents).not.toContain('unreleased')
    expect(documents).not.toMatch(/(?:expo|bunx|npx)\s+(?:install|add)\s+unified-ble-manager(?:\s|$)/u)
  })

  test('uses distinct bare React Native and Expo factories', () => {
    const gettingStarted = read('docs/GETTING_STARTED.md')

    expect(gettingStarted).toContain("import { createReactNativeBleManager } from 'unified-ble-manager/react-native'")
    expect(gettingStarted).toContain("import { createExpoBleManager } from 'unified-ble-manager/expo'")
    expect(gettingStarted).toContain('const manager = await createExpoBleManager()')
  })
})
