// __tests__/web/web-bluetooth-descriptor-absence.test.js
//
// Finding 188 (Web physical, Chrome 152 macOS, Polar H10 through the
// chooser): after connect, discovery failed with gatt.not-found at
// web-gatt.discover-descriptors because Web Bluetooth's getDescriptors()
// rejects with NotFoundError when a characteristic has no descriptors.
// That must become an empty descriptor list; any other error stays an
// error. After the chooser, the peer name was reported null: the backend
// must take BluetoothDevice.name when the browser provides it.

const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')

const HEART_RATE_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HEART_RATE_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'

function namedError(name, message) {
  const error = new Error(message)
  error.name = name
  return error
}

function createH10LikeBoundary({ descriptorsError = null, deviceName = 'Polar H10 E997042F' } = {}) {
  const disconnectListeners = new Set()
  const characteristic = {
    uuid: HEART_RATE_MEASUREMENT,
    properties: {
      read: true,
      write: true,
      writeWithoutResponse: true,
      notify: true,
      indicate: false
    },
    getDescriptors: async () => {
      if (descriptorsError !== null) throw descriptorsError
      return []
    },
    readValue: async () => new Uint8Array([0, 72]),
    writeValueWithResponse: async () => undefined,
    writeValueWithoutResponse: async () => undefined,
    startNotifications: async () => undefined,
    stopNotifications: async () => undefined,
    addNotificationListener: () => undefined,
    removeNotificationListener: () => undefined
  }
  const service = {
    uuid: HEART_RATE_SERVICE,
    getCharacteristics: async () => [characteristic]
  }
  const gatt = {
    connected: false,
    connect: async () => {
      gatt.connected = true
    },
    disconnect: () => {
      gatt.connected = false
      for (const listener of disconnectListeners) listener()
    },
    getPrimaryServices: async () => [service]
  }
  const device = {
    id: 'browser-owned-h10-identifier',
    ...(deviceName === null ? {} : { name: deviceName }),
    gatt,
    addDisconnectListener: listener => disconnectListeners.add(listener),
    removeDisconnectListener: listener => disconnectListeners.delete(listener)
  }
  const boundary = {
    implementationVersion: 'finding-188-test',
    browserEngine: 'test-browser',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => true,
    requestDevice: async () => ({ device, grantedServices: [HEART_RATE_SERVICE] }),
    now: () => 10,
    setTimer: () => ({ callback: () => undefined }),
    clearTimer: () => undefined,
    addPageLifecycleListener: () => () => undefined
  }
  return { boundary, device, characteristic }
}

async function attachedBackend(boundary) {
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  return backend
}

function chooserRequest() {
  return {
    filters: [{ serviceUuids: [HEART_RATE_SERVICE], manufacturerData: [], localNamePrefix: null }],
    acceptAllDevices: false,
    optionalServices: [HEART_RATE_SERVICE]
  }
}

function noDeadline() {
  return { signal: null, deadline: null }
}

describe('finding 188: descriptor-less characteristics discover with an empty descriptor list', () => {
  test('NotFoundError from getDescriptors becomes an empty descriptor list', async () => {
    const { boundary } = createH10LikeBoundary({
      descriptorsError: namedError('NotFoundError', 'No Descriptors found in characteristic.')
    })
    const backend = await attachedBackend(boundary)
    const selected = await backend.choose(chooserRequest(), noDeadline())
    const lease = await backend.connections.connect(selected.peerId, 'finding-188-client', noDeadline())
    const database = await backend.gatt.discover(lease.connection, noDeadline())
    const snapshot = await database.snapshot()
    expect(snapshot.characteristics).toHaveLength(1)
    expect(snapshot.descriptors).toHaveLength(0)
    await lease.release()
    await backend.destroy()
  })

  test('a non-absence error from getDescriptors still fails discovery', async () => {
    const { boundary } = createH10LikeBoundary({
      descriptorsError: namedError('NetworkError', 'GATT Server is disconnected.')
    })
    const backend = await attachedBackend(boundary)
    const selected = await backend.choose(chooserRequest(), noDeadline())
    const lease = await backend.connections.connect(selected.peerId, 'finding-188-client', noDeadline())
    await expect(backend.gatt.discover(lease.connection, noDeadline())).rejects.toMatchObject({
      normalized: { code: 'connection.lost' }
    })
    await lease.release()
    await backend.destroy()
  })
})

describe('finding 188: the chooser peer carries the browser device name', () => {
  test('BluetoothDevice.name reaches the authorized peer record', async () => {
    const { boundary } = createH10LikeBoundary({ deviceName: 'Polar H10 E997042F' })
    boundary.getAuthorizedDevices = async () => [(await boundary.requestDevice()).device]
    const backend = await attachedBackend(boundary)
    const records = await backend.peers.authorized(noDeadline())
    expect(records).toHaveLength(1)
    expect(records[0].name).toBe('Polar H10 E997042F')
    await backend.destroy()
  })

  test('a browser that withholds the name reports the peer name absent', async () => {
    const { boundary } = createH10LikeBoundary({ deviceName: null })
    boundary.getAuthorizedDevices = async () => [(await boundary.requestDevice()).device]
    const backend = await attachedBackend(boundary)
    const records = await backend.peers.authorized(noDeadline())
    expect(records).toHaveLength(1)
    expect(records[0].name).toBeNull()
    await backend.destroy()
  })

  test('the public choose peer carries the browser device name', async () => {
    const { createWebBleManagerWithEnvironment } = require('../../src/web')
    const rawGatt = {
      connected: false,
      connect: async () => undefined,
      disconnect: () => undefined,
      getPrimaryServices: async () => []
    }
    const requestDevice = jest.fn(async () => ({
      id: 'raw-h10',
      name: 'Polar H10 E997042F',
      gatt: rawGatt,
      addEventListener: () => undefined,
      removeEventListener: () => undefined
    }))
    const manager = await createWebBleManagerWithEnvironment({
      environment: {
        implementationVersion: 'finding-188-public',
        browserEngine: 'test',
        bluetooth: {
          getAvailability: async () => true,
          requestDevice
        },
        isSecureContext: () => true,
        hasTransientUserActivation: () => true,
        now: () => 10,
        setTimer: callback => ({ callback }),
        clearTimer: () => undefined,
        addPageLifecycleListener: () => () => undefined
      }
    })
    const peer = await manager.choose({
      filters: [{ serviceUuids: [HEART_RATE_SERVICE] }],
      optionalServices: [HEART_RATE_SERVICE],
      acceptAllDevices: false
    })
    expect(peer.name).toBe('Polar H10 E997042F')
    await manager.destroy()
  })
})
