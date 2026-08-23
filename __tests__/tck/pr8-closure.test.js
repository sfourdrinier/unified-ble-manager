// __tests__/tck/pr8-closure.test.js

const { baseTckScenarios } = require('../../src/tck/scenarios')
const {
  createDeterministicBackendTckFactory,
  deterministicTckFeatureSuites
} = require('../../src/tck/deterministic/deterministic-tck-factory')
const { runBackendTck } = require('../../src/tck/runner')
const {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration
} = require('../../src/backend-contract/capabilities')

const CONNECTION_CONTROLS_SCENARIO_ID = 'connection.rssi-and-att-mtu-capability-contract'
const CONNECTION_CONTROLS_SUITE = Object.freeze({
  suiteId: 'connection-controls',
  scenarioIds: [CONNECTION_CONTROLS_SCENARIO_ID]
})

function scenario(id) {
  const definition = baseTckScenarios.find(candidate => candidate.id === id)
  if (definition === undefined) throw new Error(`missing scenario ${id}`)
  return definition
}

function receipt(report, id) {
  const value = report.receipts.find(candidate => candidate.scenarioId === id)
  if (value === undefined) throw new Error(`missing receipt ${id}`)
  return value
}

function connectionControlRegistration(id, state) {
  const registration = createBackendOperationCapabilityRegistration({
    id,
    implementationVersion: 'pr8-tck-test-fixture',
    sourceDigest: `pr8-tck-${id.replace(':', '-')}`,
    tckSuiteId: CONNECTION_CONTROLS_SUITE.suiteId,
    requiredScenarioIds: [CONNECTION_CONTROLS_SCENARIO_ID],
    ...(state === 'supported'
      ? {
          limitations: []
        }
      : {})
  })
  return {
    ...registration,
    state,
    ...(state === 'supported'
      ? {
          limitations: [],
          evidence: {
            ...registration.evidence,
            evidenceLevel: 'supported',
            limitations: []
          }
        }
      : state === 'limited'
        ? {}
        : {
            evidence: {
              ...registration.evidence,
              evidenceLevel: 'blocked'
            }
          })
  }
}

function controlsFactory(states) {
  return createDeterministicBackendTckFactory({
    backend: {
      featureRegistrations: Object.entries({
        [BUILT_IN_FEATURE_IDS.connectionDirect]: 'limited',
        ...states
      }).map(([id, state]) => connectionControlRegistration(id, state))
    }
  })
}

async function runConnectionControls(states) {
  return runBackendTck(controlsFactory(states), [CONNECTION_CONTROLS_SUITE, ...deterministicTckFeatureSuites])
}

describe('PR8 TCK closure', () => {
  test('declares advanced link-control truth facts on the canonical connection-controls scenario', () => {
    expect(scenario('connection.rssi-and-att-mtu-capability-contract').requiredFacts).toEqual(
      expect.arrayContaining([
        'connection-priority-request-outcome-is-not-observed-parameters',
        'connection-observation-is-bound-to-generation',
        'connection-phy-truth-is-explicit',
        'connection-parameters-truth-is-explicit',
        'connection-subrate-truth-is-explicit',
        'gatt-write-readiness-truth-is-explicit'
      ])
    )
  })

  test('declares scheduler fairness/backpressure and cancellation cleanup facts on deterministic scenarios', () => {
    expect(scenario('gatt.reads-descriptors-write-policy-and-dispatched-cancellation').requiredFacts).toContain(
      'gatt-operation-queue-is-fair-and-bounded'
    )
    expect(scenario('lifecycle.destroy-idempotency-admission-and-exact-settlement').requiredFacts).toContain(
      'operation-cancellation-and-destroy-leave-zero-residual-resources'
    )
  })

  test('deterministic TCK receipts prove every PR8 closure fact without physical-radio labeling', async () => {
    const report = await runBackendTck(createDeterministicBackendTckFactory(), [])
    const scheduler = receipt(report, 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation')
    const lifecycle = receipt(report, 'lifecycle.destroy-idempotency-admission-and-exact-settlement')

    expect(scheduler.proof).toMatchObject({ scope: 'deterministic', claim: 'deterministic-conformance' })
    expect(lifecycle.proof).toMatchObject({ scope: 'deterministic', claim: 'deterministic-conformance' })
    expect(scheduler.facts).toEqual(
      expect.arrayContaining([expect.objectContaining({ id: 'gatt-operation-queue-is-fair-and-bounded', holds: true })])
    )
    expect(lifecycle.facts).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: 'operation-cancellation-and-destroy-leave-zero-residual-resources',
          holds: true
        })
      ])
    )
  })

  test.each(['limited', 'supported'])(
    'does not accept descriptor-only %s PHY, parameter, subrate, or readiness claims',
    async state => {
      await expect(
        runConnectionControls({
          [BUILT_IN_FEATURE_IDS.connectionPhy]: state,
          [BUILT_IN_FEATURE_IDS.connectionParameters]: state,
          [BUILT_IN_FEATURE_IDS.connectionSubrate]: state,
          [BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness]: state
        })
      ).rejects.toThrow('required fact connection-phy-truth-is-explicit did not hold')
    }
  )

  test.each(['limited', 'supported'])('does not accept descriptor-only %s parameter claims', async state => {
    await expect(
      runConnectionControls({
        [BUILT_IN_FEATURE_IDS.connectionPhy]: 'unsupported',
        [BUILT_IN_FEATURE_IDS.connectionParameters]: state,
        [BUILT_IN_FEATURE_IDS.connectionSubrate]: 'unsupported',
        [BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness]: 'unsupported'
      })
    ).rejects.toThrow('required fact connection-parameters-truth-is-explicit did not hold')
  })

  test.each(['limited', 'supported'])('does not accept descriptor-only %s subrate claims', async state => {
    await expect(
      runConnectionControls({
        [BUILT_IN_FEATURE_IDS.connectionPhy]: 'unsupported',
        [BUILT_IN_FEATURE_IDS.connectionParameters]: 'unsupported',
        [BUILT_IN_FEATURE_IDS.connectionSubrate]: state,
        [BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness]: 'unsupported'
      })
    ).rejects.toThrow('required fact connection-subrate-truth-is-explicit did not hold')
  })

  test.each([
    ['unsupported', 'capability.unsupported'],
    ['unavailable', 'capability.unavailable']
  ])('keeps descriptor-only %s parameter and subrate truth explicit', async (state, errorCode) => {
    const report = await runConnectionControls({
      [BUILT_IN_FEATURE_IDS.connectionPhy]: 'unsupported',
      [BUILT_IN_FEATURE_IDS.connectionParameters]: state,
      [BUILT_IN_FEATURE_IDS.connectionSubrate]: state,
      [BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness]: 'unsupported'
    })
    const controls = receipt(report, CONNECTION_CONTROLS_SCENARIO_ID)

    expect(controls.facts).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: 'connection-parameters-truth-is-explicit',
          holds: true,
          detail: expect.objectContaining({
            invoked: false,
            operationAvailable: false,
            expectedError: errorCode
          })
        }),
        expect.objectContaining({
          id: 'connection-subrate-truth-is-explicit',
          holds: true,
          detail: expect.objectContaining({
            invoked: false,
            operationAvailable: false,
            expectedError: errorCode
          })
        })
      ])
    )
  })

  test('invokes and exactly rejects unsupported readiness through the internal handle', async () => {
    const report = await runConnectionControls({
      [BUILT_IN_FEATURE_IDS.connectionPhy]: 'unsupported',
      [BUILT_IN_FEATURE_IDS.connectionParameters]: 'unsupported',
      [BUILT_IN_FEATURE_IDS.connectionSubrate]: 'unsupported',
      [BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness]: 'unsupported'
    })
    const controls = receipt(report, CONNECTION_CONTROLS_SCENARIO_ID)
    expect(controls.facts).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: 'gatt-write-readiness-truth-is-explicit',
          holds: true,
          detail: expect.objectContaining({ invoked: true, errorCode: 'capability.unsupported' })
        })
      ])
    )
  })
})
