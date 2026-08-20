// __tests__/profiles/standard-commands.test.js

const {
  attachBleBackend,
  BleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager')
const { capacity, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { canonicalUuid, VirtualPeripheral } = require('../../src/testing')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const {
  readCharacteristic,
  writeCharacteristic,
  subscribeCharacteristic
} = require('../../src/profiles/commands')
const { bodySensorLocationSelector, heartRateMeasurementSelector } = require('../../src/profiles/heart-rate')
const { batteryLevelSelector } = require('../../src/profiles/battery-service')
const standard = require('../../src/profiles/standard-commands')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function operation(signal = null) {
  return { signal, deadline: null }
}

function subscriptionOptions() {
  return {
    ...operation(),
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(128),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    }
  }
}

async function settle(controller, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 20 && !settled; attempt += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

function sigPropertyFixture() {
  return new VirtualPeripheral({
    key: 'sig-property-fixture',
    services: [
      {
        uuid: canonicalUuid('180d'),
        occurrence: 0,
        primary: true,
        characteristics: [
          {
            uuid: canonicalUuid('2a37'),
            occurrence: 0,
            initialValue: new Uint8Array([0x06, 72]),
            readable: false,
            writableWithResponse: false,
            writableWithoutResponse: false,
            notifying: true,
            indicating: false,
            descriptors: []
          },
          {
            uuid: canonicalUuid('2a38'),
            occurrence: 0,
            initialValue: new Uint8Array([1]),
            readable: true,
            writableWithResponse: false,
            writableWithoutResponse: false,
            notifying: false,
            indicating: false,
            descriptors: []
          }
        ]
      },
      {
        uuid: canonicalUuid('180f'),
        occurrence: 0,
        primary: true,
        characteristics: [
          {
            uuid: canonicalUuid('2a19'),
            occurrence: 0,
            initialValue: new Uint8Array([80]),
            readable: true,
            writableWithResponse: false,
            writableWithoutResponse: false,
            notifying: false,
            indicating: false,
            descriptors: []
          }
        ]
      }
    ]
  })
}

async function createPropertyFixture() {
  const fixture = createDeterministicTestBackend({ peripheral: sigPropertyFixture() })
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const manager = await BleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('sig-client', 'client', 'deterministic:sig'),
      managerId: opaqueId('sig-manager', 'manager', 'deterministic:sig'),
      ownerMode: 'owning'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  const peerId = opaqueId('sig-peer', 'peer', 'deterministic')
  const connection = await settle(fixture.controller, manager.connect(peerId, operation()))
  const database = await settle(fixture.controller, connection.discover(operation()))
  return { fixture, manager, database }
}

describe('standard-commands SIG surface', () => {
  test('does not export invalid SIG reads', () => {
    expect(standard.readHeartRateMeasurement).toBeUndefined()
    expect(standard.readBloodPressureMeasurement).toBeUndefined()
    expect(standard.readTemperatureMeasurement).toBeUndefined()
  })

  test('keeps legal helpers', () => {
    expect(typeof standard.subscribeHeartRateMeasurements).toBe('function')
    expect(typeof standard.subscribeBloodPressureMeasurements).toBe('function')
    expect(typeof standard.subscribeTemperatureMeasurements).toBe('function')
    expect(typeof standard.resetHeartRateEnergyExpended).toBe('function')
    expect(typeof standard.readBatteryLevel).toBe('function')
    expect(typeof standard.readBodySensorLocation).toBe('function')
  })

  test('rejects read of a notify-only Heart Rate Measurement before the backend is asked', async () => {
    const { fixture, manager, database } = await createPropertyFixture()
    await expect(readCharacteristic(database, heartRateMeasurementSelector(), operation())).rejects.toMatchObject({
      normalized: { code: 'gatt.property-not-supported', operation: 'profiles.read-characteristic' }
    })
    await settle(fixture.controller, manager.destroy())
  })

  test('rejects write of read-only Battery Level before the backend is asked', async () => {
    const { fixture, manager, database } = await createPropertyFixture()
    await expect(
      writeCharacteristic(database, batteryLevelSelector(), new Uint8Array([1]), {
        ...operation(),
        mode: 'with-response'
      })
    ).rejects.toMatchObject({
      normalized: { code: 'gatt.property-not-supported', operation: 'profiles.write-characteristic' }
    })
    await settle(fixture.controller, manager.destroy())
  })

  test('rejects subscribe of a read-only Body Sensor Location before the backend is asked', async () => {
    const { fixture, manager, database } = await createPropertyFixture()
    await expect(
      subscribeCharacteristic(database, bodySensorLocationSelector(), subscriptionOptions())
    ).rejects.toMatchObject({
      normalized: { code: 'gatt.property-not-supported', operation: 'profiles.subscribe-characteristic' }
    })
    await settle(fixture.controller, manager.destroy())
  })
})
