const fs = require('node:fs')
const path = require('node:path')
const read = file => fs.readFileSync(path.join(__dirname, '..', file), 'utf8')

test('BlueZ consumer guidance separates explicit daemon preparation, strict discovery and qualification', () => {
  const node = read('docs/NODE.md')
  expect(node).toContain('BLUEZ_LE_GATT.md')
  expect(node).toContain('org.unifiedblemanager.LEGatt1.GetSnapshot')
  expect(node).not.toContain('omission now leaves discovery available')
  expect(read('README.md')).toContain('docs/BLUEZ_LE_GATT.md')
  expect(read('RELEASE.md')).toContain('source-only daemon-extension build')
  const guide = read('docs/BLUEZ_LE_GATT.md')
  for (const requirement of [
    'capability.unsupported',
    'daemonUniqueOwner',
    'owner/attachment/revision',
    'prepare-isolated.sh',
    'build-test-isolated.sh',
    'explicit host action',
    '--experimental',
    'KernelExperimental',
    '/etc/bluetooth',
    '/var/lib/bluetooth',
    'isolated-test binary',
    'not physical-radio qualification',
    'ServicesResolved',
    'No daemon is installed',
    'indeterminate',
    'canceled waiter',
    'NoReply',
    'unrelated devices',
    'explicit rediscovery can reverify'
  ])
    expect(guide).toContain(requirement)
  expect(guide).not.toContain('sudo make install')
})

test('BlueZ guidance distinguishes fresh LE discovery from inherited hash-matched cache', () => {
  for (const file of ['docs/BLUEZ_LE_GATT.md', 'vendor/bluez/README.md']) {
    const guide = read(file)
    for (const requirement of ['in-memory', 'matching database hash', 'cached files or bonds', 'Classic'])
      expect(guide).toContain(requirement)
  }
})
