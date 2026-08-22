// __tests__/tck/pr8-closure.test.js

const { baseTckScenarios } = require('../../src/tck/scenarios')
const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const { runBackendTck } = require('../../src/tck/runner')

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
      expect.arrayContaining([
        expect.objectContaining({ id: 'gatt-operation-queue-is-fair-and-bounded', holds: true })
      ])
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
})
