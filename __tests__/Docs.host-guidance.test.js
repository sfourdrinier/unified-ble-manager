const fs = require('node:fs')
const path = require('node:path')

const read = name => fs.readFileSync(path.join(__dirname, '..', name), 'utf8')

test('Android monitoring documentation preserves notification denial without inventing a startup gate', () => {
  const guide = read('docs/PLATFORMS.md')
  const driver = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/background/AndroidConnectedDeviceForegroundServiceDriver.java')
  expect(driver).toMatch(/static String\[\] requiredRuntimePermissions\(int sdk\)[\s\S]*?Manifest\.permission\.BLUETOOTH_CONNECT/)
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
      permissions: { request: jest.fn(async () => ({ granted: granted ? ['bluetooth'] : [], denied: granted ? [] : ['bluetooth'] })) },
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
