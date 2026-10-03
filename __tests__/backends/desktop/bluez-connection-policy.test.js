'use strict'

const { admitBluezConnectionPolicy } = require('../../../src/backends/desktop/bluez-connection-policy')

test('BlueZ authority is native-owned by default and explicit owner pins are cloned', () => {
  expect(admitBluezConnectionPolicy(undefined)).toBeUndefined()
  expect(admitBluezConnectionPolicy({ mode: 'le-bearer' })).toBeUndefined()
  for (const policy of [{ mode: 'le-bearer', daemonUniqueOwner: ':1.42' }]) {
    const admitted = admitBluezConnectionPolicy(policy)
    expect(admitted).toEqual(policy)
    expect(admitted).not.toBe(policy)
    expect(Object.isFrozen(admitted)).toBe(true)
  }
})
test.each([
  null,
  [],
  'legacy-device-wide',
  {},
  { mode: 'fallback' },
  { mode: 'legacy-device-wide' },
  { mode: 'le-bearer', daemonUniqueOwner: undefined },
  { mode: 'le-bearer', fallback: true },
  { mode: 'le-bearer', daemonUniqueOwner: 'org.bluez' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1.42\n' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1..42' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1.é' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1.42\0' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1.' + 'a'.repeat(253) },
  { mode: 'le-bearer', daemonUniqueOwner: 42 },
  { mode: 'legacy-device-wide', daemonUniqueOwner: ':1.42' },
  { mode: 'le-bearer', daemonUniqueOwner: ':1.42', fallback: true }
])('malformed/unattested BlueZ policy refuses before dispatch: %p', policy => {
  expect(() => admitBluezConnectionPolicy(policy)).toThrow(
    expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
  )
})

test('production provider snapshots policy and sends it to the native open boundary', async () => {
  const {
    createTestDesktopRustCoreBackendProvider
  } = require('../../../src/backends/desktop/desktop-rust-core-provider')
  const policy = { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }
  const openProduction = jest.fn(async () => {
    throw new Error('intentional open sentinel')
  })
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'bluez',
    owner: 'policy-test',
    now: () => 0,
    hostPlatform: 'linux',
    connectionPolicy: policy,
    binding: {
      listAdapters: async () => [{ index: 0, label: 'hci0', displayName: 'test' }],
      openProduction
    }
  })
  policy.daemonUniqueOwner = ':1.99'
  await provider.listAdapters()
  expect(openProduction).toHaveBeenCalledTimes(1)
  expect(openProduction.mock.calls[0][0].connectionPolicy).toEqual({ mode: 'le-bearer', daemonUniqueOwner: ':1.42' })
})

test.each([
  ['corebluetooth', { mode: 'le-bearer' }],
  ['corebluetooth', { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }],
  ['winrt', { mode: 'le-bearer' }],
  ['winrt', { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }]
])('non-BlueZ provider %s rejects policy before native loading', (platform, connectionPolicy) => {
  const {
    createTestDesktopRustCoreBackendProvider
  } = require('../../../src/backends/desktop/desktop-rust-core-provider')
  const loadBinding = jest.fn()
  expect(() =>
    createTestDesktopRustCoreBackendProvider({
      platform,
      owner: 'policy-test',
      now: () => 0,
      hostPlatform: platform === 'winrt' ? 'win32' : 'darwin',
      loadBinding,
      connectionPolicy
    })
  ).toThrow('argument.invalid')
  expect(loadBinding).not.toHaveBeenCalled()
})

test.each(['corebluetooth', 'winrt'])(
  'non-BlueZ public factories %s do not silently discard policy',
  async platform => {
    const {
      createDesktopCoreBleManager,
      createDesktopCoreProcessHost,
      createDesktopCoreProvider
    } = require('../../../src/node-desktop-manager')
    const options = {
      now: () => 0,
      binding: {
        listAdapters: jest.fn(async () => {
          throw new Error('unexpected native access')
        })
      },
      connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }
    }
    for (const create of [
      () => createDesktopCoreBleManager(platform, options),
      () => createDesktopCoreProcessHost(platform, options),
      () => createDesktopCoreProvider(platform, options, 'node')
    ]) {
      await expect(Promise.resolve().then(create)).rejects.toThrow('argument.invalid')
    }
    expect(options.binding.listAdapters).not.toHaveBeenCalled()
  }
)
