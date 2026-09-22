// __tests__/tck/ubm5-race-bounds-cleanup.test.js
//
// UBM 5.0 TCK race/bounds/cleanup vectors (TCK card; enables U2/U3/U7).
// Test-first RED gate for src/tck/race-bounds-cleanup-vectors.ts + approved-corrections.ts.

const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const {
  RACE_BOUNDS_CLEANUP_VECTOR_IDS,
  cleanupRecordsDeepEqual,
  runRaceBoundsCleanupVector
} = require('../../src/tck/race-bounds-cleanup-vectors')

const EXPECTED_VECTORS = [
  'cleanup.failed-cleanup-retains-and-reports',
  'cleanup.duplicate-destroy-is-idempotent',
  'generation.stale-path-rejects-before-dispatch',
  'completion.duplicate-completion-settles-once',
  'bounds.subscription-overflow-is-bounded-and-terminal',
  'cancel.admission-completion-boundary-settles-once',
  'invalidation.service-change-invalidates-generation'
]

function withSabotagedController(factory, onPerform) {
  return {
    ...factory,
    create: async context => {
      const fixture = await factory.create(context)
      return {
        ...fixture,
        controller: {
          ...fixture.controller,
          perform: (action, input) => onPerform(action, input, () => fixture.controller.perform(action, input))
        }
      }
    }
  }
}

const dropControllerAction = name => (action, input, next) =>
  action === name ? Promise.resolve() : next()

describe('UBM5 TCK race/bounds/cleanup vectors', () => {
  test('registers every required vector id exactly once', () => {
    expect([...RACE_BOUNDS_CLEANUP_VECTOR_IDS].sort()).toEqual([...EXPECTED_VECTORS].sort())
  })

  test.each(EXPECTED_VECTORS)('%s holds against the pinned TS reference', async vectorId => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, vectorId)
    expect(observation.vectorId).toBe(vectorId)
    expect(observation.holds).toBe(true)
    expect(observation.detail).toBeDefined()
  })

  test('duplicate completion settles once with exactly one late acknowledgement (counterexample first)', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'completion.duplicate-completion-settles-once')
    expect(observation.holds).toBe(true)
    expect(observation.detail.settledOnce).toBe(true)
    expect(observation.detail.noDoubleSettlement).toBe(true)
    expect(observation.detail.lateAcknowledgements).toBe(1)
    expect(observation.detail.followUpHealthy).toBe(true)
  })

  test('stale generation rejects before dispatch (counterexample first)', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'generation.stale-path-rejects-before-dispatch')
    expect(observation.holds).toBe(true)
    expect(observation.detail.staleRejected).toBe(true)
    expect(observation.detail.currentStillReads).toBe(true)
  })

  test('failed cleanup reports the failure and retains the resource until release', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'cleanup.failed-cleanup-retains-and-reports')
    expect(observation.holds).toBe(true)
    expect(observation.detail.failureReported).toBe(true)
    expect(observation.detail.retained).toBe(true)
    expect(observation.detail.followUpReleased).toBe(true)
  })

  test('duplicate destroy compares cleanup records by value, not identity', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'cleanup.duplicate-destroy-is-idempotent')
    expect(observation.holds).toBe(true)
    expect(observation.detail.equalCleanupRecords).toBe(true)
  })

  test('service-change rediscovery reads a non-empty new generation', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'invalidation.service-change-invalidates-generation')
    expect(observation.holds).toBe(true)
    expect(observation.detail.newGenerationByteLength).toBeGreaterThan(0)
  })

  test('cancel boundary settles once and records the late acknowledgement', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'cancel.admission-completion-boundary-settles-once')
    expect(observation.holds).toBe(true)
    expect(observation.detail.settledOnce).toBe(true)
    expect(observation.detail.lateAcknowledgedOnce).toBe(true)
  })

})

describe('UBM5 TCK race/bounds/cleanup negative controls (vectors can fail)', () => {
  test('failed cleanup passes cleanly when no failure is injected', async () => {
    const factory = withSabotagedController(
      createDeterministicBackendTckFactory(),
      dropControllerAction('inject-unsubscribe-failure')
    )
    const observation = await runRaceBoundsCleanupVector(factory, 'cleanup.failed-cleanup-retains-and-reports')
    expect(observation.holds).toBe(false)
    expect(observation.detail.failureReported).toBe(false)
  })

  test('stale path reads cleanly when no services-changed arrives', async () => {
    const factory = withSabotagedController(
      createDeterministicBackendTckFactory(),
      dropControllerAction('trigger-services-changed')
    )
    const observation = await runRaceBoundsCleanupVector(factory, 'generation.stale-path-rejects-before-dispatch')
    expect(observation.holds).toBe(false)
    expect(observation.detail.staleRejected).toBe(false)
  })

  test('duplicate completion signature discriminates: plain success settles differently', async () => {
    // Same setup as the vector but WITHOUT the abort: the read settles with
    // success and no late acknowledgement exists, so the vector's exact
    // conjuncts (abort-code settlement + exactly one late acknowledgement)
    // would fail. Proves the vector is not vacuous.
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
        require('../../src/manager/ble-manager')
      const { createAttachmentBoundIdFactory, version, versionRange } =
        require('../../src/backend-contract/primitives')
      const { connectAndDiscover, operationOptions, rejectsWithCode } =
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
          clientId: ids.clientId('tck-negative-duplicate-client'),
          managerId: ids.managerId('tck-negative-duplicate-manager'),
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
          throw new Error('negative duplicate probe discovery returned no characteristic')
        }
        await fixture.controller.perform(
          'queue-operation-completion',
          Object.freeze({ stage: 'read', delayMilliseconds: 10 })
        )
        await fixture.controller.perform(
          'queue-operation-completion',
          Object.freeze({ stage: 'read', delayMilliseconds: 10 })
        )
        let callerSettlements = 0
        const outcome = await fixture.controller.settle(
          connected.database.read(characteristic.path, operationOptions).then(
            value => {
              callerSettlements += 1
              return value
            },
            error => {
              callerSettlements += 1
              throw error
            }
          )
        )
        await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 10 }))
        await fixture.controller.flush()
        expect(callerSettlements).toBe(1)
        expect(outcome.byteLength).toBeGreaterThan(0)
        expect(await rejectsWithCode(Promise.resolve(outcome), 'operation.aborted')).toBe(false)
        const late = manager
          .traces()
          .filter(entry => entry.resource === 'operation')
          .filter(entry => entry.transition === 'late-success' || entry.transition === 'late-failure').length
        expect(late).toBe(0)
        await fixture.controller.settle(connected.connection.release())
      } finally {
        await fixture.controller.settle(manager.destroy())
      }
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('cancel boundary signature discriminates: un-aborted writes settle normally', async () => {
    // Same setup as the cancel vector but WITHOUT the abort: the write
    // succeeds and no late acknowledgement exists, so the vector's exact
    // conjuncts (aborted settlement + exactly one late acknowledgement)
    // would fail. Proves the vector is not vacuous.
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
        require('../../src/manager/ble-manager')
      const { createAttachmentBoundIdFactory, version, versionRange } =
        require('../../src/backend-contract/primitives')
      const { connectAndDiscover, rejectsWithCode } = require('../../src/tck/runner-public-scenario-support')
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
          clientId: ids.clientId('tck-negative-cancel-client'),
          managerId: ids.managerId('tck-negative-cancel-manager'),
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
          throw new Error('negative cancel probe discovery returned no characteristic')
        }
        await fixture.controller.perform(
          'queue-operation-completion',
          Object.freeze({ stage: 'write', delayMilliseconds: 10 })
        )
        let callerSettlements = 0
        await fixture.controller.settle(
          connected.database
            .write(characteristic.path, new Uint8Array([91]), { signal: null, deadline: null, mode: 'with-response' })
            .then(
              value => {
                callerSettlements += 1
                return value
              },
              error => {
                callerSettlements += 1
                throw error
              }
            )
        )
        await fixture.controller.perform('advance-time', Object.freeze({ milliseconds: 10 }))
        await fixture.controller.flush()
        expect(callerSettlements).toBe(1)
        expect(
          await fixture.controller.settle(
            rejectsWithCode(
              connected.database.write(
                characteristic.path,
                new Uint8Array([91]),
                { signal: null, deadline: null, mode: 'with-response' }
              ),
              'operation.aborted'
            )
          )
        ).toBe(false)
        const late = manager
          .traces()
          .filter(entry => entry.resource === 'operation')
          .filter(entry => entry.transition === 'late-success' || entry.transition === 'late-failure').length
        expect(late).toBe(0)
        await fixture.controller.settle(connected.connection.release())
      } finally {
        await fixture.controller.settle(manager.destroy())
      }
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('service-change snapshot stays valid when no services-changed arrives', async () => {
    const factory = withSabotagedController(
      createDeterministicBackendTckFactory(),
      dropControllerAction('trigger-services-changed')
    )
    const observation = await runRaceBoundsCleanupVector(factory, 'invalidation.service-change-invalidates-generation')
    expect(observation.holds).toBe(false)
    expect(observation.detail.snapshotInvalidated).toBe(false)
  })

  test('overflow terminal discriminates policy: drop-oldest never terminates', async () => {
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
        require('../../src/manager/ble-manager')
      const { createAttachmentBoundIdFactory, version, versionRange } =
        require('../../src/backend-contract/primitives')
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
          clientId: ids.clientId('tck-negative-overflow-client'),
          managerId: ids.managerId('tck-negative-overflow-manager'),
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
          throw new Error('negative overflow probe discovery returned no characteristic')
        }
        const probe = await fixture.controller.settle(
          connected.database.subscribe(characteristic.path, subscriptionOptions('drop-oldest', 1, 128))
        )
        await fixture.controller.perform(
          'emit-notification',
          notificationInput(characteristic.path, new Uint8Array([4]))
        )
        await fixture.controller.perform(
          'emit-notification',
          notificationInput(characteristic.path, new Uint8Array([5]))
        )
        await fixture.controller.flush()
        const iterator = probe.values[Symbol.asyncIterator]()
        const first = await fixture.controller.settle(iterator.next())
        expect(first.done).toBe(false)
        expect(first.value.kind).not.toBe('terminal')
        await fixture.controller.settle(probe.remove())
        await fixture.controller.settle(connected.connection.release())
      } finally {
        await fixture.controller.settle(manager.destroy())
      }
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('destroy admission check discriminates: a live manager still admits scans', async () => {
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } =
        require('../../src/manager/ble-manager')
      const { createAttachmentBoundIdFactory, version, versionRange } =
        require('../../src/backend-contract/primitives')
      const { rejectsWithCode, scanOptions } = require('../../src/tck/runner-public-scenario-support')
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
          clientId: ids.clientId('tck-negative-admission-client'),
          managerId: ids.managerId('tck-negative-admission-manager'),
          ownerMode: 'owning'
        },
        authority,
        { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => fixture.controller.now() }
      )
      try {
        const liveScan = await fixture.controller.settle(manager.scan(scanOptions(false)))
        expect(await rejectsWithCode(Promise.resolve(liveScan), 'lifecycle.destroyed')).toBe(false)
        expect(await fixture.controller.settle(liveScan.stop())).toEqual({ state: 'released', failures: [] })
        const first = await fixture.controller.settle(manager.destroy())
        const second = await fixture.controller.settle(manager.destroy())
        expect(cleanupRecordsDeepEqual(first, second)).toBe(true)
        expect(
          cleanupRecordsDeepEqual(first, { state: 'release-failed', failures: [{ resourceKind: 'x', error: { code: 'y' } }] })
        ).toBe(false)
      } finally {
        await fixture.controller.settle(manager.destroy().catch(() => ({ state: 'released', failures: [] })))
      }
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('overflow vector asserts exact terminal counts on the valid post-R12 limits', async () => {
    const factory = createDeterministicBackendTckFactory()
    const observation = await runRaceBoundsCleanupVector(factory, 'bounds.subscription-overflow-is-bounded-and-terminal')
    expect(observation.holds).toBe(true)
    expect(observation.detail.exactTerminal).toBe(true)
    expect(observation.detail.oneTerminal).toBe(true)
  })
})
