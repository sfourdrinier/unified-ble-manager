import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const { shutdownOwners } = require('../shutdown.cjs')

test('shutdown attempts every owner and keeps rejected and refused cleanup visible', async () => {
  const calls = []
  const failure = new Error('binding cleanup rejected')
  const result = await shutdownOwners([
    {
      name: 'binding',
      destroy: async () => {
        calls.push('binding')
        throw failure
      }
    },
    {
      name: 'manager',
      destroy: async () => {
        calls.push('manager')
        return { state: 'release-failed', failures: ['held'] }
      }
    }
  ])
  assert.deepEqual(calls, ['binding', 'manager'])
  assert.equal(result.state, 'release-failed')
  assert.equal(result.outcomes[0].error, failure)
  assert.equal(result.outcomes[1].receipt.state, 'release-failed')
})

test('a retry uses the same owners and only succeeds on confirmed release', async () => {
  let attempts = 0
  const owner = {
    name: 'host',
    destroy: async () => ({ state: ++attempts === 1 ? 'release-failed' : 'released', failures: [] })
  }
  assert.equal((await shutdownOwners([owner])).state, 'release-failed')
  assert.equal((await shutdownOwners([owner])).state, 'released')
  assert.equal(attempts, 2)
})

test('missing cleanup confirmation is never successful shutdown', async () => {
  const result = await shutdownOwners([{ name: 'host', destroy: async () => undefined }])
  assert.equal(result.state, 'release-failed')
  assert.match(result.outcomes[0].error.message, /cleanup receipt/)
})

for (const receipt of [{ state: 'released' }, { state: 'released', failures: ['still-owned'] }]) {
  test(`invalid released receipt cannot permit exit: ${JSON.stringify(receipt)}`, async () => {
    let nextAttempted = false
    const result = await shutdownOwners([
      { name: 'invalid', destroy: async () => receipt },
      {
        name: 'next',
        destroy: async () => {
          nextAttempted = true
          return { state: 'released', failures: [] }
        }
      }
    ])
    assert.equal(result.state, 'release-failed')
    assert.ok(result.outcomes[0].error instanceof Error)
    assert.equal(nextAttempted, true)
  })
}

// Actual main overlap/refusal/retry coverage lives in main-lazy.test.mjs,
// together with the process-data handoff and non-allocating startup checks.
