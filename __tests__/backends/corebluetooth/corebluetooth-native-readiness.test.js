// __tests__/backends/corebluetooth/corebluetooth-native-readiness.test.js

const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')
const addonSource = fs.readFileSync(path.join(repositoryRoot, 'native/electron/corebluetooth/src/addon.mm'), 'utf8')
const controlsSource = fs.readFileSync(
  path.join(repositoryRoot, 'src/backends/corebluetooth/corebluetooth-connection-controls.ts'),
  'utf8'
)
const bridgePath = path.join(repositoryRoot, 'native/electron/corebluetooth')
const nativeAddonPath = path.join(bridgePath, 'build', 'Release', 'unified_ble_corebluetooth.node')

async function withDarwinPlatform(run) {
  const originalDescriptor = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: 'darwin' })
  try {
    return await run()
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
    expect(addonSource).toContain('kReadinessTsfnQueueCapacity = 64')
    expect(addonSource).toContain('kReadinessIngressCapacity = 64')
    expect(addonSource).toMatch(/ubm_write_without_response_readiness"\s*,\s*kReadinessTsfnQueueCapacity\s*,\s*1/)
    expect(addonSource).toContain('class ReadinessIngress final')
    expect(addonSource).toContain('std::deque<JsCallbackData *> pending_')
    expect(addonSource).toContain('for (auto it = pending_.rbegin(); it != pending_.rend(); ++it)')
    expect(addonSource).toContain('pending->deviceId == data->deviceId')
    expect(addonSource).toContain('pending->connectionGeneration == data->connectionGeneration')
    expect(addonSource).toContain('delete *it')
    expect(addonSource).toContain('*it = data')
    expect(addonSource).not.toContain('delete pending_.back()')
    expect(addonSource).toContain('const napi_status status = tsfn_.BlockingCall(data, CallJs)')
    expect(addonSource).toContain('if (status != napi_ok) delete data')
    expect(addonSource).toContain('tsfn_.Abort()')
    expect(addonSource).toContain('dispatch_sync(queue_,')
    expect(addonSource).not.toContain('readinessTsfn.NonBlockingCall')
    const readinessIngressStart = addonSource.indexOf('radio_.writeWithoutResponseReadinessHandler = ^')
    const readinessIngressSource = addonSource.slice(readinessIngressStart, readinessIngressStart + 900)
    expect(readinessIngressSource).toContain('readinessIngress->Enqueue(data)')
    expect(readinessIngressSource).not.toContain('BlockingCall')
    expect(readinessIngressSource).not.toContain('NonBlockingCall')
    const readinessReleaseStart = addonSource.indexOf('if (readinessIngress_)')
    const readinessReleaseSource = addonSource.slice(readinessReleaseStart, readinessReleaseStart + 360)
    expect(readinessReleaseSource).toContain('readinessIngress_->Close()')
    expect(readinessReleaseSource).toContain('writeWithoutResponseReadinessTsfn_ = Napi::ThreadSafeFunction()')
    expect(readinessReleaseSource.indexOf('readinessIngress_->Close()')).toBeLessThan(
      readinessReleaseSource.indexOf('writeWithoutResponseReadinessTsfn_ = Napi::ThreadSafeFunction()')
    )
    expect(controlsSource).not.toContain('buffered.push(event)')
    expect(controlsSource).toContain('buffered.current = event')
    expect(controlsSource).toContain('event.ordinal > buffered.current.ordinal')
    const readinessCallbackStart = addonSource.indexOf('peripheralIsReadyToSendWriteWithoutResponse')
    expect(addonSource.slice(readinessCallbackStart, readinessCallbackStart + 900)).toContain(
      'peripheral.canSendWriteWithoutResponse'
    )
    expect(addonSource).toContain('[self.connectionGenerations removeObjectForKey:deviceId]')

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
      nativeHandler({ id: 'malformed', connectionGeneration: 'native-generation-1', ready: true, ordinal: '18' })
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

  test('guards source-aware bounded retention and synchronized TSFN ownership', () => {
    const ingressStart = addonSource.indexOf('class ReadinessIngress final')
    const ingressEnd = addonSource.indexOf('static std::vector<std::uint8_t> CopyBytes', ingressStart)
    const ingressSource = addonSource.slice(ingressStart, ingressEnd)

    expect(ingressSource).toContain('const napi_status acquireStatus = tsfn_.Acquire()')
    expect(ingressSource).toContain('if (acquireStatus != napi_ok)')
    expect(ingressSource).toContain('tsfn_.Release()')
    expect(ingressSource).toContain('for (auto it = pending_.rbegin(); it != pending_.rend(); ++it)')
    expect(ingressSource).toContain('pending->deviceId == data->deviceId')
    expect(ingressSource).toContain('pending->connectionGeneration == data->connectionGeneration')
    expect(ingressSource).toContain('delete *it')
    expect(ingressSource).toContain('*it = data')
    expect(ingressSource).toContain('for (JsCallbackData *data : pending_)')
    expect(addonSource).toContain('if (!readinessIngress->Enqueue(data)) delete data')
    expect(ingressSource).not.toContain('delete pending_.back()')
    expect(ingressSource).toMatch(/different source.*return false/s)
    expect(ingressSource).toContain('if (status != napi_ok) delete data')
    expect(addonSource).toContain('if (!env || !jsCallback) {')
    expect(addonSource).toMatch(/if \(!env \|\| !jsCallback\) \{\s*delete data;/s)

    const releasePersistentStart = addonSource.indexOf('void ReleasePersistentTsfns()')
    const releasePersistentEnd = addonSource.indexOf('void DestroyInternal()', releasePersistentStart)
    const releasePersistentSource = addonSource.slice(releasePersistentStart, releasePersistentEnd)
    expect(releasePersistentSource).toContain('readinessIngress_->Close()')
    expect(releasePersistentSource).toContain('writeWithoutResponseReadinessTsfn_ = Napi::ThreadSafeFunction()')
    expect(releasePersistentSource.indexOf('readinessIngress_->Close()')).toBeLessThan(
      releasePersistentSource.indexOf('writeWithoutResponseReadinessTsfn_ = Napi::ThreadSafeFunction()')
    )

    const destroyStart = addonSource.indexOf('Napi::Value Destroy(')
    const destroySource = addonSource.slice(destroyStart)
    expect(destroySource).not.toContain('CoreBluetoothAddon *self = this')
    expect(destroySource).not.toContain('self->ReleasePersistentTsfns()')
    expect(destroySource).toContain('ReleasePersistentTsfns()')
  })
})
