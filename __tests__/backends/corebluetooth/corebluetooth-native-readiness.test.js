// __tests__/backends/corebluetooth/corebluetooth-native-readiness.test.js

const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')
const addonSource = fs.readFileSync(path.join(repositoryRoot, 'native/electron/corebluetooth/src/addon.mm'), 'utf8')
const bridgePath = path.join(repositoryRoot, 'native/electron/corebluetooth')
const nativeAddonPath = path.join(bridgePath, 'build', 'Release', 'unified_ble_corebluetooth.node')

function withDarwinPlatform(run) {
  const originalDescriptor = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: 'darwin' })
  try {
    return run()
  } finally {
    if (originalDescriptor === undefined) {
      delete process.platform
    } else {
      Object.defineProperty(process, 'platform', originalDescriptor)
    }
  }
}

function loadBoundary(radio) {
  let createContractBoundary
  jest.isolateModules(() => {
    jest.doMock('fs', () => ({
      ...jest.requireActual('fs'),
      existsSync: candidate => candidate === nativeAddonPath
    }))
    jest.doMock(nativeAddonPath, () => ({ createNativeRadio: () => radio }), { virtual: true })
    ;({ createContractBoundary } = require(bridgePath))
  })
  return createContractBoundary
}

function createRadio() {
  return {
    startScan: jest.fn(() => Promise.resolve()),
    stopScan: jest.fn(() => Promise.resolve()),
    connect: jest.fn(() => Promise.resolve()),
    disconnect: jest.fn(() => Promise.resolve()),
    getConnectionState: jest.fn(() => 'disconnected'),
    getAdapterState: jest.fn(() => 'PoweredOn'),
    readRssi: jest.fn(() => Promise.resolve(-61)),
    maximumWriteValueLengthForType: jest.fn(() => Promise.resolve(182)),
    canSendWriteWithoutResponse: jest.fn(() =>
      Promise.resolve({
        id: 'peripheral-id',
        connectionGeneration: 'native-generation-1',
        ready: true,
        ordinal: 16
      })
    ),
    discoverServices: jest.fn(() => Promise.resolve([])),
    discoverCharacteristicsAt: jest.fn(() => Promise.resolve([])),
    readDescriptorAt: jest.fn(() => Promise.resolve(Buffer.alloc(0))),
    writeDescriptorAt: jest.fn(() => Promise.resolve()),
    readCharacteristicAt: jest.fn(() => Promise.resolve(Buffer.alloc(0))),
    writeCharacteristicAt: jest.fn(() => Promise.resolve()),
    startNotifyAt: jest.fn(() => Promise.resolve()),
    stopNotifyAt: jest.fn(() => Promise.resolve()),
    setDisconnectHandler: jest.fn(),
    setDatabaseChangedHandler: jest.fn(),
    setAdapterStateHandler: jest.fn(),
    setWriteWithoutResponseReadinessHandler: jest.fn(),
    destroy: jest.fn(() => Promise.resolve())
  }
}

describe('CoreBluetooth native write-readiness boundary', () => {
  afterEach(() => {
    jest.resetModules()
    jest.dontMock('fs')
  })

  test('uses CoreBluetooth readiness truth and carries generation-safe native ordinals through the bridge', async () => {
    expect(addonSource).toContain('canSendWriteWithoutResponse')
    expect(addonSource).toContain('peripheralIsReadyToSendWriteWithoutResponse')
    expect(addonSource).toContain('writeWithoutResponseReadinessHandler')
    expect(addonSource).toContain('connectionGeneration')
    expect(addonSource).toContain('ordinal')
    expect(addonSource).toContain('BlockingCall')

    const bridgeSource = fs.readFileSync(path.join(repositoryRoot, 'native/electron/corebluetooth/index.js'), 'utf8')
    expect(bridgeSource).toContain("'canSendWriteWithoutResponse'")
    expect(bridgeSource).toContain('onWriteWithoutResponseReadiness')

    const radio = createRadio()
    const createContractBoundary = loadBoundary(radio)

    await withDarwinPlatform(async () => {
      const boundary = createContractBoundary()
      await expect(boundary.canSendWriteWithoutResponse('peripheral-id')).resolves.toMatchObject({
        nativePeerId: 'peripheral-id',
        connectionGeneration: 'native-generation-1',
        ready: true,
        ordinal: 16
      })
      expect(radio.canSendWriteWithoutResponse).toHaveBeenCalledWith('peripheral-id')

      const events = []
      const stop = boundary.onWriteWithoutResponseReadiness(event => events.push(event))
      const nativeHandler = radio.setWriteWithoutResponseReadinessHandler.mock.calls[0][0]
      nativeHandler({
        id: 'peripheral-id',
        connectionGeneration: 'native-generation-1',
        ready: true,
        ordinal: 17
      })
      stop()
      nativeHandler({
        id: 'ignored-after-unsubscribe',
        connectionGeneration: 'native-generation-1',
        ready: true,
        ordinal: 18
      })

      expect(events).toEqual([
        {
          nativePeerId: 'peripheral-id',
          connectionGeneration: 'native-generation-1',
          ready: true,
          ordinal: 17
        }
      ])

      await boundary.destroy()
      expect(radio.setWriteWithoutResponseReadinessHandler).toHaveBeenLastCalledWith(null)
    })
  })
})
