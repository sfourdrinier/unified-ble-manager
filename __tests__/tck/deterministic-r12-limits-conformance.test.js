// __tests__/tck/deterministic-r12-limits-conformance.test.js
//
// Deterministic-backend R12 conformance slice (UBM 5.0; U7 parity enabler).
//
// Frozen rule (contracts/src/streams.ts validateStreamLimits, R12
// like-with-like): the byte budget is compared against the 64-byte control
// reserve, so byteCapacity <= reservedControlBytes fails closed with
// stream.quota. This slice conforms the reference side: every subscribe
// issued by the deterministic TCK scenarios and the runner-owned
// subscription-overflow scenario must satisfy the frozen validator, and the
// single-value byte-overflow probe must pass via the direct-evidence and
// runner harnesses on the deterministic backend under valid limits with the
// same corpus. The shared corpus (plus the central-manager leg below, which
// runs the same scenario through the public BleManager stack) enables U7
// cross-backend work but does not prove it: no second BackendTckFactory
// exists in this tree, so genuine cross-implementation proof stays pending.

const { validateStreamLimits, RESERVED_CONTROL_BYTES } = require('../../contracts/src/streams')
const { executePublicTckScenario } = require('../../src/tck/runner-public-scenarios')
const deterministicHelpers = require('../../src/tck/deterministic/deterministic-tck-scenario-helpers')
const runnerSupport = require('../../src/tck/runner-public-scenario-support')
const { baseTckScenarios } = require('../../src/tck/scenarios')
const {
  executeDeterministicTckScenarioEvidence
} = require('../../src/tck/deterministic/deterministic-tck-scenarios')
const {
  deterministicLifecycleFacts
} = require('../../src/tck/deterministic/deterministic-tck-lifecycle-diagnostics')
const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { executeRunnerOwnedTckScenario } = require('../../src/tck/runner-observers')

const DETERMINISTIC_SUBSCRIBE_SCENARIO_IDS = Object.freeze([
  'subscription.enable-ready-shared-cccd-and-fanout',
  'subscription.pre-ready-overflow-controls-and-late-quarantine',
  'scenario.scan-connect-discover-read-notify-destroy'
])

const RUNNER_OVERFLOW_SCENARIO_ID = 'subscription.pre-ready-overflow-controls-and-late-quarantine'

function findScenario(id) {
  const definition = baseTckScenarios.find(candidate => candidate.id === id)
  if (definition === undefined) {
    throw new Error(`TCK scenario is missing: ${id}`)
  }
  return definition
}

// Post-R12 valid budgets this slice pins: every scenario subscribe keeps its
// item capacity and overflow policy and moves the byte budget above the
// 64-byte control reserve. The byte-overflow probes share one corpus across
// harnesses: a single 128-byte value against a (4, 128) budget, so the
// overflow is byte-triggered (1 item retained capacity-wise) with exact
// droppedItems/droppedBytes on each side.
const EXPECTED_VALID_SUBSCRIBES = Object.freeze([
  Object.freeze({ overflowPolicy: 'drop-oldest', itemCapacity: 1, byteCapacity: 128 }),
  Object.freeze({ overflowPolicy: 'error', itemCapacity: 1, byteCapacity: 128 }),
  Object.freeze({ overflowPolicy: 'error', itemCapacity: 4, byteCapacity: 128 }),
  Object.freeze({ overflowPolicy: 'drop-oldest', itemCapacity: 4, byteCapacity: 128 }),
  Object.freeze({ overflowPolicy: 'drop-oldest', itemCapacity: 2, byteCapacity: 128 }),
  // Runner aggregate probe: frozen MAX_STREAM_BYTE_CAPACITY (4 MiB) stream
  // budget tying the 4 MiB aggregate quota. At the tie the stream budget
  // trips first (the control reserve squeezes stream-usable below the quota,
  // so the quota projection goes small and passes) with the same exact
  // terminal, so the overflow proof is unchanged. The aggregate-tie test
  // below pins this cause explicitly.
  Object.freeze({ overflowPolicy: 'error', itemCapacity: 10, byteCapacity: 4 * 1024 * 1024 })
])

describe('deterministic-backend R12 limits conformance', () => {
  let recordedSubscribes
  let helpersSpy
  let supportSpy

  beforeEach(() => {
    recordedSubscribes = []
    const helpersOriginal = deterministicHelpers.subscriptionOptions
    const supportOriginal = runnerSupport.subscriptionOptions
    helpersSpy = jest
      .spyOn(deterministicHelpers, 'subscriptionOptions')
      .mockImplementation((overflowPolicy, itemCapacity, byteCapacity) => {
        recordedSubscribes.push({ source: 'deterministic-helpers', overflowPolicy, itemCapacity, byteCapacity })
        return helpersOriginal(overflowPolicy, itemCapacity, byteCapacity)
      })
    supportSpy = jest
      .spyOn(runnerSupport, 'subscriptionOptions')
      .mockImplementation((overflowPolicy, itemCapacity, byteCapacity) => {
        recordedSubscribes.push({ source: 'runner-support', overflowPolicy, itemCapacity, byteCapacity })
        return supportOriginal(overflowPolicy, itemCapacity, byteCapacity)
      })
  })

  afterEach(() => {
    helpersSpy.mockRestore()
    supportSpy.mockRestore()
  })

  test('frozen R12 rule rejects the pre-fix tiny budgets (fail-closed oracle)', () => {
    expect(RESERVED_CONTROL_BYTES).toBe(64)
    for (const byteCapacity of [3, 4, 8, 16, 32, 5 * 1024 * 1024]) {
      expect(() =>
        validateStreamLimits({
          itemCapacity: 1,
          byteCapacity,
          reservedControlCapacity: 1,
          reservedControlBytes: RESERVED_CONTROL_BYTES
        })
      ).toThrow('stream.quota')
    }
    expect(() =>
      validateStreamLimits({
        itemCapacity: 1,
        byteCapacity: 128,
        reservedControlCapacity: 1,
        reservedControlBytes: RESERVED_CONTROL_BYTES
      })
    ).not.toThrow()
    // The conformed aggregate budget sits exactly at the frozen ceiling.
    expect(() =>
      validateStreamLimits({
        itemCapacity: 10,
        byteCapacity: 4 * 1024 * 1024,
        reservedControlCapacity: 1,
        reservedControlBytes: RESERVED_CONTROL_BYTES
      })
    ).not.toThrow()
  })

  test('every deterministic-scenario and runner-overflow subscribe satisfies validateStreamLimits', async () => {
    const factory = createDeterministicBackendTckFactory()
    const provider = factory.provider

    for (const scenarioId of DETERMINISTIC_SUBSCRIBE_SCENARIO_IDS) {
      const fixture = createDeterministicTestBackend()
      try {
        const facts = await executeDeterministicTckScenarioEvidence(fixture, provider, findScenario(scenarioId))
        expect(facts.filter(fact => !fact.holds)).toEqual([])
      } finally {
        fixture.controller.clock.runUntilIdle()
        await fixture.backend.destroy()
      }
    }

    const lifecycleFacts = await deterministicLifecycleFacts()
    expect(lifecycleFacts.filter(fact => !fact.holds)).toEqual([])

    const runnerFactory = createDeterministicBackendTckFactory()
    const runnerFixture = await runnerFactory.create(Object.freeze({ scenarioId: RUNNER_OVERFLOW_SCENARIO_ID }))
    try {
      const runnerFacts = await executeRunnerOwnedTckScenario(
        runnerFactory,
        runnerFixture,
        findScenario(RUNNER_OVERFLOW_SCENARIO_ID)
      )
      expect(runnerFacts.filter(fact => !fact.holds)).toEqual([])
    } finally {
      expect(await runnerFixture.dispose()).toEqual({ state: 'released', failures: [] })
    }

    expect(recordedSubscribes.length).toBeGreaterThan(0)
    for (const subscribe of recordedSubscribes) {
      expect(() =>
        validateStreamLimits({
          itemCapacity: subscribe.itemCapacity,
          byteCapacity: subscribe.byteCapacity,
          reservedControlCapacity: 1,
          reservedControlBytes: RESERVED_CONTROL_BYTES
        })
      ).not.toThrow()
    }
    for (const expected of EXPECTED_VALID_SUBSCRIBES) {
      expect(recordedSubscribes).toContainEqual(expect.objectContaining(expected))
    }
  })

  test('public sharing subscribes use post-R12 valid budgets with the same proof', async () => {
    const { executePublicTckScenario } = require('../../src/tck/runner-public-scenarios')
    const definition = findScenario('subscription.enable-ready-shared-cccd-and-fanout')
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(Object.freeze({ scenarioId: definition.id }))
    let facts
    try {
      facts = await executePublicTckScenario(factory, fixture, definition)
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
    expect(facts.filter(fact => !fact.holds)).toEqual([])
    expect(facts.map(fact => fact.id).sort()).toEqual(
      expect.arrayContaining([
        'subscription-no-value-before-ready',
        'subscription-shares-physical-cccd-with-consumer-refcount',
        'subscription-fanout-is-consumer-isolated'
      ])
    )
    expect(recordedSubscribes.length).toBeGreaterThan(0)
    for (const subscribe of recordedSubscribes) {
      expect(() =>
        validateStreamLimits({
          itemCapacity: subscribe.itemCapacity,
          byteCapacity: subscribe.byteCapacity,
          reservedControlCapacity: 1,
          reservedControlBytes: RESERVED_CONTROL_BYTES
        })
      ).not.toThrow()
    }
    // The two conformed sharing subscribes keep their drop-oldest policy and
    // item capacity and move the byte budget above the 64-byte control
    // reserve; the fanout proof above is unchanged.
    const sharing = recordedSubscribes.filter(
      subscribe =>
        subscribe.source === 'runner-support' &&
        subscribe.overflowPolicy === 'drop-oldest' &&
        subscribe.itemCapacity === 4 &&
        subscribe.byteCapacity === 128
    )
    expect(sharing).toHaveLength(2)
  })

  test('single-value byte-overflow probe passes via direct and runner harnesses on the deterministic backend with the shared corpus', async () => {
    const factory = createDeterministicBackendTckFactory()

    const directFixture = createDeterministicTestBackend()
    let directFacts
    try {
      directFacts = await executeDeterministicTckScenarioEvidence(
        directFixture,
        factory.provider,
        findScenario('subscription.pre-ready-overflow-controls-and-late-quarantine')
      )
    } finally {
      directFixture.controller.clock.runUntilIdle()
      await directFixture.backend.destroy()
    }
    const directOverflow = directFacts.find(
      fact => fact.id === 'subscription-overflow-quota-order-and-one-terminal-are-exact'
    )
    expect(directOverflow).toMatchObject({
      holds: true,
      detail: expect.objectContaining({
        byteCapacityExactTerminal: true,
        byteCapacityOneTerminal: true,
        // Quota-first cause where it genuinely holds: the shrunken-quota
        // aggregate probe trips stream.quota (not stream.overflow).
        aggregateQuotaProbe: true
      })
    })

    const runnerFixture = await factory.create(Object.freeze({ scenarioId: RUNNER_OVERFLOW_SCENARIO_ID }))
    let runnerFacts
    try {
      runnerFacts = await executeRunnerOwnedTckScenario(
        factory,
        runnerFixture,
        findScenario(RUNNER_OVERFLOW_SCENARIO_ID)
      )
    } finally {
      expect(await runnerFixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
    const runnerOverflow = runnerFacts.find(
      fact => fact.id === 'subscription-overflow-quota-order-and-one-terminal-are-exact'
    )
    expect(runnerOverflow).toMatchObject({
      holds: true,
      detail: expect.objectContaining({ byteExact: true, itemExact: true, aggregateExact: true })
    })

    // Same corpus across harnesses: the byte probe drops exactly the shared
    // single value on each side (each exact-terminal already encodes
    // droppedItems 1 / droppedBytes 128), and both sides requested the
    // identical (error, 4, 128) budget.
    expect(directOverflow.detail.byteCapacityExactTerminal).toBe(runnerOverflow.detail.byteExact)
    const byteProbeBudgets = recordedSubscribes.filter(
      subscribe =>
        subscribe.overflowPolicy === 'error' && subscribe.itemCapacity === 4 && subscribe.byteCapacity === 128
    )
    expect(byteProbeBudgets.map(subscribe => subscribe.source).sort()).toEqual(
      expect.arrayContaining(['deterministic-helpers', 'runner-support'])
    )
  })

  test('central-manager leg runs the shared overflow scenario with the same byte-exact terminal', async () => {
    // MEDIUM-1 central leg: the same overflow scenario through the public
    // BleManager (central) stack on its own fixture, asserting the same
    // (1, 128) byte-exact terminal. Same-backend central-stack proof: it
    // shares the deterministic backend implementation (no second
    // BackendTckFactory exists), so this is not cross-implementation proof.
    const factory = createDeterministicBackendTckFactory()
    const centralFixture = await factory.create(
      Object.freeze({ scenarioId: RUNNER_OVERFLOW_SCENARIO_ID })
    )
    try {
      const centralFacts = await executePublicTckScenario(
        factory,
        centralFixture,
        findScenario(RUNNER_OVERFLOW_SCENARIO_ID)
      )
      const centralOverflow = centralFacts.find(
        fact => fact.id === 'subscription-overflow-quota-order-and-one-terminal-are-exact'
      )
      expect(centralOverflow).toMatchObject({
        holds: true,
        detail: expect.objectContaining({ byteExact: true, itemExact: true, aggregateExact: true })
      })
    } finally {
      expect(await centralFixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
    const centralByteProbe = recordedSubscribes.filter(
      subscribe =>
        subscribe.source === 'runner-support' &&
        subscribe.overflowPolicy === 'error' &&
        subscribe.itemCapacity === 4 &&
        subscribe.byteCapacity === 128
    )
    expect(centralByteProbe).toHaveLength(1)
  })

  test('aggregate tie breaks toward the stream budget with the exact terminal (LOW-2)', async () => {
    // LOW-2: at the 4 MiB stream-budget/aggregate-quota tie the stream
    // budget trips first — the control reserve squeezes stream-usable below
    // the quota, so the quota projection goes small and passes while the
    // stream push overflows — with the exact (1, 524288) terminal. Pinning
    // the cause (not just the counts) keeps any admission-path reordering
    // loud: a reorder that let the quota trip first would flip this cause
    // while the counts stayed green.
    const tieFixture = createDeterministicTestBackend({ aggregateStreamByteQuota: 4 * 1024 * 1024 })
    try {
      const connected = await deterministicHelpers.connectAndDiscover(tieFixture, 'subscription-aggregate-tie')
      const characteristic = connected.snapshot.characteristics[0]
      if (characteristic === undefined) {
        throw new Error('aggregate tie probe has no subscribable characteristic')
      }
      const address = deterministicHelpers.characteristicAddress(characteristic.path)
      const subscriptionPromise = connected.database.subscribe(
        characteristic.path,
        deterministicHelpers.subscriptionOptions('error', 10, 4 * 1024 * 1024)
      )
      tieFixture.controller.clock.runUntilIdle()
      const subscription = await subscriptionPromise
      const traceStart = tieFixture.controller.traceSnapshot().length
      for (let index = 0; index < 9; index += 1) {
        tieFixture.controller.emitNotification(address, new Uint8Array(524288))
      }
      const terminal = await deterministicHelpers.nextStreamItem(subscription.values)
      const afterTerminal = await deterministicHelpers.nextStreamItem(subscription.values)
      await deterministicHelpers.drainVirtualClock(tieFixture)
      const removal = subscription.remove()
      await deterministicHelpers.drainVirtualClock(tieFixture)
      await removal
      expect(terminal).toMatchObject({
        kind: 'terminal',
        reason: 'overflow',
        droppedItems: 1,
        droppedBytes: 524288,
        replacedItems: 0
      })
      expect(afterTerminal).toBeNull()
      const tieTraces = tieFixture.controller
        .traceSnapshot()
        .slice(traceStart)
        .filter(entry => entry.kind === 'stream' && entry.event === 'subscription-overflow-terminal')
      expect(tieTraces).toHaveLength(1)
      expect(tieTraces[0].cause).toBe('stream.overflow')
      const counters = tieFixture.backend.resourceCounters()
      expect(Number(counters.subscriptionConsumers)).toBe(0)
      expect(Number(counters.physicalCccdEnablements)).toBe(0)
      await deterministicHelpers.releaseConnection(tieFixture, connected.lease)
    } finally {
      tieFixture.controller.clock.runUntilIdle()
      await tieFixture.backend.destroy()
    }
  })
})
