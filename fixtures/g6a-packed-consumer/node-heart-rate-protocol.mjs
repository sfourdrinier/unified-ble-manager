// fixtures/g6a-packed-consumer/node-heart-rate-protocol.mjs

import assert from 'node:assert/strict'
import { createDeterministicTestBleManager, VirtualPeripheral } from 'unified-ble-manager/testing'
import {
  encodeResetEnergyExpended,
  HEART_RATE_CONTROL_POINT_CHARACTERISTIC,
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import { strictNumericCounters } from './resource-counters.mjs'

const operationOptions = Object.freeze({ signal: null })
const notificationOptions = Object.freeze({
  ...operationOptions,
  delivery: 'prefer-notification',
  stream: 'balanced'
})

export async function runNodeHeartRateProtocol() {
  const measurement = new Uint8Array([0x06, 72])
  const { manager, fixture, attachment } = await createDeterministicTestBleManager({ backend: {
    peripheral: new VirtualPeripheral({
      key: 'g6a-packed-heart-rate-node',
      services: [
        {
          uuid: HEART_RATE_SERVICE,
          occurrence: 0,
          primary: true,
          characteristics: [
            {
              uuid: HEART_RATE_MEASUREMENT_CHARACTERISTIC,
              occurrence: 0,
              initialValue: measurement,
              readable: false,
              writableWithResponse: false,
              writableWithoutResponse: false,
              notifying: true,
              indicating: false,
              descriptors: []
            },
            {
              uuid: HEART_RATE_CONTROL_POINT_CHARACTERISTIC,
              occurrence: 0,
              initialValue: new Uint8Array([0]),
              readable: true,
              writableWithResponse: true,
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
})

  let scan = null
  let connection = null
  let subscription = null
  let primaryError = null
  const cleanupErrors = []
  try {
    scan = await settle(fixture, manager.scan({ delivery: 'balanced' }), 'scan.start')
    const observationPromise = scan.observations[Symbol.asyncIterator]().next()
    fixture.controller.emitAdvertisement(advertisement(attachment))
    await flushMicrotasks()
    const observation = await observationPromise
    assert.equal(observation.done, false, 'packed Node scan produced an observation')
    assert.equal(observation.value.kind, 'value', 'packed Node scan produced a value item')

    connection = await settle(fixture, manager.connect(observation.value.value.peer.id, operationOptions), 'connect')
    const database = await settle(fixture, connection.discover(operationOptions), 'discover')
    const measurementCharacteristic = database.characteristic(
      HEART_RATE_SERVICE,
      HEART_RATE_MEASUREMENT_CHARACTERISTIC
    )
    const controlPointCharacteristic = database.characteristic(
      HEART_RATE_SERVICE,
      HEART_RATE_CONTROL_POINT_CHARACTERISTIC
    )
    const abort = new AbortController()
    abort.abort()
    await assertAborted(
      fixture,
      measurementCharacteristic.subscribe({
        signal: abort.signal,
        delivery: notificationOptions.delivery,
        stream: notificationOptions.stream
      }),
      'subscribe.cancellation'
    )
    subscription = await settle(fixture, measurementCharacteristic.subscribe(notificationOptions), 'subscribe')
    const values = subscription.values[Symbol.asyncIterator]()
    const measurementPromise = values.next()
    fixture.backend.emitNotification(
      {
        serviceUuid: HEART_RATE_SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: HEART_RATE_MEASUREMENT_CHARACTERISTIC,
        characteristicOccurrence: 0
      },
      measurement
    )
    await flushMicrotasks()
    const measurementItem = await measurementPromise
    assert.equal(measurementItem.done, false, 'packed Node Heart Rate subscription remained open')
    assert.equal(measurementItem.value.kind, 'value', 'packed Node Heart Rate subscription delivered a value')
    assert.equal(
      parseHeartRateMeasurement(measurementItem.value.value.value).beatsPerMinute,
      72,
      'packed Node Heart Rate profile decoded the notification response'
    )
    const response = await settle(fixture, controlPointCharacteristic.read(operationOptions), 'response-read')
    assert.deepEqual([...response], [0], 'packed Node Heart Rate profile read the control-point response')

    const write = await settle(
      fixture,
      controlPointCharacteristic.write(encodeResetEnergyExpended(), { ...operationOptions, response: 'required' }),
      'write'
    )
    assert.equal(write.commitState, 'confirmed', 'packed Node Heart Rate command write committed')
    const writes = fixture.controller.peripheral.recordedWrites()
    assert.deepEqual(
      [...writes[0].value],
      [1],
      'packed Node Heart Rate profile emitted the SIG Reset Energy Expended command'
    )
    subscription = null

    assertCleanup(await settle(fixture, scan.stop(), 'scan.stop'), 'packed Node scan cleanup')
    scan = null
    assertCleanup(await settle(fixture, connection.release(), 'connection.release'), 'packed Node connection cleanup')
    connection = null
  } catch (error) {
    primaryError = error
  }

  if (subscription !== null) {
    await cleanupStep(
      cleanupErrors,
      () => settle(fixture, subscription.remove(), 'subscription.remove'),
      'packed Node subscription cleanup'
    )
  }
  if (scan !== null) {
    await cleanupStep(cleanupErrors, () => settle(fixture, scan.stop(), 'scan.stop'), 'packed Node scan cleanup')
  }
  if (connection !== null) {
    await cleanupStep(
      cleanupErrors,
      () => settle(fixture, connection.release(), 'connection.release'),
      'packed Node connection cleanup'
    )
  }
  await cleanupStep(
    cleanupErrors,
    () => settle(fixture, manager.destroy(), 'manager.destroy'),
    'packed Node manager cleanup'
  )

  if (primaryError !== null && cleanupErrors.length > 0) {
    throw new AggregateError([primaryError, ...cleanupErrors], 'packed Node vendor protocol and cleanup failed')
  }
  if (primaryError !== null) {
    throw primaryError
  }
  if (cleanupErrors.length > 0) {
    throw new AggregateError(cleanupErrors, 'packed Node vendor protocol cleanup failed')
  }

  const counters = strictNumericCounters(manager.diagnostics.resourceCounters(), 'Node')
  assert.deepEqual(Object.values(counters), new Array(Object.keys(counters).length).fill(0), 'packed Node resources released')
  return {
    schema: 'unified-ble-g6a-host-proof-v1',
    host: {
      id: 'node',
      family: 'node',
      runtime: 'node',
      moduleSystem: 'esm-loaded-from-commonjs-host',
      backendHostKind: 'test',
      browserEngine: null,
      liveBrowserEngine: false
    },
    packageContract: 'public-root-testing-and-profile-subpaths',
    vendorProtocol: {
      profile: 'bluetooth-sig-heart-rate',
      scan: 'passed',
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

function advertisement(attachment) {
  const absent = Object.freeze({ state: 'absent', reason: 'not-provided', provenance: 'not-provided' })
  return {
    device: {
      id: 'g6a-packed-heart-rate-peer',
      backendInstanceId: attachment.backendInstanceId,
      scope: 'backend',
      stableAcrossRestarts: false,
      address: null
    },
    provenance: 'platform-raw',
    sourceTimestamp: absent,
    receivedAtMonotonicMs: 0,
    ingressOrdinal: 1,
    scanSessionId: 'deterministic-scan-session',
    localName: { state: 'present', value: 'G6A Heart Rate', provenance: 'observed' },
    rssi: absent,
    txPower: absent,
    connectable: { state: 'present', value: true, provenance: 'observed' },
    appearance: absent,
    serviceUuids: { state: 'present', value: [HEART_RATE_SERVICE], provenance: 'observed' },
    solicitedServiceUuids: absent,
    overflowServiceUuids: absent,
    serviceData: absent,
    manufacturerData: absent,
    rawRecord: absent,
    scanResponseRecord: absent
  }
}

async function settle(fixture, promise, operation) {
  if (promise === null || typeof promise.then !== 'function') {
    throw new TypeError(`packed Node ${operation} did not return a promise`)
  }
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await flushMicrotasks()
  }
  if (!settled) {
    throw new Error(`packed Node ${operation} did not settle after 100 deterministic pump attempts`)
  }
  return promise
}

async function flushMicrotasks() {
    for (let turn = 0; turn < 32; turn += 1) {
    await Promise.resolve()
  }
}

function assertCleanup(cleanup, operation) {
  assert.equal(cleanup.state, 'released', `${operation} released resources`)
  assert.deepEqual(cleanup.failures, [], `${operation} had no cleanup failures`)
}

async function assertAborted(fixture, promise, operation) {
  let settled = false
  let rejected = false
  let rejection = null
  void promise.then(
    () => {
      settled = true
    },
    error => {
      settled = true
      rejected = true
      rejection = error
    }
  )
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await flushMicrotasks()
  }
  if (!settled) {
    throw new Error(`packed Node ${operation} did not settle after 100 deterministic pump attempts`)
  }
  assert.equal(rejected, true, `packed Node ${operation} rejected`)
  assert.equal(rejection?.code ?? rejection?.normalized?.code, 'operation.aborted', `packed Node ${operation} abort code`)
}

async function cleanupStep(errors, operation, label) {
  try {
    assertCleanup(await operation(), label)
  } catch (error) {
    errors.push(error)
  }
}
