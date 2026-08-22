// fixtures/g6a-packed-consumer/web-heart-rate-protocol.mjs

import assert from 'node:assert/strict'
import { createWebBleManager, createWebBluetoothProvider } from 'unified-ble-manager/web'
import {
  encodeResetEnergyExpended,
  HEART_RATE_CONTROL_POINT_CHARACTERISTIC,
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import { strictNumericCounters } from './resource-counters.mjs'

const operationOptions = Object.freeze({ signal: null, deadline: null })
const notificationOptions = Object.freeze({
  ...operationOptions,
  delivery: 'prefer-notification',
  stream: {
    preset: 'custom',
    budget: {
      itemCapacity: 4,
      byteCapacity: 128,
      reservedControlCapacity: 1,
      overflowPolicy: 'drop-oldest'
    }
  }
})

export async function runWebHeartRateProtocol() {
  const { boundary, controls } = createBoundary()
  const provider = createWebBluetoothProvider(boundary)
  const session = await createWebBleManager({
    provider,
    clientId: 'g6a-packed-web-client',
    managerId: 'g6a-packed-web-manager',
    now: boundary.now
  })
  let connection = null
  let subscription = null
  let primaryError = null
  const cleanupErrors = []
  try {
    const selection = await session.chooser.choose(
      {
        filters: [{ serviceUuids: [HEART_RATE_SERVICE], manufacturerData: [], localNamePrefix: 'G6A' }],
        acceptAllDevices: false,
        optionalServices: [HEART_RATE_SERVICE]
      },
      operationOptions
    )
    connection = await session.manager.connect(selection.peerId, operationOptions)
    const database = await connection.discover(operationOptions)
    const measurementCharacteristic = database.characteristic(
      HEART_RATE_SERVICE,
      HEART_RATE_MEASUREMENT_CHARACTERISTIC
    )
    const controlPointCharacteristic = database.characteristic(
      HEART_RATE_SERVICE,
      HEART_RATE_CONTROL_POINT_CHARACTERISTIC
    )
    subscription = await measurementCharacteristic.subscribe(notificationOptions)
    const measurementPromise = subscription.values[Symbol.asyncIterator]().next()
    controls.emitNotification(new Uint8Array([0x06, 72]))
    const measurementItem = await measurementPromise
    assert.equal(measurementItem.done, false, 'packed Web Heart Rate subscription remained open')
    assert.equal(measurementItem.value.kind, 'value', 'packed Web Heart Rate subscription delivered a value')
    assert.equal(
      parseHeartRateMeasurement(measurementItem.value.value.value).beatsPerMinute,
      72,
      'packed Web Heart Rate profile decoded the chooser-selected notification'
    )

    const write = await controlPointCharacteristic.write(encodeResetEnergyExpended(), {
      ...operationOptions,
      response: 'required'
    })
    assert.equal(write.commitState, 'confirmed', 'packed Web Heart Rate command write committed')
    assert.deepEqual(controls.writes, [[1]], 'packed Web Heart Rate profile emitted the reset command')

    const notificationPromise = subscription.values[Symbol.asyncIterator]().next()
    controls.emitNotification(new Uint8Array([0x06, 76]))
    const notification = await notificationPromise
    assert.equal(notification.done, false, 'packed Web Heart Rate subscription remained open')
    assert.equal(notification.value.kind, 'value', 'packed Web Heart Rate subscription delivered a value')
    assert.equal(
      parseHeartRateMeasurement(notification.value.value.value).beatsPerMinute,
      76,
      'packed Web Heart Rate profile decoded the browser notification'
    )

    controls.holdNextSubscribe()
    const abort = new AbortController()
    const cancelledSubscribe = measurementCharacteristic.subscribe({
      ...notificationOptions,
      signal: abort.signal
    })
    await flushMicrotasks()
    abort.abort()
    await assertAborted(cancelledSubscribe, 'subscribe.cancellation')
    controls.resolvePendingSubscribe()
    await flushMicrotasks()

    assertCleanup(await subscription.remove(), 'packed Web subscription cleanup')
    subscription = null
    assertCleanup(await connection.release(), 'packed Web connection cleanup')
    connection = null
  } catch (error) {
    primaryError = error
  }

  if (subscription !== null) {
    await cleanupStep(cleanupErrors, () => subscription.remove(), 'packed Web subscription cleanup')
  }
  if (connection !== null) {
    await cleanupStep(cleanupErrors, () => connection.release(), 'packed Web connection cleanup')
  }
  await cleanupStep(cleanupErrors, () => session.manager.destroy(), 'packed Web manager cleanup')

  if (primaryError !== null && cleanupErrors.length > 0) {
    throw new AggregateError([primaryError, ...cleanupErrors], 'packed Web vendor protocol and cleanup failed')
  }
  if (primaryError !== null) {
    throw primaryError
  }
  if (cleanupErrors.length > 0) {
    throw new AggregateError(cleanupErrors, 'packed Web vendor protocol cleanup failed')
  }

  const counters = strictNumericCounters(session.manager.localResourceCounters(), 'Web')
  assert.deepEqual(Object.values(counters), new Array(Object.keys(counters).length).fill(0), 'packed Web resources released')
  return {
    schema: 'unified-ble-g6a-host-proof-v1',
    host: {
      id: 'web',
      family: 'web',
      runtime: 'node-hosted-web-bluetooth-boundary',
      moduleSystem: 'esm',
      backendHostKind: session.manager.identity.runtime.hostKind,
      browserEngine: session.manager.identity.runtime.diagnostics.browserEngine,
      liveBrowserEngine: false
    },
    packageContract: 'public-web-manager-and-profile-subpaths',
    vendorProtocol: {
      profile: 'bluetooth-sig-heart-rate',
      chooser: 'passed',
      connect: 'passed',
      discovery: 'passed',
      commandWrite: 'passed',
      responseRead: 'passed',
      notification: 'passed',
      cancellation: 'passed',
      cleanup: 'passed'
    },
    resourceCounters: counters,
    evidence: {
      proofScope: 'deterministic',
      artifactSource: 'packed-tarball',
      physicalRadio: 'hardware-only'
    }
  }
}

function createBoundary() {
  const serviceUuid = String(HEART_RATE_SERVICE)
  const measurementUuid = String(HEART_RATE_MEASUREMENT_CHARACTERISTIC)
  const controlPointUuid = String(HEART_RATE_CONTROL_POINT_CHARACTERISTIC)
  let measurement = new Uint8Array([0x06, 72])
  let holdSubscribe = false
  let pendingSubscribeResolve = null
  let connected = false
  const notificationListeners = new Set()
  const disconnectListeners = new Set()
  const writes = []

  const measurementCharacteristic = {
    uuid: measurementUuid,
    properties: { read: false, write: false, writeWithoutResponse: false, notify: true, indicate: false },
    getDescriptors: async () => [],
    readValue: async () => {
      throw new Error('Heart Rate Measurement is not readable')
    },
    writeValueWithResponse: async () => {
      throw new Error('Heart Rate Measurement is not writable')
    },
    writeValueWithoutResponse: async () => {
      throw new Error('Heart Rate Measurement is not writable')
    },
    startNotifications: async () => {
      if (!holdSubscribe) {
        return undefined
      }
      holdSubscribe = false
      return new Promise(resolve => {
        pendingSubscribeResolve = resolve
      })
    },
    stopNotifications: async () => undefined,
    addNotificationListener: listener => notificationListeners.add(listener),
    removeNotificationListener: listener => notificationListeners.delete(listener)
  }
  const controlPointCharacteristic = {
    uuid: controlPointUuid,
    properties: { read: true, write: true, writeWithoutResponse: false, notify: false, indicate: false },
    getDescriptors: async () => [],
    readValue: async () => new Uint8Array([0]),
    writeValueWithResponse: async value => {
      writes.push([...value])
    },
    writeValueWithoutResponse: async () => {
      throw new Error('Heart Rate Control Point does not support write without response')
    },
    startNotifications: async () => undefined,
    stopNotifications: async () => undefined,
    addNotificationListener: () => undefined,
    removeNotificationListener: () => undefined
  }
  const service = {
    uuid: serviceUuid,
    getCharacteristics: async () => [measurementCharacteristic, controlPointCharacteristic]
  }
  const device = {
    id: 'g6a-packed-web-heart-rate-device',
    gatt: {
      get connected() {
        return connected
      },
      connect: async () => {
        connected = true
      },
      disconnect: () => {
        connected = false
        for (const listener of disconnectListeners) {
          listener()
        }
      },
      getPrimaryServices: async () => [service]
    },
    addDisconnectListener: listener => disconnectListeners.add(listener),
    removeDisconnectListener: listener => disconnectListeners.delete(listener)
  }
  const lifecycleListeners = new Set()
  const boundary = {
    implementationVersion: 'g6a-packed-browser-boundary-0.1.0',
    browserEngine: 'deterministic-browser-boundary',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => true,
    requestDevice: async options => {
      assert.equal(options.filters[0].services[0], serviceUuid, 'packed Web chooser requested Heart Rate service')
      return { device, grantedServices: [serviceUuid] }
    },
    now: () => Date.now(),
    setTimer: (callback, delayMilliseconds) => setTimeout(callback, delayMilliseconds),
    clearTimer: handle => clearTimeout(handle),
    addPageLifecycleListener: listener => {
      lifecycleListeners.add(listener)
      return () => lifecycleListeners.delete(listener)
    }
  }
  return {
    boundary,
    controls: {
      writes,
      holdNextSubscribe: () => {
        holdSubscribe = true
      },
      resolvePendingSubscribe: () => {
        if (pendingSubscribeResolve === null) {
          throw new Error('packed Web pending subscribe was not created')
        }
        const resolve = pendingSubscribeResolve
        pendingSubscribeResolve = null
        resolve(undefined)
      },
      emitNotification: value => {
        measurement = new Uint8Array(value)
        for (const listener of notificationListeners) {
          listener(new Uint8Array(value))
        }
      },
      lifecycleListeners
    }
  }
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

function assertCleanup(cleanup, operation) {
  assert.equal(cleanup.state, 'released', `${operation} released resources`)
  assert.deepEqual(cleanup.failures, [], `${operation} had no cleanup failures`)
}

async function assertAborted(promise, operation) {
  let timeoutHandle = null
  try {
    await Promise.race([
      promise.then(
        () => {
          throw new Error(`packed Web ${operation} unexpectedly resolved`)
        },
        error => {
          assert.equal(error?.normalized?.code, 'operation.aborted', `packed Web ${operation} abort code`)
        }
      ),
      new Promise((_, reject) => {
        timeoutHandle = setTimeout(() => {
          reject(new Error(`packed Web ${operation} did not settle before its 5000ms timeout`))
        }, 5000)
      })
    ])
  } finally {
    if (timeoutHandle !== null) {
      clearTimeout(timeoutHandle)
    }
  }
}

async function cleanupStep(errors, operation, label) {
  try {
    assertCleanup(await operation(), label)
  } catch (error) {
    errors.push(error)
  }
}
