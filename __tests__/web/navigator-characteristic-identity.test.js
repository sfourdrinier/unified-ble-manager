// __tests__/web/navigator-characteristic-identity.test.js

const { NavigatorWebBluetoothBoundary } = require('../../src/web/navigator-web-bluetooth-boundary')

test('preserves exact browser characteristic identity across discovery and fences disconnect', async () => {
  const listeners = new Set()
  const first = { uuid: 'same', properties: {} }
  const second = { uuid: 'same', properties: {} }
  const service = { uuid: 'service', getCharacteristics: async () => [first, second] }
  const server = {
    connected: true,
    connect: async () => server,
    disconnect: () => { server.connected = false },
    getPrimaryServices: async () => [service]
  }
  const device = {
    id: 'device', gatt: server,
    addEventListener: (_type, listener) => listeners.add(listener),
    removeEventListener: (_type, listener) => listeners.delete(listener)
  }
  const boundary = new NavigatorWebBluetoothBoundary({
    implementationVersion: 'test', browserEngine: 'test',
    bluetooth: { requestDevice: async () => device, getDevices: async () => [device] },
    isSecureContext: () => true, hasTransientUserActivation: () => true,
    now: () => 0, setTimer: () => ({}), clearTimer: () => {},
    addPageLifecycleListener: () => () => {}
  })
  const selected = await boundary.requestDevice({ acceptAllDevices: true, optionalServices: [], filters: [] })
  const [authorized] = await boundary.getAuthorizedDevices()
  expect(authorized).toBe(selected.device)
  const listener = jest.fn()
  selected.device.addDisconnectListener(listener)
  const discover = async owner => (await (await owner.gatt.getPrimaryServices())[0].getCharacteristics())
  const original = await discover(selected.device)
  const rediscovered = await discover(authorized)
  expect(original[0]).toBe(rediscovered[0])
  expect(original[1]).toBe(rediscovered[1])
  expect(original[0]).not.toBe(original[1])
  for (const callback of listeners) callback()
  expect(listener).toHaveBeenCalledTimes(1)
  const reconnected = await discover(selected.device)
  expect(reconnected[0]).not.toBe(original[0])
  expect(reconnected[1]).not.toBe(original[1])
  selected.device.removeDisconnectListener(listener)
  expect(listeners.size).toBe(0)
})
