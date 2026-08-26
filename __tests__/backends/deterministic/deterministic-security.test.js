const {
  attachBleBackend,
  BleManager: InternalBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../../src/manager/ble-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createDeterministicTestBackend } = require('../../../src/testing/deterministic/deterministic-test-backend')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

async function createFixture() {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const internal = await InternalBleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('security-client', 'client', 'security'),
      managerId: opaqueId('security-manager', 'manager', 'security'),
      ownerMode: 'owning'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  const manager = await createPublicBleManager(internal, () => Number(fixture.controller.clock.now()))
  return { fixture, manager }
}

async function settle(fixture, promise) {
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
    await Promise.resolve()
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

const peer = Object.freeze({ id: 'deterministic-peer', name: null, rssi: null })

describe('deterministic security backend', () => {
  test('measures, pairs, watches, recognizes an existing bond, and unpairs', async () => {
    const { fixture, manager } = await createFixture()
    const watch = manager.security.watch(peer)[Symbol.asyncIterator]()

    await expect(manager.security.state(peer)).resolves.toMatchObject({
      bond: 'not-bonded',
      encryption: 'not-encrypted',
      authentication: 'unauthenticated',
      pairingPossible: true
    })
    const initialEvent = await settle(fixture, watch.next())
    expect(initialEvent.value).toMatchObject({ kind: 'state', peerId: peer.id, sequence: 1 })

    await expect(settle(fixture, manager.security.pair(peer))).resolves.toMatchObject({
      outcome: 'paired',
      state: { bond: 'bonded', encryption: 'encrypted', authentication: 'authenticated', secureConnections: 'yes' }
    })
    const pairedEvent = await settle(fixture, watch.next())
    expect(pairedEvent.value).toMatchObject({ kind: 'state', peerId: peer.id, sequence: 2, state: { bond: 'bonded' } })

    await expect(settle(fixture, manager.security.pair(peer))).resolves.toMatchObject({
      outcome: 'already-paired',
      state: { bond: 'bonded' }
    })
    await expect(settle(fixture, manager.security.unpair(peer))).resolves.toEqual({ outcome: 'unpaired' })
    const unpairedEvent = await settle(fixture, watch.next())
    expect(unpairedEvent.value).toMatchObject({
      kind: 'state',
      peerId: peer.id,
      sequence: 3,
      state: { bond: 'not-bonded' }
    })
    await expect(settle(fixture, manager.security.unpair(peer))).resolves.toEqual({ outcome: 'already-unpaired' })

    await watch.return()
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('arbitrates duplicate pairing and maps cancellation and timeout to terminal results', async () => {
    const { fixture, manager } = await createFixture()
    fixture.controller.queueCompletion('security-pair', {
      delayMs: 10,
      failure: null,
      cancellable: true,
      deadlineOrder: 'completion-first'
    })
    const first = manager.security.pair(peer)
    await expect(manager.security.pair(peer)).rejects.toMatchObject({ code: 'ownership.denied' })
    await expect(manager.security.cancelPairing(peer)).resolves.toEqual({ outcome: 'cancelled' })
    await expect(settle(fixture, first)).resolves.toEqual({ outcome: 'cancelled' })

    fixture.controller.queueCompletion('security-pair', {
      delayMs: 10,
      failure: null,
      cancellable: true,
      deadlineOrder: 'completion-first'
    })
    await expect(settle(fixture, manager.security.pair(peer, { timeoutMs: 5 }))).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(manager.security.cancelPairing(peer)).resolves.toEqual({ outcome: 'not-pairing' })
    await expect(manager.security.state(peer)).resolves.toMatchObject({ bond: 'not-bonded' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('bounds custom ceremony responses and cancels a pending challenge without leaking', async () => {
    const { fixture, manager } = await createFixture()
    const agent = {
      onChallenge: jest.fn(async challenge => {
        expect(challenge).toMatchObject({ kind: 'confirm-passkey', peer: { id: peer.id }, passkey: 123456 })
        return { kind: 'confirm-passkey', confirmed: true }
      })
    }

    const customPair = manager.security.pair(peer, { ceremony: agent })
    for (let turn = 0; turn < 5; turn += 1) await Promise.resolve()
    await expect(settle(fixture, customPair)).resolves.toMatchObject({
      outcome: 'paired'
    })
    expect(agent.onChallenge).toHaveBeenCalledTimes(1)
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })

    const pending = await createFixture()
    const pendingPair = pending.manager.security.pair(peer, {
      ceremony: { onChallenge: () => new Promise(() => undefined) }
    })
    await expect(pending.manager.security.cancelPairing(peer)).resolves.toEqual({ outcome: 'cancelled' })
    await expect(settle(pending.fixture, pendingPair)).resolves.toEqual({ outcome: 'cancelled' })
    await expect(pending.manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('rejects a secureConnections generation it cannot select instead of ignoring it', async () => {
    const { fixture, manager } = await createFixture()
    // Every real backend fails closed here (BlueZ, WinRT, Android), so the
    // deterministic one must too. A test backend that accepts a generation no
    // radio can honour lets a consumer's suite pass on a contract production
    // rejects - the one failure mode test infrastructure must never have.
    for (const value of ['require', 'disallow']) {
      await expect(settle(fixture, manager.security.pair(peer, { secureConnections: value }))).rejects.toMatchObject(
        { code: 'capability.unsupported' }
      )
    }
    // Fail-closed: no virtual bond is created for a generation we refuse.
    await expect(manager.security.state(peer)).resolves.toMatchObject({ bond: 'not-bonded' })
    await expect(settle(fixture, manager.security.pair(peer, { secureConnections: 'prefer' }))).resolves.toMatchObject(
      { outcome: 'paired' }
    )
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })
})
