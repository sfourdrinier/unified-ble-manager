const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { BluezDbusMethodError } = require('../../../src/backends/bluez/bluez-dbus-contract')
const { awaitSignal } = require('../../helpers/async')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'
const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function objects(paired = false) {
  return [
    {
      path: adapterPath,
      interfaces: [
        {
          name: BLUEZ_ADAPTER_INTERFACE,
          properties: {
            Address: { signature: 's', value: '00:11:22:33:44:55' },
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true }
          }
        }
      ]
    },
    {
      path: devicePath,
      interfaces: [
        {
          name: BLUEZ_DEVICE_INTERFACE,
          properties: {
            Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
            Alias: { signature: 's', value: 'security peer' },
            RSSI: { signature: 'n', value: -40 },
            UUIDs: { signature: 'as', value: [] },
            Connected: { signature: 'b', value: false },
            ServicesResolved: { signature: 'b', value: false },
            Paired: { signature: 'b', value: paired }
          }
        }
      ]
    }
  ]
}

async function createFixture(paired = false, pairingGeneration = undefined) {
  const boundary = new InMemoryBluezBoundary({ objects: objects(paired) })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 100,
    pairingGeneration
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  const scan = await backend.scanner.start(
    {
      filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
      duplicatePolicy: 'all',
      timestampPolicy: 'receipt-monotonic',
      delivery: { itemCapacity: 4, byteCapacity: 4096, reservedControlCapacity: 1, overflowPolicy: 'drop-oldest' },
      deadline: null,
      signal: null,
      sharing: { mode: 'owner', allowSharing: false }
    },
    opaqueId('bluez-security-client', 'client', 'bluez-security')
  )
  const iterator = scan.observations[Symbol.asyncIterator]()
  const observation = iterator.next()
  boundary.queueAdvertisement()
  const item = await observation
  if (item.done || item.value.kind !== 'value') throw new Error('BlueZ security fixture did not observe its peer')
  await iterator.return()
  await scan.stop()
  return { backend, boundary, peerId: String(item.value.value.device.id) }
}

function pairOptions(overrides = {}) {
  return {
    signal: null,
    deadline: null,
    transport: 'auto',
    protection: 'system-default',
    ceremony: 'system',
    ...overrides
  }
}

describe('BlueZ system security backend', () => {
  test('rolls back security watch ownership when the initial state read fails', async () => {
    const { backend } = await createFixture()

    expect(() => backend.security.watch('missing-peer')).toThrow()
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not remove a device when unpair is already aborted', async () => {
    const { backend, boundary, peerId } = await createFixture(true)
    const controller = new AbortController()
    controller.abort()

    await expect(backend.security.unpair(peerId, { signal: controller.signal, deadline: null })).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([
        expect.objectContaining({ interfaceName: BLUEZ_ADAPTER_INTERFACE, method: 'RemoveDevice' })
      ])
    )
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('measures Paired, resolves after the Paired property, watches changes, and removes the OS device', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(backend.security.state(observedPeerId, pairOptions())).resolves.toMatchObject({
      bond: 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported'
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 1, state: { bond: 'not-bonded' } } }
    })
    await expect(backend.security.pair(observedPeerId, pairOptions())).resolves.toMatchObject({
      outcome: 'paired',
      state: { bond: 'bonded' }
    })
    // A just-works agent must be ensured before Pair, or BlueZ aborts the SMP.
    // Prove ordering, not just occurrence: the ensure must have happened no
    // later than the Device1.Pair call's position in the recorded call sequence.
    expect(boundary.pairingAgentEnsured).toBeGreaterThanOrEqual(1)
    const pairCallIndex = boundary.calls.findIndex(
      call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'Pair'
    )
    expect(pairCallIndex).toBeGreaterThanOrEqual(0)
    expect(boundary.pairingAgentEnsuredAtCallIndex).toBeLessThanOrEqual(pairCallIndex)
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 2, state: { bond: 'bonded' } } }
    })
    await expect(backend.security.pair(observedPeerId, pairOptions())).resolves.toMatchObject({
      outcome: 'already-paired'
    })
    await expect(backend.security.unpair(observedPeerId, pairOptions())).resolves.toEqual({ outcome: 'unpaired' })
    expect(boundary.calls.at(-1)).toMatchObject({
      interfaceName: BLUEZ_ADAPTER_INTERFACE,
      method: 'RemoveDevice',
      argumentsValue: [{ signature: 'o', value: devicePath }]
    })
    await iterator.return()
    await stream.close()
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  /**
   * cancelPairing() and pair() can never disagree about the same operation,
   * because cancelPairing reads the pairing's OWN answer instead of forming a
   * second opinion. Two observations of one fact can differ; one cannot.
   *
   * This is the #159 half. The remaining race - both agreeing on 'cancelled'
   * when the daemon bonded anyway - is #157, and closing it needs an answer to
   * "may an aborted pair stop being prompt", which the sibling tests below
   * ("cancels promptly while the native Pair call remains pending") show is a
   * deliberate requirement, not an oversight: a wedged bluetoothd must not hang
   * the caller.
   *
   * Ordering is FORCED, not timed - the test resolves Pair itself.
   */
  test('cancelPairing never contradicts the pairing it cancelled', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let completePairing = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          completePairing = resolve
        })
    )

    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    const cancelling = backend.security.cancelPairing(observedPeerId, pairOptions())
    await Promise.resolve()
    completePairing()

    const [cancelResult, pairResult] = [await cancelling, await pairing]
    // Whatever the answer is, it is ONE answer. A cancellation that reported
    // 'cancelled' while the pairing reported 'paired' is the defect.
    const pairSaysBonded =
      pairResult.outcome === 'paired' || pairResult.outcome === 'already-paired' || pairResult.outcome === 'repaired'
    expect(cancelResult.outcome === 'paired').toBe(pairSaysBonded)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels an in-flight system pairing without claiming a bond', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    // A genuinely in-flight pairing: Device1.Pair stays pending (real BlueZ does
    // not resolve Pair until the bond completes or fails), so cancelling before
    // it resolves must yield cancelled and leave no bond.
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    await expect(backend.security.state(observedPeerId, pairOptions())).resolves.toMatchObject({ bond: 'not-bonded' })
    // Settle the still-pending native call so the operation fully retires; only
    // then does a second cancel correctly report nothing in flight.
    const active = backend.security.activePairings.get(observedPeerId)
    if (active !== undefined) {
      resolvePair()
      await active.dispatch.physicalSettlement
    }
    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'not-pairing'
    })
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not dispatch pairing after the caller has already aborted', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    await expect(backend.security.pair(observedPeerId, pairOptions({ protection: 'encrypted' }))).rejects.toMatchObject(
      { normalized: { code: 'capability.unsupported' } }
    )
    const controller = new AbortController()
    controller.abort()
    await expect(backend.security.pair(observedPeerId, pairOptions({ signal: controller.signal }))).resolves.toEqual({
      outcome: 'cancelled'
    })
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'Pair' })])
    )
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not fire Pair() when aborted while the pairing agent is still registering', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    // Registering the agent is a real IPC round-trip on the live bus; model it
    // as a pending promise so the abort can land inside that window - the exact
    // gap where onCancellation sees pairCallStarted === false and skips the
    // native cancel.
    let releaseAgent = () => undefined
    boundary.ensurePairingAgent = () =>
      new Promise(resolve => {
        releaseAgent = () => {
          boundary.pairingAgentEnsured = (boundary.pairingAgentEnsured ?? 0) + 1
          resolve()
        }
      })
    const controller = new AbortController()
    const pairing = backend.security.pair(observedPeerId, pairOptions({ signal: controller.signal }))
    // Let the operation body reach and suspend on ensurePairingAgent().
    await Promise.resolve()
    await Promise.resolve()
    // Abort arrives while registration is still in flight, then registration
    // completes and the body resumes.
    controller.abort()
    releaseAgent()
    // Drain the resumed continuation: in the broken case it would go on to push
    // a Device1.Pair call; the fix re-checks the abort and throws first.
    for (let flush = 0; flush < 5; flush += 1) await Promise.resolve()
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'Pair' })])
    )
    // And having never begun a native pairing, it must not have issued a stray
    // CancelPairing either.
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([
        expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'CancelPairing' })
      ])
    )
    await expect(backend.security.state(observedPeerId, pairOptions())).resolves.toMatchObject({ bond: 'not-bonded' })
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('surfaces a CancelPairing failure instead of reporting a bond it could not stop as cancelled', async () => {
    const { backend, boundary, peerId } = await createFixture()
    let pairDispatched
    const pairSeen = new Promise(resolve => {
      pairDispatched = resolve
    })
    // Hold Device1.Pair open so the pairing is genuinely in flight when the
    // cancellation arrives - that is the only state in which CancelPairing is
    // dispatched at all.
    let resolvePair = () => undefined
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Pair', () => {
      pairDispatched()
      return new Promise(resolve => {
        resolvePair = resolve
      })
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'CancelPairing', async () => {
      throw new BluezDbusMethodError({
        name: 'org.bluez.Error.Failed',
        message: 'cancel failed',
        safeDetails: {}
      })
    })

    const pairing = backend.security.pair(peerId, pairOptions())
    pairing.catch(() => undefined)
    await awaitSignal(pairSeen, 'Device1.Pair to be dispatched')

    // bluetoothd refused to stop the bonding, so the peer may still bond.
    // Reporting 'cancelled' here would tell the caller no pairing happened
    // while one is still running.
    await expect(backend.security.cancelPairing(peerId, pairOptions())).rejects.toMatchObject({
      normalized: { code: 'platform.failure' }
    })

    resolvePair()
    await Promise.resolve()
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('treats a CancelPairing rejection that proves no pairing is in progress as cancelled', async () => {
    // 'No pairing in progress' is the answer to the question, not a failure to
    // answer it: nothing is left running, so the cancellation succeeded and the
    // caller is told the truth by reporting it.
    for (const name of ['org.bluez.Error.DoesNotExist', 'org.freedesktop.DBus.Error.UnknownObject']) {
      const { backend, boundary, peerId } = await createFixture()
      let resolvePair = () => undefined
      let pairDispatched = () => undefined
      const pairSeen = new Promise(resolve => {
        pairDispatched = resolve
      })
      boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Pair', () => {
        pairDispatched()
        return new Promise(resolve => {
          resolvePair = resolve
        })
      })
      boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'CancelPairing', async () => {
        throw new BluezDbusMethodError({ name, message: 'no pairing in progress', safeDetails: {} })
      })

      const pairing = backend.security.pair(peerId, pairOptions())
      await awaitSignal(pairSeen, `Device1.Pair to be dispatched for ${name}`)

      await expect(backend.security.cancelPairing(peerId, pairOptions())).resolves.toEqual({ outcome: 'cancelled' })
      await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })

      resolvePair()
      await Promise.resolve()
      await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
    }
  })

  test('reports security:pairing-generation unsupported when the host supplied no privileged operation', async () => {
    const { backend } = await createFixture()
    const descriptor = backend.features.descriptors.find(entry => entry.id === 'security:pairing-generation')

    expect(descriptor).toMatchObject({ state: 'unsupported' })
    // The reason names the platform gap and the way out, rather than a bare
    // "unsupported" that teaches a caller nothing.
    expect(JSON.stringify(descriptor.limitations)).toContain('CAP_NET_ADMIN')
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  /**
   * The capability contract working as intended: two BlueZ backends on one
   * machine legitimately answer differently, because one was handed the
   * privileged operation and the other was not. A static platform matrix could
   * not express that.
   */
  test('reports security:pairing-generation supported once a privileged operation is supplied', async () => {
    const { backend } = await createFixture(false, { read: async () => 'enabled', set: async () => undefined })
    const descriptor = backend.features.descriptors.find(entry => entry.id === 'security:pairing-generation')

    // 'limited', not 'supported': the evidence behind this is deterministic
    // tests, not a bond made by a real radio. The same honesty applies to
    // peer:address-targeting. It stops being 'limited' when physical evidence
    // says so, never because the code looks finished.
    expect(descriptor).toMatchObject({ state: 'limited', implementationOrigin: 'backend-native' })
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  /**
   * An explicit null is what a JSON config produces for "not configured". The
   * capability gate and the runtime's coercion disagreed about it, so the
   * descriptor claimed the capability while every directed pairing rejected.
   */
  test('an explicitly null controller reports unsupported and still fails closed', async () => {
    const { backend, peerId } = await createFixture(false, null)
    const descriptor = backend.features.descriptors.find(entry => entry.id === 'security:pairing-generation')

    expect(descriptor).toMatchObject({ state: 'unsupported' })
    await expect(backend.security.pair(peerId, pairOptions({ secureConnections: 'require' }))).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('holds the adapter at the requested generation for a directed pairing, then restores it', async () => {
    const generations = []
    let current = 'enabled'
    const { backend, boundary, peerId } = await createFixture(false, {
      read: async () => current,
      set: async (_adapterId, generation) => {
        generations.push(generation)
        current = generation
      }
    })

    await expect(backend.security.pair(peerId, pairOptions({ secureConnections: 'disallow' }))).resolves.toMatchObject({
      outcome: 'paired'
    })

    // Held for the pairing, then put back - the setting is adapter-wide, so
    // leaving it would weaken every later bond on this host.
    expect(generations).toEqual(['legacy-only', 'enabled'])
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'Pair')
    ).toHaveLength(1)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('rejects a secureConnections generation it cannot select (require and disallow) on BlueZ', async () => {
    const { backend, boundary, peerId } = await createFixture()
    for (const value of ['require', 'disallow']) {
      await expect(backend.security.pair(peerId, pairOptions({ secureConnections: value }))).rejects.toMatchObject({
        normalized: { code: 'capability.unsupported' }
      })
    }
    // Fail-closed: no native Device1.Pair is dispatched for a generation we
    // cannot honour.
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'Pair' })])
    )
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('reports paired (not cancelled) when an abort lands after Device1.Pair has bonded', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    // Pair() resolves (the bond exists), but the confirming Paired signal never
    // arrives, so the operation stays in the post-Pair wait where a late abort
    // can land.
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Pair', () => false)
    const controller = new AbortController()
    const pairing = backend.security.pair(observedPeerId, pairOptions({ signal: controller.signal }))
    // Let the body run through Pair() and into the Paired wait.
    for (let flush = 0; flush < 6; flush += 1) await Promise.resolve()
    controller.abort()
    // A bond we created must be reported truthfully, never as cancelled.
    await expect(pairing).resolves.toMatchObject({ outcome: 'paired', state: { bond: 'bonded' } })
    // And no CancelPairing may be issued against a completed bond.
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([
        expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'CancelPairing' })
      ])
    )
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels promptly while the native Pair call remains pending and issues one CancelPairing', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    resolvePair()
    await Promise.resolve()
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels promptly at a deadline while the native Pair call remains pending', async () => {
    jest.useFakeTimers()
    try {
      const { backend, boundary, peerId: observedPeerId } = await createFixture()
      let resolvePair = () => undefined
      boundary.onCall(
        devicePath,
        BLUEZ_DEVICE_INTERFACE,
        'Pair',
        () =>
          new Promise(resolve => {
            resolvePair = resolve
          })
      )
      const pairing = backend.security.pair(observedPeerId, pairOptions({ deadline: 110 }))
      const result = expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
      await jest.advanceTimersByTimeAsync(10)

      await result
      expect(
        boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
      ).toHaveLength(1)
      resolvePair()
      await Promise.resolve()
      await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
    } finally {
      jest.useRealTimers()
    }
  })

  test('cancels active pairing and terminates security watchers when the device is removed', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    boundary.objectManager.emitInterfacesRemoved(devicePath, [BLUEZ_DEVICE_INTERFACE])
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'operation-aborted' } })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    const active = backend.security.activePairings.get(observedPeerId)
    if (active === undefined) throw new Error('BlueZ active pairing disappeared before physical settlement')
    resolvePair()
    await active.dispatch.physicalSettlement
    expect(backend.security.activePairings.size).toBe(0)
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels active pairing and terminates security watchers on a BlueZ daemon reset', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    boundary.emitReset('test reset')
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'source-failed' } })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    const active = backend.security.activePairings.get(observedPeerId)
    if (active === undefined) throw new Error('BlueZ active pairing disappeared before physical settlement')
    resolvePair()
    await active.dispatch.physicalSettlement
    expect(backend.security.activePairings.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })
})
