// __tests__/backends/bluez/dbus-next-bluez-boundary.test.js

const { EventEmitter } = require('events')

const buses = []

class MockInterface {
  constructor(name) { this.$name = name }
  static configureMembers() {}
}
class MockDBusError extends Error {
  constructor(type, text) { super(text); this.type = type }
}
jest.mock('dbus-next', () => ({
  MessageType: { SIGNAL: 4 },
  Variant: class Variant {
    constructor(signature, value) {
      this.signature = signature
      this.value = value
    }
  },
  interface: { Interface: MockInterface, method: () => () => undefined },
  DBusError: MockDBusError,
  systemBus: jest.fn(() => buses.shift()),
  sessionBus: jest.fn(() => buses.shift())
}))

const dbus = require('dbus-next')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BLUEZ_OBJECT_MANAGER_INTERFACE,
  DBUS_PROPERTIES_INTERFACE
} = require('../../../src/backends/bluez/bluez-dbus-contract')
const { DbusNextBluezBoundaryFactory } = require('../../../src/backends/bluez/bluez-dbus-next-boundary')

function createBus(options = {}) {
  const emitter = new EventEmitter()
  const adapter = {
    SetDiscoveryFilter: jest.fn(async () => undefined),
    StartDiscovery: jest.fn(async () => undefined),
    StopDiscovery: jest.fn(async () => undefined),
    RemoveDevice: jest.fn(async () => undefined)
  }
  const device = {
    Connect: jest.fn(async () => undefined),
    Disconnect: jest.fn(async () => undefined),
    Pair: jest.fn(async () => undefined),
    CancelPairing: jest.fn(async () => undefined)
  }
  const manager = {
    GetManagedObjects: jest.fn(
      async () =>
        options.objects ?? {
          '/org/bluez/hci0': {
            [BLUEZ_ADAPTER_INTERFACE]: {
              Alias: { signature: 's', value: 'primary' },
              Powered: { signature: 'b', value: true }
            }
          }
        }
    )
  }
  const daemon = {
    AddMatch: jest.fn(async () => undefined),
    RemoveMatch: jest.fn(async () => undefined)
  }
  const agentManager = {
    RegisterAgent: jest.fn(async () => undefined),
    RequestDefaultAgent: jest.fn(async () => undefined)
  }
  emitter.export = jest.fn()
  emitter.unexport = jest.fn()
  emitter.getProxyObject = jest.fn(async (service, path) => ({
    getInterface: interfaceName => {
      if (service === 'org.freedesktop.DBus' && path === '/org/freedesktop/DBus') {
        return daemon
      }
      if (path === '/' && interfaceName === BLUEZ_OBJECT_MANAGER_INTERFACE) {
        return manager
      }
      if (path === '/org/bluez' && interfaceName === 'org.bluez.AgentManager1') {
        return agentManager
      }
      if (interfaceName === BLUEZ_DEVICE_INTERFACE) {
        return device
      }
      return adapter
    }
  }))
  emitter.disconnect = jest.fn()
  return { bus: emitter, manager, adapter, device, daemon, agentManager }
}

describe('dbus-next BlueZ boundary', () => {
  afterEach(() => {
    buses.length = 0
    jest.clearAllMocks()
  })

  it('dispatches Device1.Pair (regression: was rejected as unsupported)', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.methods.callVoid('/org/bluez/hci0/dev_AA', BLUEZ_DEVICE_INTERFACE, 'Pair', [])
    expect(fixture.device.Pair).toHaveBeenCalledTimes(1)
    await boundary.close()
  })

  it('dispatches Device1.CancelPairing', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.methods.callVoid('/org/bluez/hci0/dev_AA', BLUEZ_DEVICE_INTERFACE, 'CancelPairing', [])
    expect(fixture.device.CancelPairing).toHaveBeenCalledTimes(1)
    await boundary.close()
  })

  it('dispatches Adapter1.RemoveDevice with the object path argument', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.methods.callVoid('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, 'RemoveDevice', [
      { signature: 'o', value: '/org/bluez/hci0/dev_AA' }
    ])
    expect(fixture.adapter.RemoveDevice).toHaveBeenCalledWith('/org/bluez/hci0/dev_AA')
    await boundary.close()
  })

  it('registers a just-works pairing agent once (idempotent)', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.ensurePairingAgent()
    await boundary.ensurePairingAgent()
    expect(fixture.bus.export).toHaveBeenCalledTimes(1)
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledTimes(1)
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledWith(expect.any(String), 'NoInputNoOutput')
    // A default agent is deliberately NOT requested (no system-wide hijack).
    expect(fixture.agentManager.RequestDefaultAgent).not.toHaveBeenCalled()
    await boundary.close()
    expect(fixture.bus.unexport).toHaveBeenCalledTimes(1)
  })

  it('runs concurrent ensurePairingAgent calls without a duplicate export', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await Promise.all([boundary.ensurePairingAgent(), boundary.ensurePairingAgent()])
    expect(fixture.bus.export).toHaveBeenCalledTimes(1)
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledTimes(1)
    await boundary.close()
  })

  it('tolerates an AlreadyExists RegisterAgent error (by type, not message)', async () => {
    const fixture = createBus()
    fixture.agentManager.RegisterAgent.mockRejectedValueOnce(
      new MockDBusError('org.bluez.Error.AlreadyExists', 'Already Exists')
    )
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await expect(boundary.ensurePairingAgent()).resolves.toBeUndefined()
    await boundary.close()
  })

  it('retries registration after a failure rather than wedging', async () => {
    const fixture = createBus()
    fixture.agentManager.RegisterAgent
      .mockRejectedValueOnce(new Error('transient'))
      .mockResolvedValueOnce(undefined)
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await expect(boundary.ensurePairingAgent()).rejects.toThrow('transient')
    await expect(boundary.ensurePairingAgent()).resolves.toBeUndefined()
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledTimes(2)
    await boundary.close()
  })

  it('auto-accepts just-works pairing and rejects input-requiring ceremonies', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.ensurePairingAgent()
    expect(fixture.bus.export).toHaveBeenCalledTimes(1)
    const [exportedPath, agent] = fixture.bus.export.mock.calls[0]
    expect(exportedPath).toBe('/org/bluez/unifiedble/agent')

    const device = '/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF'
    // Just-works association: confirmation, authorization, and service
    // authorization are auto-accepted; lifecycle callbacks are no-ops.
    expect(() => agent.RequestConfirmation(device, 0)).not.toThrow()
    expect(() => agent.RequestAuthorization(device)).not.toThrow()
    expect(() => agent.AuthorizeService(device, '0000180d-0000-1000-8000-00805f9b34fb')).not.toThrow()
    expect(() => agent.Release()).not.toThrow()
    expect(() => agent.Cancel()).not.toThrow()

    // NoInputNoOutput cannot satisfy PIN or passkey entry, so those ceremonies
    // must be rejected with the D-Bus error BlueZ expects.
    let pinError
    try {
      agent.RequestPinCode(device)
    } catch (error) {
      pinError = error
    }
    expect(pinError).toBeInstanceOf(dbus.DBusError)
    expect(pinError.type).toBe('org.bluez.Error.Rejected')

    let passkeyError
    try {
      agent.RequestPasskey(device)
    } catch (error) {
      passkeyError = error
    }
    expect(passkeyError).toBeInstanceOf(dbus.DBusError)
    expect(passkeyError.type).toBe('org.bluez.Error.Rejected')

    await boundary.close()
  })

  it('re-registers the pairing agent after a daemon reset', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.ensurePairingAgent()
    expect(fixture.bus.export).toHaveBeenCalledTimes(1)
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledTimes(1)

    // A bluetoothd restart drops every registration; the boundary observes the
    // owner loss as a reset and must forget the agent so the next pair rebuilds it.
    fixture.bus.emit('message', {
      type: 4,
      path: '/org/freedesktop/DBus',
      interface: 'org.freedesktop.DBus',
      member: 'NameOwnerChanged',
      body: ['org.bluez', ':1.42', '']
    })
    expect(fixture.bus.unexport).toHaveBeenCalledTimes(1)

    await boundary.ensurePairingAgent()
    expect(fixture.bus.export).toHaveBeenCalledTimes(2)
    expect(fixture.agentManager.RegisterAgent).toHaveBeenCalledTimes(2)
    await boundary.close()
  })

  it('does not export a pairing agent once the boundary is closed', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    await boundary.close()
    await expect(boundary.ensurePairingAgent()).resolves.toBeUndefined()
    expect(fixture.bus.export).not.toHaveBeenCalled()
    expect(fixture.agentManager.RegisterAgent).not.toHaveBeenCalled()
  })

  test.each([
    ['system', 'systemBus'],
    ['session', 'sessionBus']
  ])('opens the explicit %s bus and closes its signal listener exactly once', async (busKind, factoryMethod) => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open(busKind)
    const added = jest.fn()
    const subscription = boundary.objectManager.onInterfacesAdded(added)

    fixture.bus.emit('message', {
      type: 4,
      path: '/',
      interface: BLUEZ_OBJECT_MANAGER_INTERFACE,
      member: 'InterfacesAdded',
      body: ['/org/bluez/hci1', { [BLUEZ_ADAPTER_INTERFACE]: { Powered: { signature: 'b', value: true } } }]
    })
    expect(added).toHaveBeenCalledWith(expect.objectContaining({ ordinal: 1, path: '/org/bluez/hci1' }))

    subscription.remove()
    subscription.remove()
    await boundary.close()
    await boundary.close()
    fixture.bus.emit('message', {
      type: 4,
      path: '/org/bluez/hci1',
      interface: DBUS_PROPERTIES_INTERFACE,
      member: 'PropertiesChanged',
      body: [BLUEZ_ADAPTER_INTERFACE, { Powered: { signature: 'b', value: false } }, []]
    })

    expect(dbus[factoryMethod]).toHaveBeenCalledTimes(1)
    expect(fixture.bus.listenerCount('message')).toBe(0)
    expect(fixture.bus.listenerCount('error')).toBe(0)
    expect(fixture.bus.disconnect).toHaveBeenCalledTimes(1)
    expect(fixture.daemon.AddMatch).toHaveBeenCalledTimes(3)
    expect(fixture.daemon.RemoveMatch).toHaveBeenCalledTimes(3)
    expect(added).toHaveBeenCalledTimes(1)
  })

  test('observes BlueZ daemon owner loss and D-Bus connection errors as runtime reset signals', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    const reset = jest.fn()
    boundary.onReset(reset)

    fixture.bus.emit('message', {
      type: 4,
      path: '/org/freedesktop/DBus',
      interface: 'org.freedesktop.DBus',
      member: 'NameOwnerChanged',
      body: ['org.bluez', ':1.42', '']
    })
    const busError = new Error('socket lost')
    fixture.bus.emit('error', busError)

    expect(reset).toHaveBeenCalledTimes(1)
    expect(reset).toHaveBeenCalledWith('BlueZ D-Bus service owner disappeared')
    expectConsoleError('[DbusNextBluezBoundary.handleBusError] D-Bus connection failed:', busError)
    fixture.bus.emit('message', {
      type: 4,
      path: '/org/freedesktop/DBus',
      interface: 'org.freedesktop.DBus',
      member: 'NameOwnerChanged',
      body: ['org.bluez', '', ':1.43']
    })
    fixture.bus.emit('message', {
      type: 4,
      path: '/org/freedesktop/DBus',
      interface: 'org.freedesktop.DBus',
      member: 'NameOwnerChanged',
      body: ['org.bluez', ':1.43', '']
    })
    expect(reset).toHaveBeenCalledTimes(2)
    await boundary.close()
  })

  test('retains failed match-rule cleanup for an exact retry before disconnecting', async () => {
    const fixture = createBus()
    const cleanupError = new Error('remove failed')
    fixture.daemon.RemoveMatch.mockRejectedValueOnce(cleanupError)
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')

    await expect(boundary.close()).rejects.toThrow('Failed to remove one or more BlueZ D-Bus match rules')
    expectConsoleErrorMatching(
      '[DbusNextBluezBoundary.close] Failed to remove D-Bus match rule:',
      expect.objectContaining({ detail: expect.objectContaining({ name: 'Error', message: 'remove failed' }) })
    )
    expect(fixture.bus.disconnect).not.toHaveBeenCalled()
    await expect(boundary.close()).resolves.toBeUndefined()
    expect(fixture.bus.disconnect).toHaveBeenCalledTimes(1)
    expect(fixture.daemon.RemoveMatch).toHaveBeenCalledTimes(4)
  })

  test('decodes a sorted owned snapshot and encodes discovery filter variants', async () => {
    const fixture = createBus({
      objects: {
        '/org/bluez/hci1': {
          [BLUEZ_ADAPTER_INTERFACE]: { Alias: { signature: 's', value: 'second' } }
        },
        '/org/bluez/hci0': {
          [BLUEZ_ADAPTER_INTERFACE]: { Alias: { signature: 's', value: 'first' } }
        }
      }
    })
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')

    const snapshot = await boundary.objectManager.getManagedObjects()
    expect(snapshot.map(object => object.path)).toEqual(['/org/bluez/hci0', '/org/bluez/hci1'])
    await boundary.methods.callVoid('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, 'SetDiscoveryFilter', [
      {
        signature: 'a{sv}',
        value: {
          DuplicateData: { signature: 'b', value: true },
          UUIDs: { signature: 'as', value: ['180d'] }
        }
      }
    ])

    expect(fixture.adapter.SetDiscoveryFilter).toHaveBeenCalledWith({
      DuplicateData: expect.objectContaining({ signature: 'b', value: true }),
      UUIDs: expect.objectContaining({ signature: 'as', value: ['180d'] })
    })
    await boundary.close()
  })

  test('normalizes a D-Bus method error without exposing an arbitrary error object', async () => {
    const fixture = createBus()
    const platformError = new Error('not ready')
    Object.defineProperty(platformError, 'type', { value: 'org.bluez.Error.NotReady' })
    fixture.adapter.StartDiscovery.mockRejectedValue(platformError)
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')

    await expect(
      boundary.methods.callVoid('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, 'StartDiscovery', [])
    ).rejects.toMatchObject({
      name: 'BluezDbusMethodError',
      detail: {
        name: 'org.bluez.Error.NotReady',
        message: 'not ready',
        safeDetails: {}
      }
    })
    await boundary.close()
  })

  test("decodes modern BlueZ 'y' and 'a{qv}' property variants", async () => {
    const fixture = createBus({
      objects: {
        '/org/bluez/hci0': {
          [BLUEZ_ADAPTER_INTERFACE]: {
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true },
            Version: { signature: 'y', value: 13 }
          }
        },
        '/org/bluez/hci0/dev_98_75_96_A2_14_34': {
          'org.bluez.Device1': {
            Address: { signature: 's', value: '98:75:96:A2:14:34' },
            ManufacturerData: {
              signature: 'a{qv}',
              value: { 76: { signature: 'ay', value: new Uint8Array([2, 21]) } }
            }
          }
        }
      }
    })
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    const snapshot = await boundary.objectManager.getManagedObjects()
    const adapter = snapshot.find(object => object.path === '/org/bluez/hci0')
    const adapterProperties = adapter.interfaces.find(entry => entry.name === BLUEZ_ADAPTER_INTERFACE).properties
    expect(adapterProperties.Version).toEqual({ signature: 'y', value: 13 })
    const device = snapshot.find(object => object.path === '/org/bluez/hci0/dev_98_75_96_A2_14_34')
    const deviceProperties = device.interfaces.find(entry => entry.name === 'org.bluez.Device1').properties
    expect(deviceProperties.ManufacturerData.signature).toBe('a{sv}')
    expect(deviceProperties.ManufacturerData.value['76']).toEqual({ signature: 'ay', value: new Uint8Array([2, 21]) })
    await boundary.close()
  })

  test('surfaces UnknownMethod for ConnectDevice on a daemon without it and calls it when exported', async () => {
    const fixture = createBus()
    buses.push(fixture.bus)
    const boundary = await new DbusNextBluezBoundaryFactory().open('system')
    const properties = {
      signature: 'a{sv}',
      value: {
        Address: { signature: 's', value: '98:75:96:A2:14:34' },
        AddressType: { signature: 's', value: 'public' }
      }
    }
    await expect(
      boundary.methods.callVoid('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', [properties])
    ).rejects.toMatchObject({ detail: { name: 'org.freedesktop.DBus.Error.UnknownMethod' } })

    fixture.adapter.ConnectDevice = jest.fn(async () => undefined)
    await boundary.methods.callVoid('/org/bluez/hci0', BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', [properties])
    expect(fixture.adapter.ConnectDevice).toHaveBeenCalledWith(
      expect.objectContaining({
        Address: expect.objectContaining({ signature: 's', value: '98:75:96:A2:14:34' }),
        AddressType: expect.objectContaining({ signature: 's', value: 'public' })
      })
    )
    await boundary.close()
  })
})
