const fs = require('fs')
const path = require('path')
const {
  derivePackedHostConsumerExports,
  PACKED_HOST_CONSUMER_CONTRACTS
} = require('../scripts/ci/packed-host-consumer-check')

const root = path.join(__dirname, '..')

describe('packed Expo/Tauri consumer release gate', () => {
  test('derives host coverage from conditional package export targets', () => {
    const exportsMap = {
      './renamed-expo-entry': {
        import: { default: './lib/module/expo.js' },
        require: { default: './lib/commonjs/expo.js' }
      },
      './renamed-tauri-entry': {
        import: { default: './lib/module/tauri.js' },
        require: { default: './lib/commonjs/tauri.js' }
      },
      './unrelated-entry': {
        import: { default: './lib/module/web.js' },
        require: { default: './lib/commonjs/web.js' }
      }
    }

    expect(derivePackedHostConsumerExports(exportsMap)).toEqual([
      { exportPath: './renamed-expo-entry', host: 'expo' },
      { exportPath: './renamed-tauri-entry', host: 'tauri' }
    ])
  })

  test('current package exports require exactly the supported Expo and Tauri packed contracts', () => {
    const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))

    expect(derivePackedHostConsumerExports(packageJson.exports)).toEqual([
      { exportPath: './expo', host: 'expo' },
      { exportPath: './tauri', host: 'tauri' }
    ])
    expect(Object.keys(PACKED_HOST_CONSUMER_CONTRACTS).sort()).toEqual(['expo', 'tauri'])
  })

  test('runner is a published-tarball gate and cannot use repository aliases', () => {
    const runner = fs.readFileSync(path.join(root, 'scripts/ci/packed-host-consumer-check.js'), 'utf8')

    expect(runner).toContain("npmCommand(), ['pack'")
    expect(runner).toContain("installedFrom: 'packed-tarball'")
    expect(runner).toContain('require.resolve')
    expect(runner).not.toContain('moduleNameMapper')
    expect(runner).not.toContain("'unified-ble-manager': 'file:")
    expect(runner).toContain('physicalRadio:')
    expect(runner).toContain("'not-provided'")
  })

  test('blocking CI and publish packed gates run the focused check after generic smoke', () => {
    const ci = fs.readFileSync(path.join(root, '.github/workflows/ci.yml'), 'utf8')
    const publish = fs.readFileSync(path.join(root, '.github/workflows/publish.yml'), 'utf8')
    const command = 'node scripts/ci/packed-host-consumer-check.js'

    expect(ci.split(command)).toHaveLength(2)
    expect(ci).toContain("if: runner.os == 'Linux' && matrix.node == '22'")
    const ciSmokeIndex = ci.indexOf('- name: Canonical npm pack + install export smoke')
    const ciFocusedIndex = ci.indexOf('- name: Packed Expo/Tauri consumer proof')
    const ciG6AIndex = ci.indexOf('- name: G6A packed consumer proof')
    expect(ciFocusedIndex).toBeGreaterThan(ciSmokeIndex)
    expect(ciG6AIndex).toBeGreaterThan(ciFocusedIndex)

    expect(publish.split(command)).toHaveLength(2)
    const publishSmokeIndex = publish.indexOf('- name: Canonical pack+install export smoke')
    const publishFocusedIndex = publish.indexOf('- name: Packed Expo/Tauri consumer proof')
    const publishG6AIndex = publish.indexOf('- name: G6A packed consumer proof')
    expect(publishFocusedIndex).toBeGreaterThan(publishSmokeIndex)
    expect(publishG6AIndex).toBeGreaterThan(publishFocusedIndex)
  })
})
