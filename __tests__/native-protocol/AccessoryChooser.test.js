const fs = require('node:fs')
const path = require('node:path')
const root = path.resolve(__dirname, '../..')
const sources = [
  'ios/AccessoryChoiceOwner.swift',
  'ios/AccessoryChoiceAdmission.swift',
  'ios/UnifiedBleAccessoryChooser.swift'
]

test('ASK sources belong to the actual pod, weak-link only iOS, and exclude unavailable SDK APIs from TV/macOS', () => {
  const pod = fs.readFileSync(path.join(root, 'unified-ble-manager.podspec'), 'utf8')
  for (const source of sources) expect(pod).toContain(`"${source}"`)
  expect(pod).toContain('s.ios.weak_frameworks = "AccessorySetupKit"')
  const native = fs.readFileSync(path.join(root, sources[2]), 'utf8')
  expect(native.indexOf('#if os(iOS) && !targetEnvironment(macCatalyst)')).toBeLessThan(
    native.indexOf('import AccessorySetupKit')
  )
  expect(native.indexOf('Self.items(optionsJson')).toBeLessThan(native.indexOf('ASAccessorySession()'))
})
