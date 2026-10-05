const fs = require('node:fs')
const path = require('node:path')

const read = name => fs.readFileSync(path.join(__dirname, '..', name), 'utf8')

test('platform guidance reflects native desktop peer and deferred-acquisition authority', () => {
  const guide = read('docs/PLATFORMS.md')
  expect(guide).not.toContain('only first-party backend that exposes')
  expect(guide).toContain('Windows and Linux desktop backends also expose')
  expect(guide).toContain('[`PEERS.md`](PEERS.md)')
  expect(guide).toContain('[`NODE.md`](NODE.md)')
  expect(guide).toContain('macOS and Windows support initial')
  expect(guide).toContain('Linux LE-specific deferred acquisition remains unsupported')
  expect(guide).toContain('CoreBluetooth does not expose unrestricted system bond inventory')
})

test('release notes distinguish new desktop acquisition mechanisms from remaining platform gaps', () => {
  const unreleased = read('CHANGELOG.md').split('## [5.0.0-rc.18]')[0]
  expect(unreleased).toContain('typed Windows public/random address targeting')
  expect(unreleased).toContain('Windows/Linux bonded-peer enumeration')
  expect(unreleased).toContain('initial deferred acquisition')
  expect(unreleased).toContain('not automatic post-loss reconnect')
  expect(unreleased).toContain('Linux LE-specific deferred availability remains unsupported')
  expect(unreleased).toContain('`5.87-ubm.4`')
  expect(unreleased).toContain('confirmed loss of the pinned unique daemon owner')
})

test('review directory identifies the shipped rc19 tracker as historical', () => {
  const guide = read('docs/review/README.md')
  expect(guide).toContain('RC19_PORT_REVIEW.md')
  expect(guide).toContain('published rc.19')
  expect(read('docs/review/RC19_PORT_REVIEW.md')).toContain('Status: Historical')
  expect(read('docs/review/RC19_PORT_REVIEW.md')).toContain('f2e98f41e0d416e6abc6a33594b9d445e8c722b6')
  expect(guide).not.toContain('Every document and findings file in this directory is a **historical record**')
  expect(guide).not.toContain('That pair is a live verification')
})

test('published rc19 guidance does not describe its registry availability as pending', () => {
  expect(read('MIGRATION_4.0.28.md')).not.toContain('until then the published baseline')
  expect(read('MIGRATION_4.0.28.md')).not.toContain('published baseline is `5.0.0-rc.18`')
  expect(read('llms.txt')).not.toContain('once published')
  expect(read('docs/GETTING_STARTED.md')).not.toContain('After the npm registry lists')
  expect(read('RELEASE.md')).toContain('## Releasing 5.0.0-rc.19 (historical)')
  expect(read('docs/5.0.0-RELEASE-COMPLETION-TRACKER.md')).toContain('rc.19 publication is verified')
})

test('Tauri missing MTU and Windows ordinary write limits retain their actual semantics', () => {
  const guide = read('docs/TAURI.md')
  expect(guide).toContain('BlueZ omits the live MTU')
  expect(guide).toContain('`capability.unavailable`')
  expect(guide).not.toContain('a withheld measurement answers `capability.unsupported`')
  const capabilities = read('crates/ubm-desktop/src/capabilities.rs')
  expect(capabilities).not.toContain('both modes are bounded by one ATT payload')
  expect(capabilities).toContain('ordinary with-response writes up to 512 bytes')
})

test('Android monitoring documentation preserves notification denial without inventing a startup gate', () => {
  const guide = read('docs/PLATFORMS.md')
  const driver = read(
    'android/src/main/java/com/sfourdrinier/unifiedblemanager/background/AndroidConnectedDeviceForegroundServiceDriver.java'
  )
  expect(driver).toMatch(
    /static String\[\] requiredRuntimePermissions\(int sdk\)[\s\S]*?Manifest\.permission\.BLUETOOTH_CONNECT/
  )
  expect(guide).toContain('`POST_NOTIFICATIONS` is not a prerequisite')
  expect(guide).toContain('The application requests `POST_NOTIFICATIONS` itself')
  expect(guide).not.toContain('UBM intentionally requires `POST_NOTIFICATIONS`')
})

test('TV guidance separates factories while retaining one native runtime and historical records', () => {
  const guide = read('docs/TV.md')
  expect(guide).toContain('createExpoBleManager')
  expect(guide).toContain('createReactNativeBleManager')
  expect(guide).toContain('manager.permissions.request')
  expect(guide).toContain('UnifiedBleRustCore')
  expect(read('docs/TVOS.md')).toMatch(/Status:\*\* Historical/)
  expect(read('docs/TVOS.md')).toContain('[`TV.md`](TV.md)')
  expect(read('docs/GAPS.4.0.md')).toMatch(/Status:\*\* Historical/)
  expect(read('docs/README.md')).toMatch(/\[`GAPS\.4\.0\.md`\].*\| Historical \|/)
  expect(read('docs/PLATFORMS.md')).toContain('[`TV.md`](TV.md)')
  expect(read('docs/PLATFORMS.md')).not.toContain('packed 4.0 contract')
})

test('Expo permission recipe requests explicitly before radio work, rather than relying on state reads', async () => {
  const guide = read('docs/GETTING_STARTED.md')
  expect(guide).toContain('Reading `readiness()` or `adapter.state()` never prompts')
  expect(guide).toContain('ordinary global-authorization path, not an AccessorySetupKit')
  expect(guide).toContain('notDetermined')
  expect(guide).toContain('manager.choose')
  expect(read('README.md')).toContain('AccessorySetupKit-configured iOS')
  const snippet = guide.match(/<!-- expo-permission-flow -->\s*```ts\n([\s\S]*?)\n```/)[1]
  for (const granted of [true, false]) {
    const manager = {
      readiness: jest.fn(async () => ({ state: 'action-required' })),
      permissions: {
        request: jest.fn(async () => ({ granted: granted ? ['bluetooth'] : [], denied: granted ? [] : ['bluetooth'] }))
      },
      adapter: { waitUntilReady: jest.fn(async () => undefined) }
    }
    const execute = new Function('manager', `return (async () => { ${snippet} })()`)
    if (granted) await execute(manager)
    else await expect(execute(manager)).rejects.toThrow('Bluetooth permission was not granted.')
    expect(manager.readiness).toHaveBeenCalledTimes(1)
    expect(manager.permissions.request).toHaveBeenCalledWith({ purpose: 'scan-and-connect' })
    expect(manager.adapter.waitUntilReady).toHaveBeenCalledTimes(granted ? 1 : 0)
  }
  expect(guide).not.toContain('the prompt is raised by _using_ the radio')
})

test('Tauri delivery documentation matches property-based planning rather than blanket refusal', () => {
  const guide = read('docs/TAURI.md')
  expect(guide).toContain('A characteristic offering only notification or only indication')
  expect(guide).toContain('CoreBluetooth and BlueZ enable notification')
  expect(guide).toContain('WinRT can select either mode')
  expect(guide).toContain('gatt.property-not-supported')
  expect(guide).toContain('capability.limited')
  expect(guide).not.toContain('refuses every hard requirement')
  expect(guide).not.toContain('Delivery modes follow the 4.x contract')
})

test('Tauri distinguishes OS-managed ordinary writes from explicit prepared transactions', () => {
  const guide = read('docs/TAURI.md')
  expect(guide).toContain('Ordinary `with-response` writes')
  expect(guide).toContain('caller-controlled prepared/reliable transactions')
  expect(guide).not.toContain('so long writes are rejected')
  expect(guide).toContain('`no-prepared-write-path`')
})

test('Tauri crate peer-directory guidance includes native bonded routes and truthful filters', () => {
  const guide = read('native/tauri/README.md')
  expect(guide).toContain('`peers.bonded`')
  expect(guide).toContain('Windows and Linux')
  expect(guide).toContain('`unified-ble:winrt`')
  expect(guide).toContain('`unified-ble:bluez-dbus`')
  expect(guide).toContain('`scope: "application"`')
  expect(guide).toContain('`peers.bonded.services`')
  expect(guide).toContain('`system-bonded`')
  expect(guide).toContain('Source filters are applied after native lookup')
  expect(guide).toContain('an empty or excluding source list returns no records')
  expect(guide).not.toContain('Other\ncategories and adapters without these native mechanisms report unsupported')
  expect(guide).not.toContain('including empty or source-filtered queries')
})
