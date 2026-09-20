'use strict'

// __tests__/ConnectRadioFailureRetryability.test.js
//
// RV1 finding 2: `connection.failed` from a genuine radio failure must report
// `caller-decides` — the vocabulary (event-vocabulary `connect-not-established`)
// and the recovery catalog (`retry-with-backoff`) say repeating the attempt is
// the caller's policy because nothing was committed. Three paths derived
// `never` instead: the WinRT platform-error helper, the desktop-core
// `classify_connect_failure`, and the unified-core non-contract fallback.
// Genuinely terminal refusals (`connection.already-owned`, `peer.not-found`,
// `ownership.denied`) stay `never`.

const { attachBleBackend, createBleManager, createManagerOwnershipAuthority } = require('../src/manager/ble-manager')
const { DEFAULT_BLE_MANAGER_OPTIONS } = require('../src/manager/ble-manager')
const { deadline, opaqueId, version, versionRange } = require('../src/backend-contract/primitives')
const { contractError } = require('../src/backend-contract/errors')
const { winRtPlatformError } = require('../src/backends/winrt/winrt-backend-helpers')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function peer() {
  return opaqueId('radio-failure-peer', 'peer', 'deterministic:radio-failure-peer')
}

function managerOptions(fixture) {
  return {
    ...DEFAULT_BLE_MANAGER_OPTIONS,
    now: () => fixture.controller.clock.now(),
    timer: {
      scheduleAt: (deadlineValue, action) => fixture.controller.clock.scheduleAt(deadlineValue, action)
    }
  }
}

async function createOwningFixture() {
  const fixture = createDeterministicTestBackend()
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const manager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('radio-failure-client', 'client', 'deterministic:radio-failure-client'),
      managerId: opaqueId('radio-failure-manager', 'manager', 'deterministic:radio-failure-manager'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    managerOptions(fixture)
  )
  return { fixture, manager }
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
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

/** A backend radio that loses the race like a genuine radio failure: a plain non-contract rejection. */
function installFailingRadio(fixture) {
  fixture.backend.connections.connect = async () => {
    throw new Error('radio lost the race')
  }
}

describe('radio-failure connect retryability (RV1 finding 2)', () => {
  test('winrt radio-failure connect reports caller-decides', () => {
    const error = winRtPlatformError('connection.failed', 'connection', 'winrt.connect', new Error('Unreachable'))
    expect(error.normalized.code).toBe('connection.failed')
    expect(error.normalized.retryability).toBe('caller-decides')
  })

  test('winrt terminal refusals stay never', () => {
    expect(winRtPlatformError('scan.start-failed', 'scan', 'winrt.scan', new Error('denied')).normalized.retryability).toBe(
      'never'
    )
    expect(
      winRtPlatformError('gatt.read-failed', 'gatt', 'winrt.read', new Error('att')).normalized.retryability
    ).toBe('never')
    expect(contractError('connection.already-owned', 'connection', 'retryability.test').normalized.retryability).toBe(
      'never'
    )
    expect(contractError('peer.not-found', 'connection', 'retryability.test').normalized.retryability).toBe('never')
    expect(contractError('ownership.denied', 'connection', 'retryability.test').normalized.retryability).toBe('never')
  })

  test('unified-core fallback for a non-contract radio failure reports caller-decides', async () => {
    const { fixture, manager } = await createOwningFixture()
    installFailingRadio(fixture)
    const pending = manager.connect(peer(), { signal: null, deadline: null })
    pending.catch(() => undefined)
    await expect(settle(fixture.controller, pending)).rejects.toMatchObject({
      normalized: { code: 'connection.failed', retryability: 'caller-decides' }
    })
    await settle(fixture.controller, manager.destroy())
  })
})
