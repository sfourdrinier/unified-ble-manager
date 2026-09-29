'use strict'

const h = require('../../helpers/desktop-rust-core-harness')
const { DesktopRustCoreBackend } = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

test('post-native-open constructor preparation failure compensates exact central', async () => {
  const harness = h.realBinding('corebluetooth')
  const originalCause = new Error('capability projection refused')
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'corebluetooth',
    owner: 'constructor-test',
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: 'darwin'
  })
  const adapters = await provider.listAdapters()
  const closes = harness.calls.filter(([name]) => name === 'close').length
  const originalOpen = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (...args) =>
    new Proxy(await originalOpen(...args), {
      get(target, key, receiver) {
        if (key === 'runtimeCapabilityStates')
          return async () => {
            throw originalCause
          }
        return Reflect.get(target, key, receiver)
      }
    })
  try {
    await expect(provider.create({ selectedAdapterId: adapters[0].adapterId })).rejects.toBe(originalCause)
    expect(harness.calls.filter(([name]) => name === 'close')).toHaveLength(closes + 1)
  } finally {
    for (const central of harness.opened) await central.close()
  }
})

test('provider initialization refusal retains exact cleanup retry authority', async () => {
  const originalDestroy = DesktopRustCoreBackend.prototype.destroy
  const originalCause = new Error('initialization refused after native allocation')
  const open = jest.spyOn(DesktopRustCoreBackend.prototype, 'open').mockRejectedValueOnce(originalCause)
  const destroy = jest
    .spyOn(DesktopRustCoreBackend.prototype, 'destroy')
    .mockRejectedValueOnce(new Error('cleanup refused'))
  let failure
  try {
    await h.openBackend('corebluetooth')
  } catch (error) {
    failure = error
  }
  try {
    expect(failure).toMatchObject({ originalCause, code: 'platform.failure', retryCleanup: expect.any(Function) })
    destroy.mockImplementation(originalDestroy)
    expect(await failure.retryCleanup()).toEqual({ state: 'released', failures: [] })
  } finally {
    // Retain actual fixture ownership even while the old implementation is red.
    destroy.mockImplementation(originalDestroy)
    const receiver = destroy.mock.contexts[0]
    if (receiver) await originalDestroy.call(receiver)
    open.mockRestore()
    destroy.mockRestore()
  }
})

test('adapter listing cleanup refusal is not hidden in an unavailable descriptor', async () => {
  const originalDestroy = DesktopRustCoreBackend.prototype.destroy
  const cleanupCause = new Error('temporary listing cleanup refused')
  const destroy = jest.spyOn(DesktopRustCoreBackend.prototype, 'destroy').mockRejectedValueOnce(cleanupCause)
  let failure
  try {
    await h.openBackend('corebluetooth')
  } catch (error) {
    failure = error
  }
  try {
    expect(failure).toMatchObject({ code: 'platform.failure', cleanupCause, retryCleanup: expect.any(Function) })
    destroy.mockImplementation(originalDestroy)
    expect(await failure.retryCleanup()).toEqual({ state: 'released', failures: [] })
  } finally {
    destroy.mockImplementation(originalDestroy)
    const receiver = destroy.mock.contexts[0]
    if (receiver) await originalDestroy.call(receiver)
    destroy.mockRestore()
  }
})
