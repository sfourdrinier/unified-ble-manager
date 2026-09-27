import { test } from 'node:test'
import assert from 'node:assert/strict'
import { stopNodeDriverScenarios, shutdownNodeDriver, createNodeShutdownHandler } from '../cleanup.ts'
import { ScenarioRegistry } from '../../examples-shared/driver/scenario-core.ts'

test('CLI refuses a resolved failed cleanup receipt using the actual registry', async () => {
  const outcome = { wasRunning: true, cleanup: [{ step: 'manager', state: 'release-failed', detail: {} }] }
  const registry = new ScenarioRegistry([{ id: 'failed', dispatch: async () => outcome, stop: async () => outcome }])
  await assert.rejects(stopNodeDriverScenarios(registry, () => {}))
})

test('CLI cleanup tries every scenario but refuses success after any stop rejection', async () => {
  const visited = [],
    reports = []
  const registry = new ScenarioRegistry(
    ['first', 'broken', 'last'].map(id => ({
      id,
      stop: async () => {
        visited.push(id)
        if (id === 'broken') throw new Error('cleanup still owned')
        return { wasRunning: true, cleanup: [] }
      }
    }))
  )
  await assert.rejects(
    stopNodeDriverScenarios(registry, report => reports.push(report)),
    /stopping every scenario/
  )
  assert.deepEqual(visited, ['first', 'broken', 'last'])
  assert.equal(reports.length, 1)
  assert.equal(reports[0].report.scenarios[1].error.message, 'cleanup still owned')
})

test('CLI cleanup succeeds only after all scenario stops settle', async () => {
  const reports = []
  const registry = new ScenarioRegistry([{ id: 'only', stop: async () => ({ wasRunning: true, cleanup: [] }) }])
  await stopNodeDriverScenarios(registry, report => reports.push(report))
  assert.deepEqual(reports, [
    {
      type: 'shutdown-stop-all',
      report: {
        scenarios: [{ scenario: 'only', wasRunning: true, cleanup: [], error: null }],
        failures: []
      }
    }
  ])
})

test('process owner shutdown runs even when scenario cleanup rejects; both failures remain visible', async () => {
  const reports = [],
    calls = []
  await assert.rejects(
    shutdownNodeDriver(
      {
        stopAll: async () => {
          calls.push('scenarios')
          throw new Error('scenario retained')
        }
      },
      {
        destroy: async () => {
          calls.push('host')
          return { state: 'release-failed', failures: [] }
        }
      },
      report => reports.push(report)
    ),
    AggregateError
  )
  assert.deepEqual(calls, ['scenarios', 'host'])
  assert.equal(reports.find(report => report.type === 'shutdown-process-host').receipt.state, 'release-failed')
})

test('process shutdown refuses missing or contradictory confirmation', async () => {
  for (const receipt of [undefined, { state: 'released' }, { state: 'released', failures: ['retained'] }]) {
    await assert.rejects(
      shutdownNodeDriver(
        { stopAll: async () => ({ scenarios: [], failures: [] }) },
        { destroy: async () => receipt },
        () => {}
      )
    )
  }
})

test('process shutdown reports its confirmed release after all scenarios', async () => {
  const reports = []
  await shutdownNodeDriver(
    { stopAll: async () => ({ scenarios: [], failures: [] }) },
    { destroy: async () => ({ state: 'released', failures: [] }) },
    report => reports.push(report)
  )
  assert.deepEqual(
    reports.map(report => report.type),
    ['shutdown-stop-all', 'shutdown-process-host']
  )
})

test('server coalesces concurrent shutdown and retains failed cleanup for the next signal', async () => {
  let finish,
    attempts = 0,
    exits = 0
  const failures = []
  const shutdown = createNodeShutdownHandler({
    cleanup: async () => {
      attempts++
      if (attempts === 1)
        await new Promise((_, reject) => {
          finish = reject
        })
    },
    released: async () => {
      exits++
    },
    failed: async error => {
      failures.push(error)
    }
  })
  const first = shutdown()
  const overlap = shutdown()
  assert.equal(attempts, 1)
  const failure = new Error('retained native session')
  finish(failure)
  await Promise.all([first, overlap])
  assert.equal(exits, 0)
  assert.deepEqual(failures, [failure])
  await shutdown()
  assert.equal(attempts, 2)
  assert.equal(exits, 1)
})
