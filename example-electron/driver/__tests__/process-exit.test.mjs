import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const { shutdownProcessSession } = require('../shutdown.cjs')

test('zero queued data does not prove acknowledged control or prepared-prefix handoff', async () => {
  let status = { queuedData: 0, lastError: null, continuationOutcome: null }
  let claims = 0
  const host = {
    continuation: {
      status: async () => status,
      claim: async () => {
        claims++
      }
    }
  }
  const session = { destroy: async () => ({ state: 'released', outcomes: [] }), allocatedHost: async () => host }
  const blocked = await shutdownProcessSession(session)
  assert.equal(blocked.state, 'release-failed')
  assert.match(blocked.outcomes[0].error.message, /explicit.*claim/)
  assert.equal(claims, 0)
  status = null
  assert.equal((await shutdownProcessSession(session)).state, 'released')
})

test('native cleanup is attempted first and status errors cannot permit exit', async () => {
  const calls = []
  const failure = new Error('native status busy')
  const session = {
    destroy: async () => {
      calls.push('destroy')
      return { state: 'release-failed', outcomes: [{ name: 'native', error: new Error('refused') }] }
    },
    allocatedHost: async () => ({
      continuation: {
        status: async () => {
          calls.push('status')
          throw failure
        }
      }
    })
  }
  const result = await shutdownProcessSession(session)
  assert.equal(result.state, 'release-failed')
  assert.deepEqual(calls, ['destroy', 'status'])
  assert.equal(result.outcomes[1].error, failure)
})

test('never-open session exits without allocating radio or claiming data', async () => {
  const result = await shutdownProcessSession({
    destroy: async () => ({ state: 'released', outcomes: [] }),
    allocatedHost: async () => null
  })
  assert.equal(result.state, 'released')
})
