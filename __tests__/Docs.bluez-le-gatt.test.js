const fs = require('node:fs')
const path = require('node:path')
const read = file => fs.readFileSync(path.join(__dirname, '..', file), 'utf8')

test('Linux prerequisite diagnostics distinguish refused authority from package installation', () => {
  const guide = read('docs/BLUEZ_DEPLOYMENT.md').replace(/\s+/g, ' ')
  expect(guide).toContain('connection.authority')
  expect(guide).toContain('observation-timeout')
  expect(guide).toContain('compile-config-loadability')
  expect(guide).toContain('does not verify the running daemon')
  expect(guide).toContain('Never invoke installation from a renderer or reconnect handler')
  expect(guide).toContain('ready-callback lifetime')
})

test('maintained Linux authority distinguishes optional LE observer from cached discovery', () => {
  const guide = read('docs/BLUEZ_DEPLOYMENT.md').replace(/\s+/g, ' ')
  expect(guide).toContain('GetLeAvailability')
  expect(guide).toContain('LeAdvertisement')
  expect(guide).toContain('capability.unsupported')
  expect(guide).toContain('older daemons still support direct scoped connection')
  expect(guide).toContain('sender-scoped LE discovery session')
  expect(guide).toContain('merged discovery filters')
  expect(guide).toContain('Unchanged RSSI')
  expect(guide).toContain('producer/private-bus tests are not physical-radio proof')
  expect(guide).toContain('No cache shortcut, polling loop or hidden retry substitutes')
})

test('current BlueZ deployment commands use the sealed producer release identity', () => {
  const manifest = JSON.parse(read('vendor/bluez/source-asset-manifest.json'))
  const releases = read('docs/BLUEZ_DEPLOYMENT.md').match(/\b5\.87-ubm\.\d+/g)
  expect(releases).not.toBeNull()
  expect(new Set(releases)).toEqual(new Set([manifest.distribution.release]))
})

test('Tauri Linux setup describes native owner binding and optional stricter policy', () => {
  const guide = read('docs/TAURI.md')
  expect(guide).toContain('resolves and pins')
  expect(guide).toContain('optional stricter')
  expect(guide).toContain('LinuxAuthority1.GetContract')
  expect(guide).not.toContain('Without attestation scanning remains')
  expect(guide).not.toContain('must also supply')
  const options = read('native/tauri/src/btleplug_dispatcher.rs')
  expect(options).not.toContain('Omission permits scanning, not connections')
  expect(read('src/electron-main.ts')).not.toContain('omission permits scanning only')
  for (const file of ['example-node/README.md', 'example-electron/README.md', 'example-tauri/README.md']) {
    const example = read(file)
    expect(example).toContain('native authority resolves and pins')
    expect(example).toContain('optional stricter')
    expect(example).not.toContain('omission leaves Linux')
    expect(example).not.toContain('omission does not permit Linux')
    expect(example).not.toContain('Omission does not grant connection authority')
  }
})

test('Tauri security guidance distinguishes scoped IPC routing from native capability', () => {
  const guide = read('docs/TAURI.md')
  expect(guide).toContain('default transport permission grants none of these security scopes')
  expect(guide).toContain('instantiated native authority')
  expect(guide).toContain('CoreBluetooth')
  expect(guide).not.toContain('still reports all generic security capabilities as unsupported')
})

test('desktop IPC guides require the option-aware protocol on both sides', () => {
  expect(read('docs/TAURI.md')).toContain('IPC protocol 5')
  expect(read('docs/ELECTRON.md')).toContain('exactly version 5')
  for (const file of ['docs/TAURI.md', 'docs/ELECTRON.md']) {
    const guide = read(file)
    expect(guide).toContain('targeting')
    expect(guide).toContain('protocol 4')
    expect(guide).toContain('protocol.incompatible')
  }
})

test('Electron security grants are trusted host facts, never renderer-controlled defaults', () => {
  const guide = read('docs/ELECTRON.md')
  expect(guide).toContain('`securityPermissions`')
  expect(guide).toContain('defaults to no security permissions')
  for (const permission of ['state', 'pair', 'cancel-pairing', 'unpair', 'custom-ceremony']) {
    expect(guide).toContain(`\`security:${permission}\``)
  }
})

test('all BlueZ deployment guides retain the exact production release reply signature', () => {
  const source = read('vendor/bluez-async/src/le_lease.rs')
  const signature = source.match(/ReleaseLease requires exact ([a-z]+) signature/)[1]
  expect(signature).toBe('uttsby')
  for (const file of ['docs/BLUEZ_DEPLOYMENT.md', 'docs/BLUEZ_LE_GATT.md', 'vendor/bluez/README.md']) {
    expect(read(file)).toContain(`\`${signature}\``)
    expect(read(file)).not.toContain('`(u,t,t,s)`')
  }
})

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

test('BlueZ recovery guidance separates link acceptance from a new ATT attachment', () => {
  const guide = read('docs/BLUEZ_LE_GATT.md')
  for (const requirement of [
    'LE link acceptance can precede primary ATT attachment',
    'retired disconnected attachment',
    'new attachment',
    'original five-second',
    'same pinned daemon owner',
    'current discovery failure'
  ])
    expect(guide).toContain(requirement)
})

test('daemon owner death retires only daemon obligations, not local cleanup or physical facts', () => {
  const guide = read('docs/BLUEZ_DEPLOYMENT.md')
  for (const text of [
    'bus-confirmed unique-owner disappearance',
    'NameHasOwner',
    'local iterator',
    'no physical disconnect reason',
    'unresponsive but still-live'
  ])
    expect(guide).toContain(text)
})
