const { stoppedIngress } = require('../scripts/ci/recording-retirement-acceptance')

test('late ingress is witnessed by observer closure before handoff, not a sleep', async () => {
  const calls = []
  let finish
  let reads = 0
  const execution = new Promise(resolve => {
    finish = resolve
  })
  const ctx = {
    central: {
      continuationExecute: async () => {
        calls.push('execute')
        return execution
      },
      stagedGattAccesses: async () => (++reads === 1 ? [] : [{ kind: 'write-with-response' }])
    },
    store: {
      stop: async () => {
        calls.push('stop')
        return JSON.stringify({ ok: true, value: { phase: 'stopped' } })
      }
    },
    declaration: () => '{}',
    stage: async () => {
      calls.push('ingress')
      finish(
        JSON.stringify({
          ok: false,
          error: { code: 'stream.closed', detail: 'setup acknowledgement observation closed' }
        })
      )
    }
  }
  await stoppedIngress(ctx, 'r')
  expect(calls).toEqual(['execute', 'stop', 'ingress'])
})

test('a setup timeout cannot masquerade as stopped ingress', async () => {
  let reads = 0
  const ctx = {
    central: {
      continuationExecute: async () => JSON.stringify({ ok: false, error: { code: 'operation.timed-out' } }),
      stagedGattAccesses: async () => (++reads === 1 ? [] : [{ kind: 'write-with-response' }])
    },
    store: { stop: async () => JSON.stringify({ ok: true, value: {} }) },
    declaration: () => '{}',
    stage: async () => {}
  }
  await expect(stoppedIngress(ctx, 'r')).rejects.toThrow()
})
