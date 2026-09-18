'use strict'

// Finding 99b: the legacy duplicate-UUID public-path tests
// (bluez-public-gatt-occurrence.test.js and the CoreBluetooth database
// duplicate tests on 4.x) ported to the Rust desktop route. Every leg runs the
// public Node factory over the REAL N-API addon on its synthetic radio: the
// radio serves a database with a second service UUID and a same-UUID
// occurrence 1 at the service, characteristic and descriptor levels, in an
// order that is not UUID order. Discovery must keep that order and number
// each repeated UUID under its parent (docs/UNIFIED_SEMANTICS.md §9), and
// every read, write, descriptor access and notification must reach exactly
// the instance its complete path names.

const path = require('node:path')

const { realBinding, withTimeout } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const BATTERY = '0000180f-0000-1000-8000-00805f9b34fb'
const HEART_RATE = '0000180d-0000-1000-8000-00805f9b34fb'
const LEVEL = '00002a19-0000-1000-8000-00805f9b34fb'
const MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb'
const PEER = 'peer-1'

const readWriteNotify = { read: true, write: true, writeWithoutResponse: false, notify: true, indicate: false }

/** Discovery order on purpose: 180f, 180d, 180f — not UUID order. */
const WORLD = [
  {
    uuid: BATTERY,
    occurrence: 0,
    characteristics: [
      {
        uuid: LEVEL,
        occurrence: 0,
        properties: readWriteNotify,
        descriptors: [
          { uuid: USER_DESCRIPTION, occurrence: 0 },
          { uuid: USER_DESCRIPTION, occurrence: 1 }
        ]
      },
      { uuid: LEVEL, occurrence: 1, properties: readWriteNotify, descriptors: [] }
    ]
  },
  {
    uuid: HEART_RATE,
    occurrence: 0,
    characteristics: [{ uuid: MEASUREMENT, occurrence: 0, properties: readWriteNotify, descriptors: [] }]
  },
  {
    uuid: BATTERY,
    occurrence: 1,
    characteristics: [{ uuid: LEVEL, occurrence: 0, properties: readWriteNotify, descriptors: [] }]
  }
]

const LEGS = [
  { platform: 'corebluetooth', os: 'darwin', module: 'node-corebluetooth', factory: 'createCoreBluetoothBleManager' },
  { platform: 'bluez', os: 'linux', module: 'node-bluez', factory: 'createBluezBleManager' },
  { platform: 'winrt', os: 'win32', module: 'node-winrt', factory: 'createWinRtBleManager' }
]

function withPlatform(platform, run) {
  const original = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: platform })
  const restore = () => Object.defineProperty(process, 'platform', original)
  let result
  try {
    result = run()
  } catch (error) {
    restore()
    throw error
  }
  if (result && typeof result.then === 'function') return result.finally(restore)
  restore()
  return result
}

function instance(serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence) {
  return { peerId: PEER, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence }
}

/** Opens the leg's public manager over the real addon and connects to the duplicate-UUID peer. */
async function connectedLeg({ platform, os, module, factory }) {
  const harness = realBinding(platform)
  const manager = await withPlatform(os, () =>
    require(path.join('..', '..', '..', 'src', module))[factory]({ binding: harness.binding })
  )
  const stage = harness.opened[harness.opened.length - 1]
  await stage.stageMtu(PEER, 185)
  await stage.stageServices(PEER, WORLD)
  await stage.stageCharacteristicValue(instance(BATTERY, 0, LEVEL, 0), Buffer.from([0x10]))
  await stage.stageCharacteristicValue(instance(BATTERY, 0, LEVEL, 1), Buffer.from([0x11]))
  await stage.stageCharacteristicValue(instance(BATTERY, 1, LEVEL, 0), Buffer.from([0x20]))
  const scan = await manager.scan()
  const observations = scan.observations[Symbol.asyncIterator]()
  const observed = observations.next()
  await stage.stageAdvertisement({ peerId: PEER, rssi: -50, localName: 'Duplicate UUID peer' })
  const observation = await withTimeout(observed, 5000, 'advertisement')
  await observations.return?.()
  await scan.stop()
  if (observation.done || observation.value.kind !== 'value') {
    throw new Error(`the scan did not observe the duplicate-UUID peer: ${JSON.stringify(observation)}`)
  }
  const connection = await manager.connect(observation.value.value.peer)
  const gatt = await connection.discover()
  return { manager, stage, connection, gatt }
}

function gattAccesses(accesses, kind) {
  return accesses
    .filter(access => access.kind === kind)
    .map(access => [
      access.serviceUuid,
      access.serviceOccurrence,
      access.characteristicUuid,
      access.characteristicOccurrence,
      access.descriptorUuid ?? null,
      access.descriptorOccurrence ?? null
    ])
}

async function nextNotification(subscription) {
  const item = await withTimeout(subscription.values[Symbol.asyncIterator]().next(), 5000, 'notification')
  return item.done || item.value.kind !== 'value' ? item : [...item.value.value.value]
}

describe.each(LEGS)('$platform duplicate-UUID occurrences over the real addon', leg => {
  test('discovery keeps discovery order and numbers each repeated UUID under its parent', async () => {
    const { manager, gatt } = await connectedLeg(leg)
    try {
      expect(gatt.services.map(service => [service.uuid, service.occurrence])).toEqual([
        [BATTERY, 0],
        [HEART_RATE, 0],
        [BATTERY, 1]
      ])
      expect(gatt.servicesByUuid(BATTERY).map(service => service.occurrence)).toEqual([0, 1])
      expect(() => gatt.service(BATTERY)).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))
      const first = gatt.service(BATTERY, { occurrence: 0 })
      expect(first.characteristicsByUuid(LEVEL).map(characteristic => characteristic.occurrence)).toEqual([0, 1])
      expect(() => first.characteristic(LEVEL)).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))
      const level = first.characteristic(LEVEL, { occurrence: 0 })
      expect(level.descriptors.map(descriptor => [descriptor.uuid, descriptor.occurrence])).toEqual([
        [USER_DESCRIPTION, 0],
        [USER_DESCRIPTION, 1]
      ])
      expect(() => level.descriptor(USER_DESCRIPTION)).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))
      const second = gatt.service(BATTERY, { occurrence: 1 })
      expect(second.characteristics.map(characteristic => [characteristic.uuid, characteristic.occurrence])).toEqual([
        [LEVEL, 0]
      ])
      expect(gatt.service(HEART_RATE).characteristic(MEASUREMENT).occurrence).toBe(0)
    } finally {
      await manager.destroy()
    }
  })

  test('reads, writes and descriptor accesses reach exactly the addressed instance', async () => {
    const { manager, stage, gatt } = await connectedLeg(leg)
    try {
      const first = gatt.service(BATTERY, { occurrence: 0 })
      const second = gatt.service(BATTERY, { occurrence: 1 })
      await expect(first.characteristic(LEVEL, { occurrence: 0 }).read()).resolves.toEqual(new Uint8Array([0x10]))
      await expect(first.characteristic(LEVEL, { occurrence: 1 }).read()).resolves.toEqual(new Uint8Array([0x11]))
      await expect(second.characteristic(LEVEL, { occurrence: 0 }).read()).resolves.toEqual(new Uint8Array([0x20]))

      await second.characteristic(LEVEL, { occurrence: 0 }).write(new Uint8Array([1]), { response: 'required' })
      await first.characteristic(LEVEL, { occurrence: 1 }).write(new Uint8Array([2]), { response: 'required' })
      const level = first.characteristic(LEVEL, { occurrence: 0 })
      await level.descriptor(USER_DESCRIPTION, { occurrence: 1 }).read()
      await level.descriptor(USER_DESCRIPTION, { occurrence: 0 }).write(new Uint8Array([3]), { response: 'required' })

      const accesses = await stage.stagedGattAccesses()
      expect(gattAccesses(accesses, 'write-with-response')).toEqual([
        [BATTERY, 1, LEVEL, 0, null, null],
        [BATTERY, 0, LEVEL, 1, null, null]
      ])
      expect(gattAccesses(accesses, 'descriptor-read')).toEqual([[BATTERY, 0, LEVEL, 0, USER_DESCRIPTION, 1]])
      expect(gattAccesses(accesses, 'descriptor-write')).toEqual([[BATTERY, 0, LEVEL, 0, USER_DESCRIPTION, 0]])
    } finally {
      await manager.destroy()
    }
  })

  test('a notification reaches only the subscription on the instance it was emitted on', async () => {
    const { manager, stage, gatt } = await connectedLeg(leg)
    try {
      const sibling = await gatt
        .service(BATTERY, { occurrence: 0 })
        .characteristic(LEVEL, { occurrence: 1 })
        .subscribe()
      const otherService = await gatt
        .service(BATTERY, { occurrence: 1 })
        .characteristic(LEVEL, { occurrence: 0 })
        .subscribe()
      const original = await gatt
        .service(BATTERY, { occurrence: 0 })
        .characteristic(LEVEL, { occurrence: 0 })
        .subscribe()

      await stage.stageNotification({ ...instance(BATTERY, 0, LEVEL, 1), value: Buffer.from([0xa1]) })
      await stage.stageNotification({ ...instance(BATTERY, 1, LEVEL, 0), value: Buffer.from([0xb0]) })
      await stage.stageNotification({ ...instance(BATTERY, 0, LEVEL, 0), value: Buffer.from([0xa0]) })

      // A value misrouted by UUID would arrive first on a sibling.
      await expect(nextNotification(sibling)).resolves.toEqual([0xa1])
      await expect(nextNotification(otherService)).resolves.toEqual([0xb0])
      await expect(nextNotification(original)).resolves.toEqual([0xa0])
      for (const subscription of [sibling, otherService, original]) {
        await expect(subscription.remove()).resolves.toMatchObject({ state: 'released' })
      }
    } finally {
      await manager.destroy()
    }
  })
})
