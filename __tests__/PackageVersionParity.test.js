// __tests__/PackageVersionParity.test.js

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const packageJson = require('../package.json')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

function sourceFiles(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const entryPath = path.join(directory, entry.name)
    if (entry.isDirectory()) return sourceFiles(entryPath)
    return entry.isFile() && entry.name.endsWith('.ts') ? [entryPath] : []
  })
}

describe('production implementation version', () => {
  test('has one source authority matching the package version', () => {
    const { UNIFIED_BLE_IMPLEMENTATION_VERSION } = require('../src/implementation-version')

    expect(UNIFIED_BLE_IMPLEMENTATION_VERSION).toBe(packageJson.version)

    const declaration = 'export const UNIFIED_BLE_IMPLEMENTATION_VERSION'
    const filesDeclaringImplementationVersion = sourceFiles(path.join(root, 'src'))
      .filter(file => fs.readFileSync(file, 'utf8').includes(declaration))
      .map(file => path.relative(root, file).split(path.sep).join('/'))

    expect(filesDeclaringImplementationVersion).toEqual(['src/implementation-version.ts'])
  })

  test('current source and install declarations match the package version', () => {
    const documents = [
      ['llms.txt source declaration', 'llms.txt', /^- This source tree is `([^`]+)`\./mu],
      ['Node guide source declaration', 'docs/NODE.md', /^All three execute .*? This source targets `([^`]+)`\./mu],
      ['Electron guide source declaration', 'docs/ELECTRON.md', /^This source targets `([^`]+)`\./mu],
      ['llms.txt install pin', 'llms.txt', /^Install this release with `npm add unified-ble-manager@([^`]+)`;/mu],
      ['example install pin', 'example/README.md', /^`unified-ble-manager@([^`]+)`, after that exact version is published/mu],
      ['release bare-install expectation', 'RELEASE.md', /^  entrypoints\. A separate bare install must select npm `latest` \(`([^`]+)` after\s+stable publication\)\./mu]
    ]

    for (const [label, relativePath, pattern] of documents) {
      const match = pattern.exec(read(relativePath))
      if (match === null) throw new Error(`${label} is missing its anchored current declaration`)
      expect(match[1]).toBe(packageJson.version)
    }
  })
})
