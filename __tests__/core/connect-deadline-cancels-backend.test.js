'use strict'

// __tests__/core/connect-deadline-cancels-backend.test.js
//
// Finding 194 root cause: UnifiedBleCore.connect settles the caller's
// deadline or abort without cancelling the backend acquisition. The backend
// call stays in flight, the core keeps a live Connecting claim
// (pendingConnectAcquisitions), a retry for the same peer arbitrates against
// the stale claim, and destroy reports connect.acquisition-pending.
//
// The fix: the core's deadline and abort cancel the backend acquisition
// through the backend contract's cancel path (the AbortSignal in
// ConnectionOptions). The result reports what happened — the connection
// failed on deadline expiry (finding 161: connection.failed,
// caller-decides, the deadline fact in platform), or was aborted on caller
// abort (operation.aborted).

const { attachBleBackend, createBleManager, createManagerOwnershipAuthority } = require('../../src/manager/ble-manager')
const { DEFAULT_BLE_MANAGER_OPTIONS } = require('../../src/manager/ble-manager')
const { deadline, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { contractError } = require('../../src/backend-contract/errors')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function peer() {
  return opaqueId('direct-peer', 'peer', 'deterministic:direct-peer')
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
      clientId: opaqueId('connect-cancel-client', 'client', 'deterministic:connect-cancel-client'),
      managerId: opaqueId('connect-cancel-manager', 'manager', 'deterministic:connect-cancel-manager'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    managerOptions(fixture)
  )
  return { fixture, manager }
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
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
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

function abortedError() {
  return contractError('operation.aborted', 'core', 'connect-cancel-test.backend')
}

/**
 * A backend radio that hangs like a peer that never answers, arbitrates like
 * one (a second acquisition for a claimed peer is already-owned), and honors
 * the contract cancel path: aborting the options signal ends the in-flight
 * acquisition and frees its claim. Retries delegate to the real backend once
 * `hang` is cleared.
 */
function installHangingRadio(fixture) {
  const originalConnect = fixture.backend.connections.connect.bind(fixture.backend.connections)
  const claims = new Set()
  const seenSignals = []
  const state = { hang: true }
  fixture.backend.connections.connect = async (peerId, clientId, options) => {
    const key = String(peerId)
    if (claims.has(key)) {
      throw contractError('connection.already-owned', 'connection', 'connect-cancel-test.arbitration')
    }
    claims.add(key)
    seenSignals.push(options.signal ?? null)
    if (!state.hang) {
      claims.delete(key)
      return originalConnect(peerId, clientId, { ...options, signal: null, deadline: null })
    }
    return new Promise((_, reject) => {
      const signal = options.signal
      const onAbort = () => {
        claims.delete(key)
        reject(abortedError())
      }
      if (signal === null || signal === undefined) return
      if (signal.aborted === true) {
        onAbort()
        return
      }
      signal.addEventListener('abort', onAbort, { once: true })
    })
  }
  return { seenSignals, state }
}

describe('core connect deadline/abort cancels the backend acquisition (194)', () => {
  test('deadline expiry cancels the backend call and reports connection.failed', async () => {
    const { fixture, manager } = await createOwningFixture()
    const radio = installHangingRadio(fixture)
    const operationDeadline = deadline(Number(fixture.controller.clock.now()) + 5)
    const pending = manager.connect(peer(), { signal: null, deadline: operationDeadline })
    pending.catch(() => undefined)
    await flushMicrotasks()
    fixture.controller.clock.advanceBy(10)
    await expect(settle(fixture.controller, pending)).rejects.toMatchObject({
      normalized: {
        code: 'connection.failed',
        retryability: 'caller-decides',
        platform: { domain: 'core', code: 'deadline-expired' }
      }
    })
    expect(radio.seenSignals).toHaveLength(1)
    expect(radio.seenSignals[0]?.aborted).toBe(true)
    await settle(fixture.controller, manager.destroy())
  })

  test('caller abort cancels the backend call and stays operation.aborted', async () => {
    const { fixture, manager } = await createOwningFixture()
    const radio = installHangingRadio(fixture)
    const controller = new AbortController()
    const pending = manager.connect(peer(), { signal: controller.signal, deadline: null })
    pending.catch(() => undefined)
    await flushMicrotasks()
    controller.abort()
    await expect(settle(fixture.controller, pending)).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    expect(radio.seenSignals).toHaveLength(1)
    expect(radio.seenSignals[0]?.aborted).toBe(true)
    await settle(fixture.controller, manager.destroy())
  })

  test('a retry after an expired deadline connects instead of already-owned', async () => {
    const { fixture, manager } = await createOwningFixture()
    const radio = installHangingRadio(fixture)
    const operationDeadline = deadline(Number(fixture.controller.clock.now()) + 5)
    const first = manager.connect(peer(), { signal: null, deadline: operationDeadline })
    first.catch(() => undefined)
    await flushMicrotasks()
    fixture.controller.clock.advanceBy(10)
    await expect(settle(fixture.controller, first)).rejects.toMatchObject({
      normalized: { code: 'connection.failed' }
    })
    radio.state.hang = false
    const retry = await settle(fixture.controller, manager.connect(peer(), { signal: null, deadline: null }))
    expect(String(retry.connection.resource.connectionId)).toEqual(expect.any(String))
    await settle(fixture.controller, retry.release())
    await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })

  test('destroy after an expired deadline reports released with no live claim', async () => {
    const { fixture, manager } = await createOwningFixture()
    installHangingRadio(fixture)
    const operationDeadline = deadline(Number(fixture.controller.clock.now()) + 5)
    const pending = manager.connect(peer(), { signal: null, deadline: operationDeadline })
    pending.catch(() => undefined)
    await flushMicrotasks()
    fixture.controller.clock.advanceBy(10)
    await expect(settle(fixture.controller, pending)).rejects.toMatchObject({
      normalized: { code: 'connection.failed' }
    })
    await flushMicrotasks()
    await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({
      state: 'released',
      failures: []
    })
  })
})
