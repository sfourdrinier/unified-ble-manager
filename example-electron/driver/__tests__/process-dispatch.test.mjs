import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const { createProcessDispatch } = require('../process-dispatch.cjs')

function fixture() {
  const calls = []
  const access = Object.fromEntries(
    [
      'execute',
      'describeBacklog',
      'prepareClaim',
      'acknowledgeClaim',
      'status',
      'prepare',
      'acknowledge',
      'stop',
      'clear'
    ].map(name => [
      name,
      async function (...args) {
        assert.equal(this, access)
        calls.push([name, ...args])
        return 'canonical-envelope'
      }
    ])
  )
  return { calls, dispatch: createProcessDispatch({ controls: async () => access, recordings: async () => access }) }
}

test('dispatch preserves raw envelopes and receivers, with prepare separate from ACK', async () => {
  const f = fixture()
  assert.equal(
    await f.dispatch({ operation: 'prepare-claim', args: { maxItems: 2, maxBytes: 100 } }),
    'canonical-envelope'
  )
  assert.deepEqual(f.calls, [['prepareClaim', 2, 100]])
  await f.dispatch({ operation: 'acknowledge-claim', args: { token: 'prefix' } })
  await f.dispatch({ operation: 'recording-prepare', args: { id: 'journal', maxItems: 3, maxBytes: 200 } })
  assert.deepEqual(f.calls.slice(1), [
    ['acknowledgeClaim', 'prefix'],
    ['prepare', 'journal', 3, 200]
  ])
})

test('offline recording operations never acquire process controls', async () => {
  let acquired = 0
  const dispatch = createProcessDispatch({
    controls: async () => {
      acquired++
      throw new Error('radio must stay closed')
    },
    recordings: async () => ({ status: async id => id })
  })
  assert.equal(await dispatch({ operation: 'recording-status', args: { id: 'retained' } }), 'retained')
  assert.equal(acquired, 0)
})

test('unknown, surplus, path, and malformed arguments fail before authority acquisition', async () => {
  let acquired = 0
  const acquire = async () => {
    acquired++
    throw new Error('unexpected acquisition')
  }
  const dispatch = createProcessDispatch({ controls: acquire, recordings: acquire })
  for (const request of [
    { operation: 'configure-directory', args: { directory: '/tmp' } },
    { operation: 'status', args: { extra: true } },
    { operation: 'execute', args: { peerId: '', declarationJson: '{}' } },
    { operation: 'prepare-claim', args: { maxItems: 1.5, maxBytes: 1 } },
    { operation: 'prepare-claim', args: { maxItems: 4294967297, maxBytes: 1 } },
    { operation: 'recording-prepare', args: { id: 'a', maxItems: 1, maxBytes: Number.MAX_SAFE_INTEGER } },
    { operation: 'recording-clear', args: { id: 'a', directory: '/tmp' } },
    { operation: 'recording-acknowledge', args: { id: 'a', token: '' } }
  ])
    await assert.rejects(dispatch(request), /arguments refused/)
  assert.equal(acquired, 0)
})
