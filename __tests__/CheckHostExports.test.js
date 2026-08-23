const path = require('path')

const root = path.join(__dirname, '..')
const packageJson = require('../package.json')
const {
  derivePublicPackageExports,
  isHostBoundExport,
  selectCommonJsRuntimeExports
} = require('../scripts/ci/check-host-exports')

describe('check-host-exports package surface', () => {
  test('derives every public export-map entry, including the PR10 host entries', () => {
    const derived = derivePublicPackageExports(packageJson.exports)

    expect(derived.map(entry => entry.exportPath)).toEqual(Object.keys(packageJson.exports).sort())
    expect(derived.map(entry => entry.exportPath)).toEqual(
      expect.arrayContaining(['./cli', './react', './tauri', './advanced', './expo'])
    )
  })

  test('does not load host-bound exports from the generic Node checker runtime', () => {
    expect(isHostBoundExport('./app.plugin.js')).toBe(true)
    expect(isHostBoundExport('./react')).toBe(true)
    expect(isHostBoundExport('./react-native')).toBe(true)
    expect(isHostBoundExport('./expo')).toBe(true)
    expect(isHostBoundExport('./tauri')).toBe(true)
    expect(isHostBoundExport('./node/bluez')).toBe(true)
    expect(isHostBoundExport('./electron/main')).toBe(true)
    expect(isHostBoundExport('./advanced')).toBe(false)
    expect(isHostBoundExport('./cli')).toBe(false)

    const runtimeExports = selectCommonJsRuntimeExports(packageJson.exports, root)
    expect(runtimeExports.map(entry => entry.exportPath)).toEqual(
      expect.arrayContaining(['.', './cli', './advanced', './web'])
    )
    expect(runtimeExports.map(entry => entry.exportPath)).not.toEqual(
      expect.arrayContaining(['./app.plugin.js', './react', './react-native', './expo', './tauri'])
    )
  })
})
