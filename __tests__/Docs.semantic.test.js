// __tests__/Docs.semantic.test.js

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const teaching = [
  'README.md',
  'MIGRATION_4.0.md',
  'docs/GETTING_STARTED.md',
  'docs/TUTORIALS.md',
  'docs/HELPERS.md',
  'docs/WEB.md',
  'docs/NODE.md',
  'docs/ELECTRON.md',
  'docs/TAURI.md',
  'docs/PROFILES_AND_COMMANDS.md',
  'docs/EXPO_PLUGIN.md'
]

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
}

function fences(document) {
  const blocks = []
  const regex = /```[^\n]*\n([\s\S]*?)```/g
  let match
  while ((match = regex.exec(document)) !== null) {
    const preceding = document.slice(0, match.index)
    if (preceding.trimEnd().endsWith('<!-- ubm-fence: pseudo -->')) {
      continue
    }
    blocks.push(match[1])
  }
  return blocks
}

describe('teaching documentation semantic BLE rules', () => {
  const documents = teaching.map(relativePath => ({ relativePath, text: read(relativePath) }))

  test('does not export or teach invalid SIG reads', () => {
    for (const { relativePath, text } of documents) {
      expect(`${relativePath}:${text}`).not.toMatch(/readHeartRateMeasurement|readBloodPressureMeasurement|readTemperatureMeasurement/)
    }
  })

  test('does not teach isBackgroundEnabled or the ios/android ternary', () => {
    for (const { relativePath, text } of documents) {
      expect(`${relativePath}:${text}`).not.toContain('isBackgroundEnabled')
      expect(`${relativePath}:${text}`).not.toContain("Platform.OS === 'ios' ? 'apple' : 'android'")
    }
  })

  test('does not claim the same manager contract on every host', () => {
    for (const { text } of documents) {
      expect(text).not.toContain('Same manager contract on every host')
    }
  })

  test('name-dependent scans do not use duplicatePolicy first', () => {
    for (const { relativePath, text } of documents) {
      for (const fence of fences(text)) {
        if (fence.includes('localName')) {
          expect(`${relativePath}:${fence}`).not.toContain("duplicatePolicy: 'first'")
        }
      }
    }
  })

  test('Battery Level is not written in a teaching fence', () => {
    for (const { relativePath, text } of documents) {
      for (const fence of fences(text)) {
        if (fence.includes('batteryLevelSelector') || fence.includes('BATTERY_LEVEL')) {
          expect(`${relativePath}:${fence}`).not.toMatch(/\.write\(/)
        }
      }
    }
  })

  test('Heart Rate Measurement is not read in a teaching fence', () => {
    for (const { relativePath, text } of documents) {
      for (const fence of fences(text)) {
        if (fence.includes('heartRateMeasurementSelector')) {
          expect(`${relativePath}:${fence}`).not.toContain('database.read')
        }
      }
    }
  })

  test('does not invent applicationWriteSelector or treat getDevices as a package API', () => {
    for (const { relativePath, text } of documents) {
      expect(`${relativePath}:${text}`).not.toContain('applicationWriteSelector')
      if (text.includes('getDevices()')) {
        expect(`${relativePath}:${text}`).toMatch(/navigator\.bluetooth\.getDevices/)
      }
    }
  })

  test('example README AbortSignal claim matches the service', () => {
    const exampleReadme = read('example/README.md')
    const service = read('example/src/services/BLEService/BLEService.ts')
    if (exampleReadme.includes('AbortSignal')) {
      expect(service).toMatch(/AbortController|signal:/)
    }
  })
})
