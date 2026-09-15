// __tests__/tck/ubm5-watermark-and-fault-guard.test.js
//
// UBM 5.0 TCK causal-watermark drains + test-only fault/time hooks (TCK card).
// Test-first RED gate for src/tck/causal-watermark-drain.ts + src/tck/test-only-fault-hooks.ts.

const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const {
  createCausalWatermark,
  drainToWatermark
} = require('../../src/tck/causal-watermark-drain')
const {
  TCK_TEST_ONLY_MARKER,
  assertTestOnlyFaultContext,
  createTestOnlyFaultHooks,
  isProductionEntryCleanOfTestOnlyFaultExports
} = require('../../src/tck/test-only-fault-hooks')

describe('UBM5 TCK causal-watermark drains', () => {
  test('drains only the causally-submitted prefix while a scan stays live (no global-idleness wait)', async () => {
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
      require('../../src/manager/ble-manager')
    const { createAttachmentBoundIdFactory, version, versionRange } = require('../../src/backend-contract/primitives')
    const compatibility = () =>
      ({
        backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
        capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
        eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
        traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
      })
    let manager = null
    try {
      const attached = await attachBleBackend(fixture.backend, compatibility())
      const attachment = attached.attachment.attachment
      const ids = createAttachmentBoundIdFactory({
        attachmentId: attachment.attachmentId,
        backendInstanceId: attachment.backendInstanceId,
        backendGeneration: attachment.backendGeneration,
        adapterId: attachment.adapter.adapterId,
        adapterGeneration: attachment.adapter.adapterGeneration
      })
      const authority = createManagerOwnershipAuthority(attached)
      manager = await createBleManager(
        {
          attachedBackend: attached,
          clientId: ids.clientId('tck-watermark-client'),
          managerId: ids.managerId('tck-watermark-manager'),
          ownerMode: 'owning'
        },
        authority,
        { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => fixture.controller.now() }
      )
      const { scanOptions, operationOptions, connectAndDiscover } = require('../../src/tck/runner-public-scenario-support')
      const watermark = createCausalWatermark()
      const connected = await connectAndDiscover(manager, fixture, {
        id: 'scenario.scan-connect-discover-read-notify-destroy'
      })
      const characteristic = connected.snapshot.characteristics[0]
      if (characteristic === undefined) {
        throw new Error('watermark probe discovery returned no characteristic')
      }
      const scan = await fixture.controller.settle(manager.scan(scanOptions(false)))
      const firstRead = connected.database.read(characteristic.path, operationOptions)
      let readSettled = false
      firstRead.then(
        () => {
          readSettled = true
        },
        () => {
          readSettled = true
        }
      )
      watermark.markSubmitted('first-read')
      let flushCalls = 0
      const countingController = {
        ...fixture.controller,
        flush: async () => {
          flushCalls += 1
          return fixture.controller.flush()
        }
      }
      const drained = await drainToWatermark(countingController, watermark, [firstRead])
      expect(drained.drainedToSequence).toBe(1)
      expect(drained.globalIdlenessWaited).toBe(false)
      expect(readSettled).toBe(true)
      expect(flushCalls).toBeLessThanOrEqual(2)
      const value = await fixture.controller.settle(firstRead)
      expect(value.byteLength).toBeGreaterThan(0)
      const observations = scan.observations[Symbol.asyncIterator]()
      await fixture.controller.perform('queue-advertisement', Object.freeze({}))
      await fixture.controller.flush()
      const observed = await fixture.controller.settle(observations.next())
      expect(observed.done !== true && observed.value.kind === 'value').toBe(true)
      await fixture.controller.settle(connected.connection.release())
      const scanCleanup = await fixture.controller.settle(scan.stop())
      expect(scanCleanup).toEqual({ state: 'released', failures: [] })
    } finally {
      if (manager !== null) {
        await fixture.controller.settle(manager.destroy())
      }
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })
})

describe('UBM5 TCK test-only fault/time hooks shipping guard', () => {
  const ORIGINAL_ENV = process.env.NODE_ENV
  const ORIGINAL_MARKER = globalThis[TCK_TEST_ONLY_MARKER]

  afterEach(() => {
    process.env.NODE_ENV = ORIGINAL_ENV
    if (ORIGINAL_MARKER === undefined) {
      delete globalThis[TCK_TEST_ONLY_MARKER]
    } else {
      globalThis[TCK_TEST_ONLY_MARKER] = ORIGINAL_MARKER
    }
  })

  test('fail-closed marker asserts test-only (proves the guard)', () => {
    delete globalThis[TCK_TEST_ONLY_MARKER]
    process.env.NODE_ENV = 'test'
    expect(() => assertTestOnlyFaultContext('unit-probe')).toThrow('test-only')

    globalThis[TCK_TEST_ONLY_MARKER] = true
    process.env.NODE_ENV = 'test'
    expect(() => assertTestOnlyFaultContext('unit-probe')).not.toThrow()

    globalThis[TCK_TEST_ONLY_MARKER] = true
    process.env.NODE_ENV = 'production'
    expect(() => assertTestOnlyFaultContext('unit-probe')).toThrow('test-only')
  })

  test('fault hooks refuse to construct outside the test-only context', () => {
    delete globalThis[TCK_TEST_ONLY_MARKER]
    process.env.NODE_ENV = 'test'
    const factory = createDeterministicBackendTckFactory()
    expect(() => createTestOnlyFaultHooks({ controller: null, factory })).toThrow('test-only')
  })

  test('fault hooks construct inside the test-only context and expose only time/fault inputs', async () => {
    globalThis[TCK_TEST_ONLY_MARKER] = true
    process.env.NODE_ENV = 'test'
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      const hooks = createTestOnlyFaultHooks({ controller: fixture.controller, factory })
      expect(Object.keys(hooks).sort()).toEqual(['advanceTimeMs', 'emitNotification', 'queueAdvertisement'])
      await hooks.queueAdvertisement()
      await hooks.advanceTimeMs(0)
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('emitNotification observably delivers instead of silently succeeding', async () => {
    globalThis[TCK_TEST_ONLY_MARKER] = true
    process.env.NODE_ENV = 'test'
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
      require('../../src/manager/ble-manager')
    const { createAttachmentBoundIdFactory, version, versionRange } = require('../../src/backend-contract/primitives')
    const { connectAndDiscover, notificationInput, subscriptionOptions } =
      require('../../src/tck/runner-public-scenario-support')
    const attached = await attachBleBackend(fixture.backend, {
      backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
      capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
      eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
      traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
    })
    const attachment = attached.attachment.attachment
    const ids = createAttachmentBoundIdFactory({
      attachmentId: attachment.attachmentId,
      backendInstanceId: attachment.backendInstanceId,
      backendGeneration: attachment.backendGeneration,
      adapterId: attachment.adapter.adapterId,
      adapterGeneration: attachment.adapter.adapterGeneration
    })
    const authority = createManagerOwnershipAuthority(attached)
    const manager = await createBleManager(
      {
        attachedBackend: attached,
        clientId: ids.clientId('tck-emit-notification-client'),
        managerId: ids.managerId('tck-emit-notification-manager'),
        ownerMode: 'owning'
      },
      authority,
      { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => fixture.controller.now() }
    )
    try {
      const connected = await connectAndDiscover(manager, fixture, {
        id: 'scenario.scan-connect-discover-read-notify-destroy',
        execution: 'base',
        requiredFacts: [],
        requiredControllerActions: []
      })
      const characteristic = connected.snapshot.characteristics[0]
      if (characteristic === undefined) {
        throw new Error('emit-notification probe discovery returned no characteristic')
      }
      const hooks = createTestOnlyFaultHooks({ controller: fixture.controller, factory })
      const subscription = await fixture.controller.settle(
        connected.database.subscribe(characteristic.path, subscriptionOptions('drop-oldest', 4, 128))
      )
      const delivered = [9, 8, 7]
      await hooks.emitNotification(notificationInput(characteristic.path, new Uint8Array(delivered)))
      await fixture.controller.flush()
      const iterator = subscription.values[Symbol.asyncIterator]()
      const observed = await fixture.controller.settle(iterator.next())
      expect(observed.done).toBe(false)
      expect(observed.value.kind).toBe('value')
      expect([...observed.value.value.value]).toEqual(delivered)
      await fixture.controller.settle(subscription.remove())
      await fixture.controller.settle(connected.connection.release())
    } finally {
      await fixture.controller.settle(manager.destroy())
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('no reference/fault exports enter the production package entry', () => {
    expect(isProductionEntryCleanOfTestOnlyFaultExports()).toBe(true)
  })

  test('production entry check fails closed when the loader fails', () => {
    expect(
      isProductionEntryCleanOfTestOnlyFaultExports(() => {
        throw new Error('simulated production entry load failure')
      })
    ).toBe(false)
  })
})
