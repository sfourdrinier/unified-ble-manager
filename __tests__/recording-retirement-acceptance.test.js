const {
  stoppedIngress,
  retirementCheckpoint,
  retire,
  pinnedAuthorityCheckpoint
} = require('../scripts/ci/recording-retirement-acceptance')

test('real pinned authority exhaustion refuses admission before missing-file inspection', async () => {
  const store = {
    status: async () =>
      JSON.stringify({
        ok: false,
        error: {
          code: 'platform.failure',
          detail: 'recording authority capacity reached; existing owners retained',
          platform: {
            domain: 'sqlite',
            code: 'storage.busy',
            metadata: { operation: 'journal', storageKind: 'storage.busy' }
          }
        }
      })
  }
  await expect(pinnedAuthorityCheckpoint(store, () => 256)).resolves.toBeUndefined()
  await expect(pinnedAuthorityCheckpoint(store, () => 255)).rejects.toThrow()
})

test('cache checkpoint occurs only after confirmed disposal, clear and empty healthy status', async () => {
  const calls = []
  const wire = value => JSON.stringify({ ok: true, value })
  const ctx = {
    central: {
      continuationPrepareClaim: async () => {
        calls.push('claim')
        return wire({ claimToken: 'claim' })
      },
      continuationAcknowledgeClaim: async () => {
        calls.push('dispose')
        return wire({ disposed: true })
      }
    },
    store: {
      clear: async () => {
        calls.push('clear')
        return wire({})
      },
      status: async () => {
        calls.push('status')
        return wire({ records: 0, bytes: 0, collectionFailure: null })
      }
    }
  }
  await retire(ctx, 'r', async () => {
    calls.push('checkpoint')
    return {}
  })
  expect(calls).toEqual(['claim', 'dispose', 'clear', 'status', 'checkpoint'])
  ctx.central.continuationAcknowledgeClaim = async () => wire({ disposed: false })
  const checkpoint = jest.fn()
  await expect(retire(ctx, 'r', checkpoint)).rejects.toThrow()
  expect(checkpoint).not.toHaveBeenCalled()
})

function missingLookup() {
  return JSON.stringify({
    ok: false,
    error: {
      code: 'platform.failure',
      detail: 'recording file cannot be inspected',
      platform: { domain: 'sqlite', code: 'storage.io', metadata: { operation: 'lookup', storageKind: 'storage.io' } }
    }
  })
}

test('retired-handle checkpoint drives normal maintenance and retains the strict sixteen bound', async () => {
  let count = 17
  const store = {
    status: jest.fn(async id => {
      expect(id).toBe('__maintenance_missing__')
      count = 16
      return missingLookup()
    })
  }
  expect(await retirementCheckpoint({ store, readHandles: () => count })).toEqual({
    before: 17,
    after: 16,
    maintenanceLookups: 1
  })
  expect(store.status).toHaveBeenCalledTimes(1)
})

test('maintenance never swallows an unrelated failure or treats a created recording as expected', async () => {
  await expect(
    retirementCheckpoint({
      store: { status: async () => JSON.stringify({ ok: true, value: {} }) },
      readHandles: () => 17
    })
  ).rejects.toThrow()
  await expect(
    retirementCheckpoint({
      store: {
        status: async () =>
          JSON.stringify({ ok: false, error: { code: 'platform.failure', platform: { code: 'storage.busy' } } })
      },
      readHandles: () => 17
    })
  ).rejects.toThrow()
})

test('genuinely pinned handles fail the bounded checkpoint rather than relaxing its bound', async () => {
  let tick = 0
  await expect(
    retirementCheckpoint({
      store: { status: async () => missingLookup() },
      readHandles: () => 256,
      now: () => ++tick,
      timeoutMs: 3
    })
  ).rejects.toThrow('retired handles 256 exceeds 16')
})

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
