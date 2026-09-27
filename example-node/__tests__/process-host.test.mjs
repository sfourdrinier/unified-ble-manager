import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createNodeDriverHost } from '../host.ts'

function fixture(overrides = {}, configuration = {}) {
  const calls = []
  const recordings = { list: async () => [] }
  const owner = {
    createManager: async options => {
      calls.push(['child', options])
      return { destroy: async () => ({ state: 'released', failures: [] }) }
    },
    continuation: {
      execute: async declaration => {
        calls.push(['execute', declaration])
        return { event: 'continuation.completed' }
      },
      status: async () => null,
      claim: async () => ({ disposed: true }),
      recordings: async directory => {
        calls.push(['configure', directory])
        return recordings
      }
    },
    destroy: async () => {
      calls.push(['destroy'])
      return { state: 'released', failures: [] }
    },
    ...overrides
  }
  const dependencies = {
    createProcessHost: async (...args) => {
      calls.push(['open', ...args])
      return owner
    },
    openRecordings: async (...args) => {
      calls.push(['offline', ...args])
      return recordings
    }
  }
  const host = createNodeDriverHost('bluez', '/org/bluez/hci1', {
    recordingsDirectory: '/fixture/private',
    dependencies,
    ...configuration
  })
  return { host, owner, calls, recordings, dependencies }
}

test('trusted owner policy is shared by ordinary managers and native continuation without offline radio acquisition', async () => {
  const { host, calls } = fixture({}, { bluezDaemonOwner: ':1.42' })
  await host.nativeContinuation.recordings()
  assert.equal(
    calls.some(([name]) => name === 'open'),
    false
  )
  await host.createManager('ordinary')
  await host.nativeContinuation.execute({ onAppearance: 'native', peerId: 'peer' })
  assert.deepEqual(
    calls.filter(([name]) => name === 'open'),
    [
      [
        'open',
        'bluez',
        {
          adapterId: '/org/bluez/hci1',
          connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }
        }
      ]
    ]
  )
  await host.destroy()
})

test('lazy host coalesces children on one exact selected process owner', async () => {
  const { host, calls } = fixture()
  assert.deepEqual(calls, [])
  await Promise.all([host.createManager('first'), host.createManager('second')])
  assert.deepEqual(
    calls.filter(([name]) => name === 'open'),
    [['open', 'bluez', { adapterId: '/org/bluez/hci1' }]]
  )
  assert.deepEqual(
    calls.filter(([name]) => name === 'child'),
    [
      ['child', { instanceId: 'first' }],
      ['child', { instanceId: 'second' }]
    ]
  )
  assert.deepEqual(await host.destroy(), { state: 'released', failures: [] })
  await assert.rejects(host.createManager('late'), /closed/)
})

test('offline recordings do not open a process radio, and live recording execution configures first', async () => {
  const { host, calls, recordings } = fixture()
  assert.equal(await host.nativeContinuation.recordings(), recordings)
  assert.deepEqual(calls, [['offline', 'bluez', '/fixture/private']])
  const declaration = { onAppearance: 'native', peerId: 'native-peer', recording: { id: 'r' } }
  await host.nativeContinuation.execute(declaration)
  assert.deepEqual(calls.slice(1), [
    ['open', 'bluez', { adapterId: '/org/bluez/hci1' }],
    ['configure', '/fixture/private'],
    ['execute', declaration]
  ])
  await host.destroy()
})

test('failed process release retries the exact owner and never reopens admission', async () => {
  let attempts = 0
  const { host, calls } = fixture({
    destroy: async () => ({ state: ++attempts === 1 ? 'release-failed' : 'released', failures: [] })
  })
  await host.createManager('one')
  assert.equal((await host.destroy()).state, 'release-failed')
  await assert.rejects(host.nativeContinuation.execute({}), /closed/)
  assert.equal((await host.destroy()).state, 'released')
  assert.equal(calls.filter(([name]) => name === 'open').length, 1)
})

test('destroy before lazy use never opens a radio; storage paths are trusted absolute configuration', async () => {
  const { host, calls } = fixture()
  assert.equal(await host.nativeContinuation.status(), null)
  assert.deepEqual(await host.nativeContinuation.claim(), {
    selectors: [],
    values: [],
    streamEnds: [],
    control: [],
    controlLost: 0,
    afterCutoffLoss: { items: 0, bytes: 0 },
    disposed: false,
    disposeFailure: null
  })
  assert.equal((await host.destroy()).state, 'released')
  assert.deepEqual(calls, [])
  assert.throws(() => createNodeDriverHost('bluez', undefined, { recordingsDirectory: '../renderer-path' }), /absolute/)
})

test('allocated owner status and cleanup remain reachable after admission closes', async () => {
  const { host, owner } = fixture()
  let reads = 0,
    claims = 0
  owner.continuation.status = async () => {
    reads++
    return { queuedData: 7, lastError: null, continuationOutcome: null }
  }
  owner.continuation.claim = async () => {
    claims++
    return { disposed: true }
  }
  await host.createManager('first')
  await host.destroy()
  assert.equal((await host.nativeContinuation.status()).queuedData, 7)
  assert.equal((await host.nativeContinuation.claim()).disposed, true)
  assert.equal(reads, 1)
  assert.equal(claims, 1)
})

test('failed initialization retains explicit cleanup retry instead of opening another owner', async () => {
  const { host, dependencies } = fixture()
  let opens = 0,
    retries = 0
  const failure = Object.assign(new Error('initial owner cleanup failed'), {
    retryCleanup: async () => {
      retries++
      return { state: 'released', failures: [] }
    }
  })
  dependencies.createProcessHost = async () => {
    opens++
    throw failure
  }
  await assert.rejects(host.createManager('first'), error => error === failure)
  await assert.rejects(host.createManager('second'), error => error === failure)
  assert.equal((await host.destroy()).state, 'released')
  assert.equal(await host.nativeContinuation.status(), null)
  assert.equal((await host.nativeContinuation.claim()).disposed, false)
  assert.equal(opens, 1)
  assert.equal(retries, 1)
})

test('compensated initialization preserves the caller error without retaining nonexistent ownership', async () => {
  const { host, dependencies, owner, calls } = fixture()
  const failure = new Error('factory refused after confirmed compensation')
  let opens = 0
  dependencies.createProcessHost = async () => {
    if (++opens === 1) throw failure
    return owner
  }
  const requests = [host.createManager('first'), host.createManager('coalesced')]
  await Promise.all(requests.map(request => assert.rejects(request, error => error === failure)))
  assert.equal(opens, 1)
  assert.equal(await host.nativeContinuation.status(), null)
  assert.equal((await host.nativeContinuation.claim()).disposed, false)
  await host.createManager('fresh')
  assert.equal(opens, 2)
  assert.deepEqual(
    calls.filter(([name]) => name === 'child'),
    [['child', { instanceId: 'fresh' }]]
  )
  assert.deepEqual(await host.destroy(), { state: 'released', failures: [] })
})

test('shutdown racing a compensated initialization does not invent cleanup debt', async () => {
  const { host, dependencies } = fixture()
  let refuse
  const failure = new Error('factory already compensated')
  dependencies.createProcessHost = () =>
    new Promise((_resolve, reject) => {
      refuse = reject
    })
  const child = assert.rejects(host.createManager('pending'), error => error === failure)
  await Promise.resolve()
  const shutdown = host.destroy()
  refuse(failure)
  await child
  assert.deepEqual(await shutdown, { state: 'released', failures: [] })
  assert.equal(await host.nativeContinuation.status(), null)
  await assert.rejects(host.createManager('late'), /closed/)
})

test('shutdown waits for late initialization and refuses to publish its requested child', async () => {
  const { host, dependencies, owner, calls } = fixture()
  let complete
  dependencies.createProcessHost = () =>
    new Promise(resolve => {
      complete = resolve
    })
  const child = host.createManager('late')
  const rejected = assert.rejects(child, /closed/)
  await Promise.resolve()
  const shutdown = host.destroy()
  complete(owner)
  await rejected
  assert.equal((await shutdown).state, 'released')
  assert.equal(calls.filter(([name]) => name === 'child').length, 0)
  assert.equal(calls.filter(([name]) => name === 'destroy').length, 1)
})

test('recording configuration failure prevents execution and retains the same process owner for cleanup', async () => {
  const { host, owner, calls } = fixture()
  const refusal = new Error('private directory refused')
  owner.continuation.recordings = async () => {
    throw refusal
  }
  await assert.rejects(host.nativeContinuation.execute({ recording: { id: 'r' } }), error => error === refusal)
  assert.equal(
    calls.some(([name]) => name === 'execute'),
    false
  )
  assert.equal((await host.destroy()).state, 'released')
})
