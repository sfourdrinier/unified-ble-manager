let mockNative
jest.mock('react-native', () => ({
  Platform: { OS: 'ios', Version: '26.0' },
  TurboModuleRegistry: { get: () => mockNative }
}))
const {
  DeterministicRustCoreNative,
  defaultPeripheral
} = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManager } = require('../../../src/react-native-app-manager')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')

beforeEach(() => {
  mockNative = new DeterministicRustCoreNative({ platform: 'apple' })
  mockNative.accessoryChooserAvailable = jest.fn(async () => true)
  mockNative.chooseAccessory = jest.fn(async () =>
    JSON.stringify({
      revision: 'ubm-accessory-chooser/1',
      peripheralIdentifier: '12345678-1234-1234-1234-123456789abc',
      name: 'Sensor'
    })
  )
  mockNative.cancelAccessoryChoice = jest.fn(async () => {})
})

test('ordinary RN factory exposes native ASK choose with scoped peer identity and truthful hybrid discovery', async () => {
  const manager = await createReactNativeBleManager()
  expect(manager.discovery.kind).toBe('hybrid')
  expect(manager.capabilities.get('discovery:system-chooser').state).toBe('limited')
  const peer = await manager.choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
  expect(peer.id).not.toBe('12345678-1234-1234-1234-123456789abc')
  expect(peer.sources).toEqual(['origin-authorized'])
  expect(mockNative.chooseAccessory).toHaveBeenCalledTimes(1)
  await manager.destroy()
})

test.each([
  [{ serviceUuids: ['180d'] }, { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb' }],
  [{ manufacturerData: [{ companyIdentifier: 107 }] }, { companyIdentifier: 107, manufacturerPrefix: [] }]
])('ordinary RN factory forwards identifier-only ASK filtering: %j', async (filter, nativeFilter) => {
  const manager = await createReactNativeBleManager()
  try {
    const peer = await manager.choose({ filters: [filter], timeoutMs: 30000 })
    expect(peer.sources).toEqual(['origin-authorized'])
    expect(JSON.parse(mockNative.chooseAccessory.mock.calls[0][1]).filters).toEqual([nativeFilter])
    expect(mockNative.opsInvoked('scan.start')).toEqual([])
  } finally {
    await manager.destroy()
  }
})

test('unavailable host remains continuous-scan and cannot allocate a setup session', async () => {
  mockNative.accessoryChooserAvailable.mockResolvedValue(false)
  const manager = await createReactNativeBleManager()
  expect(manager.discovery.kind).toBe('continuous-scan')
  await expect(
    manager.choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
  ).rejects.toMatchObject({ code: 'capability.unsupported' })
  expect(mockNative.chooseAccessory).not.toHaveBeenCalled()
  await manager.destroy()
})

test('previously unobserved ASK peer stays scoped and reaches the native connect route without scanning', async () => {
  const identifier = '12345678-1234-1234-1234-123456789abc'
  mockNative.peripherals.set(identifier, defaultPeripheral(identifier))
  const manager = await createReactNativeBleManager()
  const peer = await manager.choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
  expect(peer.id).not.toBe(identifier)
  const connection = await manager.connect(peer)
  expect(mockNative.opsInvoked('connection.connect')[0].peerId).toBe(identifier)
  expect(mockNative.opsInvoked('scan.start')).toEqual([])
  expect(mockNative.opsInvoked('peers.remember')).toEqual([])
  await connection.release()
  await manager.destroy()
})

test('destroyed manager refuses picker allocation and cancels its pending native owner', async () => {
  let rejectChoice
  mockNative.chooseAccessory.mockImplementation(
    () =>
      new Promise((_, reject) => {
        rejectChoice = reject
      })
  )
  mockNative.cancelAccessoryChoice.mockImplementation(async () => {
    rejectChoice(new Error('native picker cancelled'))
  })
  const manager = await createReactNativeBleManager()
  const choice = manager.choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
  const observed = choice.catch(error => error)
  while (mockNative.chooseAccessory.mock.calls.length === 0) await Promise.resolve()
  await manager.destroy()
  expect(mockNative.cancelAccessoryChoice).toHaveBeenCalledTimes(1)
  expect(await observed).toBeInstanceOf(Error)
  await expect(
    manager.choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
  ).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  expect(mockNative.chooseAccessory).toHaveBeenCalledTimes(1)
})

test('setup identity mismatch is refused before availability or OS UI', async () => {
  mockNative.identity = { ...mockNative.identity, sourceDigest: '0'.repeat(64) }
  const binding = createReactNativeRustCoreBinding({ platform: 'apple', native: mockNative })
  await expect(binding.accessoryChooserAvailable()).rejects.toMatchObject({
    normalized: { code: 'protocol.incompatible' }
  })
  expect(mockNative.accessoryChooserAvailable).not.toHaveBeenCalled()
  expect(mockNative.chooseAccessory).not.toHaveBeenCalled()
})

test('held picker cancellation cannot block parent session release and remains retryable', async () => {
  let rejectChoice
  let releaseCancel
  mockNative.chooseAccessory.mockImplementation(
    () =>
      new Promise((_, reject) => {
        rejectChoice = reject
      })
  )
  mockNative.cancelAccessoryChoice.mockImplementation(
    () =>
      new Promise(resolve => {
        releaseCancel = () => {
          rejectChoice(new Error('cancelled'))
          resolve()
        }
      })
  )
  const manager = await createReactNativeBleManager()
  const observed = manager
    .choose({ filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] })
    .catch(error => error)
  while (mockNative.chooseAccessory.mock.calls.length === 0) await Promise.resolve()
  const receipt = await manager.destroy()
  expect(receipt.state).toBe('release-failed')
  releaseCancel()
  expect(await observed).toBeInstanceOf(Error)
  expect((await manager.destroy()).state).toBe('released')
}, 5000)

test('malformed capability probe is fail-closed, not a truthy platform promise', async () => {
  mockNative.accessoryChooserAvailable.mockResolvedValue('yes')
  const binding = createReactNativeRustCoreBinding({ platform: 'apple', native: mockNative })
  await expect(binding.accessoryChooserAvailable()).rejects.toMatchObject({
    normalized: { code: 'protocol.malformed' }
  })
})
