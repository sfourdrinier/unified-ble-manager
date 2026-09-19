'use strict'

// Finding 95: a desktop discovery registers the whole database or fails with
// the core's typed error, which reaches the public API with its code and domain unchanged
// (the public operation is the host's 4.x id; the core's rides the detail). Nothing is
// skipped and no partial snapshot becomes current. The failures come from
// the REAL N-API addon's core over its synthetic radio. The one the public
// path cannot provoke (an empty lease; the provider refuses before any core
// call) is the real core's answer to a withheld lease, carried through the
// same provider over the same addon.

const path = require('node:path')

const { BleError } = require('../../../src')
const { realBinding, withTimeout } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(60000)

const PEER = 'peer-1'
const SERVICE = '0000180f-0000-1000-8000-00805f9b34fb'
const LEVEL = '00002a19-0000-1000-8000-00805f9b34fb'
const readNotify = { read: true, write: false, writeWithoutResponse: false, notify: true, indicate: false }
/** The ATT handle space: at most 65535 attributes in one peer database. */
const ATT_HANDLE_SPACE = 65535

/** LEGACY-AUDIT-5 S5: the public id is the host's 4.x discover id; the core's stays in the core detail. */
const DISCOVER = { corebluetooth: 'direct-gatt.gatt.discover', bluez: 'bluez.gatt.discover', winrt: 'winrt.gatt.discover' }
const expectCoreOperation = (error, leg, coreOperation) => {
  expect(error.operation).toBe(DISCOVER[leg.platform])
  expect(error.platform).toMatchObject({ domain: 'desktop-rust-core', code: 'core-detail' })
  expect(error.platform.metadata.coreOperation).toContain(coreOperation)
}

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

/** The leg's public manager over the real addon, connected to a peer serving `services`. */
async function connectedLeg({ platform, os, module, factory }, services, bindingOverride = binding => binding) {
  const harness = realBinding(platform)
  const manager = await withPlatform(os, () =>
    require(path.join('..', '..', '..', 'src', module))[factory]({ binding: bindingOverride(harness.binding) })
  )
  const stage = harness.opened[harness.opened.length - 1]
  await stage.stageMtu(PEER, 185)
  await stage.stageServices(PEER, services)
  const scan = await manager.scan()
  const observations = scan.observations[Symbol.asyncIterator]()
  const observed = observations.next()
  await stage.stageAdvertisement({ peerId: PEER, rssi: -50, localName: 'Discovery failure peer' })
  const observation = await withTimeout(observed, 5000, 'advertisement')
  await observations.return?.()
  await scan.stop()
  if (observation.done || observation.value.kind !== 'value') {
    throw new Error(`the scan did not observe the peer: ${JSON.stringify(observation)}`)
  }
  const connection = await manager.connect(observation.value.value.peer)
  return { manager, connection }
}

async function discoveryFailure(connection) {
  return connection.discover().then(
    () => {
      throw new Error('discovery must fail as a whole')
    },
    error => error
  )
}

function oneService(characteristics) {
  return [{ uuid: SERVICE, occurrence: 0, characteristics }]
}

function levelCharacteristics(count) {
  return Array.from({ length: count }, (_, occurrence) => ({
    uuid: LEVEL,
    occurrence,
    properties: readNotify,
    descriptors: []
  }))
}

describe.each(LEGS)('$platform discovery fails whole, typed, through the public API', leg => {
  test('a malformed platform UUID is protocol.malformed at discovery.snapshot.uuid', async () => {
    const services = oneService([
      { uuid: LEVEL, occurrence: 0, properties: readNotify, descriptors: [] },
      { uuid: 'not-a-uuid', occurrence: 0, properties: readNotify, descriptors: [] }
    ])
    const { manager, connection } = await connectedLeg(leg, services)
    try {
      const error = await discoveryFailure(connection)
      expect(error).toBeInstanceOf(BleError)
      expect(error).toMatchObject({ code: 'protocol.malformed', domain: 'gatt' })
      expectCoreOperation(error, leg, 'discovery.snapshot.uuid')
    } finally {
      await manager.destroy()
    }
  })

  test('a database past the ATT handle space is capability.limited at discovery.database-bound', async () => {
    // One service plus 65535 characteristics: 65536 attributes.
    const { manager, connection } = await connectedLeg(leg, oneService(levelCharacteristics(ATT_HANDLE_SPACE)))
    try {
      const error = await discoveryFailure(connection)
      expect(error).toBeInstanceOf(BleError)
      expect(error).toMatchObject({ code: 'capability.limited', domain: 'gatt' })
      expectCoreOperation(error, leg, 'discovery.database-bound')
    } finally {
      await manager.destroy()
    }
  })

  test('a database exactly at the ATT handle space registers whole', async () => {
    // One service plus 65534 characteristics: 65535 attributes.
    const { manager, connection } = await connectedLeg(leg, oneService(levelCharacteristics(ATT_HANDLE_SPACE - 1)))
    try {
      const gatt = await connection.discover()
      expect(gatt.service(SERVICE).characteristicsByUuid(LEVEL)).toHaveLength(ATT_HANDLE_SPACE - 1)
    } finally {
      await manager.destroy()
    }
  })

  test("the core's argument.invalid at path.owner reaches the public API unchanged", async () => {
    const ownerRejection = binding => ({
      ...binding,
      openProduction: async options => {
        const central = await binding.openProduction(options)
        return new Proxy(central, {
          get(target, property) {
            // The core's own answer to a discovery with no lease: the
            // provider never sends one, so the lease is withheld here.
            if (property === 'discover') return options => target.discover({ ...options, lease: '' })
            const value = Reflect.get(target, property)
            return typeof value === 'function' ? (...args) => Reflect.apply(value, target, args) : value
          }
        })
      }
    })
    const { manager, connection } = await connectedLeg(leg, oneService(levelCharacteristics(1)), ownerRejection)
    try {
      const error = await discoveryFailure(connection)
      expect(error).toBeInstanceOf(BleError)
      expect(error).toMatchObject({ code: 'argument.invalid', domain: 'core' })
      expectCoreOperation(error, leg, 'path.owner')
    } finally {
      await manager.destroy()
    }
  })
})
