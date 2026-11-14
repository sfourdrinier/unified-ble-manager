// __tests__/tck/deterministic/deterministic-resource-counters.test.js

const { capacity, opaqueId } = require('../../../src/backend-contract/primitives')
const { contractError } = require('../../../src/backend-contract/errors')
const { createDeterministicTestBackend } = require('../../../src/testing/deterministic/deterministic-test-backend')

function scanOptions(itemCapacity = 1) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(itemCapacity),
      byteCapacity: capacity(2),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

describe('deterministic resource counters', () => {
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

  async function startScan(fixture, client = 'deterministic-stop-client') {
    return settle(
      fixture.controller,
      fixture.backend.scanner.start(scanOptions(4), opaqueId(client, 'client', 'deterministic:scan-stop'))
    )
  }

  function advertisedPeer(id) {
    const absent = { state: 'absent', reason: 'test-not-observed', provenance: 'not-provided' }
    return {
      device: {
        id: opaqueId(id, 'peer', 'deterministic:scan-stop'),
        backendInstanceId: opaqueId('backend', 'backend-instance', 'deterministic:scan-stop'),
        scope: 'backend',
        stableAcrossRestarts: false,
        address: null
      },
      provenance: 'platform-raw',
      sourceTimestamp: absent,
      receivedAtMonotonicMs: 1,
      ingressOrdinal: 1,
      scanSessionId: opaqueId('session', 'scan-session', 'deterministic:scan-stop'),
      localName: absent,
      rssi: absent,
      txPower: absent,
      connectable: absent,
      appearance: absent,
      serviceUuids: absent,
      solicitedServiceUuids: absent,
      overflowServiceUuids: absent,
      serviceData: absent,
      manufacturerData: absent,
      rawRecord: absent,
      scanResponseRecord: absent
    }
  }

  test('successful stop drains its admitted deterministic scan prefix before owner-released', async () => {
    const fixture = createDeterministicTestBackend()
    const scan = await startScan(fixture)
    fixture.controller.emitAdvertisement(advertisedPeer('prefix-first-peer'))
    fixture.controller.emitAdvertisement(advertisedPeer('prefix-second-peer'))

    await expect(settle(fixture.controller, scan.stop())).resolves.toEqual({ state: 'released', failures: [] })
    const observations = scan.observations[Symbol.asyncIterator]()
    await expect(observations.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { device: { id: 'prefix-first-peer' } } }
    })
    await expect(observations.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { device: { id: 'prefix-second-peer' } } }
    })
    await expect(observations.next()).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'owner-released' } })
    await fixture.backend.destroy()
  })

  test('failed stop leaves a deterministic scan stream live for a retry', async () => {
    const fixture = createDeterministicTestBackend()
    const scan = await startScan(fixture, 'deterministic-stop-failure-client')
    fixture.controller.emitAdvertisement(advertisedPeer('retry-peer'))
    fixture.controller.queueCompletion('scan-stop', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })

    await expect(settle(fixture.controller, scan.stop())).rejects.toMatchObject({
      normalized: { code: 'platform.failure' }
    })
    fixture.controller.emitAdvertisement(advertisedPeer('retry-after-failure-peer'))
    const observations = scan.observations[Symbol.asyncIterator]()
    await expect(observations.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { device: { id: 'retry-peer' } } }
    })
    await expect(observations.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { device: { id: 'retry-after-failure-peer' } } }
    })

    await expect(settle(fixture.controller, scan.stop())).resolves.toEqual({ state: 'released', failures: [] })
    await expect(observations.next()).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'owner-released' } })
    await fixture.backend.destroy()
  })

  test('successful stop keeps an already-published deterministic source failure terminal', async () => {
    const fixture = createDeterministicTestBackend()
    const scan = await startScan(fixture, 'deterministic-source-failure-client')
    const failure = contractError('platform.failure', 'scan', 'deterministic.source').normalized
    scan.observations.closeWithReason('source-failed', failure)

    await expect(settle(fixture.controller, scan.stop())).resolves.toEqual({ state: 'released', failures: [] })
    await expect(scan.observations[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error: failure }
    })
    await fixture.backend.destroy()
  })

  test('reports retained records rather than idle control-capacity reservations', async () => {
    const fixture = createDeterministicTestBackend()

    expect(Number(fixture.backend.resourceCounters().retainedByteBuffers)).toBe(0)

    const events = fixture.backend.events()[Symbol.asyncIterator]()
    fixture.controller.setAdapterState('available', 'granted', 'off', 'counter regression')

    expect(Number(fixture.backend.resourceCounters().retainedByteBuffers)).toBe(1)
    await events.next()
    expect(Number(fixture.backend.resourceCounters().retainedByteBuffers)).toBe(0)

    await fixture.backend.destroy()
  })

  test('admits an idle control reservation exactly at the aggregate quota', async () => {
    const fixture = createDeterministicTestBackend({ aggregateStreamByteQuota: 1 })

    const started = fixture.backend.scanner.start(
      scanOptions(),
      opaqueId('counter-client', 'client', 'deterministic:counter')
    )
    fixture.controller.clock.runUntilIdle()
    const scan = await started

    expect(Number(fixture.backend.resourceCounters().retainedByteBuffers)).toBe(0)
    const stopped = scan.stop()
    fixture.controller.clock.runUntilIdle()
    await stopped
    await fixture.backend.destroy()
  })
})
