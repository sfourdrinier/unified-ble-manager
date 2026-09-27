import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const { createProcessSession } = require('../process-session.cjs')
const released = () => ({ state: 'released', failures: [] })
const deferred = () => {
  let resolve
  const promise = new Promise(done => {
    resolve = done
  })
  return { promise, resolve }
}

function fixture() {
  const calls = []
  const manager = {
    destroy: async () => {
      calls.push('manager.destroy')
      return released()
    }
  }
  const binding = {
    invoke: async (...args) => args,
    destroy: async () => {
      calls.push('binding.destroy')
      return released()
    }
  }
  const host = {
    createInternalManager: async () => {
      calls.push('child')
      return manager
    },
    destroy: async () => {
      calls.push('host.destroy')
      return released()
    }
  }
  const factories = {
    createProcessHost: async () => {
      calls.push('open')
      return host
    },
    createBinding: async received => {
      assert.equal(received, manager)
      calls.push('bind')
      return binding
    },
    openRecordings: async () => {
      calls.push('offline')
      return 'recordings'
    }
  }
  return { calls, manager, binding, host, factories, session: createProcessSession(factories) }
}

test('offline access is lazy; simultaneous binding callers share one host and borrower', async () => {
  const f = fixture()
  assert.equal(await f.session.allocatedHost(), null)
  assert.equal(await f.session.recordings(), 'recordings')
  assert.deepEqual(f.calls, ['offline'])
  const bindings = await Promise.all([f.session.binding(), f.session.binding()])
  assert.equal(bindings[0], f.binding)
  assert.equal(bindings[1], f.binding)
  assert.equal(await f.session.processHost(), f.host)
  assert.deepEqual(f.calls, ['offline', 'open', 'child', 'bind'])
  assert.deepEqual(await bindings[0].invoke('event', 'request'), ['event', 'request'])
  assert.equal((await f.session.destroy()).state, 'released')
  assert.deepEqual(f.calls.slice(-3), ['binding.destroy', 'manager.destroy', 'host.destroy'])
  assert.equal(await f.session.allocatedHost(), f.host)
  await assert.rejects(f.session.processHost(), /closed/)
  await assert.rejects(f.session.binding(), /closed/)
})

test('unused shutdown does not open anything; offline storage remains independently accessible', async () => {
  const f = fixture()
  assert.deepEqual(await f.session.destroy(), { state: 'released', outcomes: [] })
  assert.equal(await f.session.recordings(), 'recordings')
  assert.deepEqual(f.calls, ['offline'])
})

test('shutdown coalesces, tries every owner, and retries only retained failure', async () => {
  const f = fixture()
  const fault = new Error('binding refused')
  let attempts = 0
  f.binding.destroy = async () => {
    f.calls.push('binding.destroy')
    if (++attempts === 1) throw fault
    return released()
  }
  await f.session.binding()
  const first = f.session.destroy()
  assert.equal(f.session.destroy(), first)
  const result = await first
  assert.equal(result.state, 'release-failed')
  assert.equal(result.outcomes[0].error, fault)
  assert.deepEqual(f.calls.slice(-3), ['binding.destroy', 'manager.destroy', 'host.destroy'])
  assert.equal((await f.session.destroy()).state, 'released')
  assert.equal(f.calls.filter(call => call === 'open').length, 1)
  assert.equal(f.calls.filter(call => call === 'host.destroy').length, 1)
})

test('late host initialization is awaited, cleaned, and never published after shutdown', async () => {
  const f = fixture(),
    gate = deferred()
  f.factories.createProcessHost = () => gate.promise
  const pending = assert.rejects(f.session.processHost(), /closed/)
  const shutdown = f.session.destroy()
  gate.resolve(f.host)
  await pending
  assert.equal((await shutdown).state, 'released')
  assert.deepEqual(f.calls, ['host.destroy'])
})

test('late binding initialization is awaited and all acquired owners are cleaned', async () => {
  const f = fixture(),
    started = deferred(),
    gate = deferred()
  f.factories.createBinding = () => {
    started.resolve()
    return gate.promise
  }
  const pending = assert.rejects(f.session.binding(), /closed/)
  await started.promise
  const shutdown = f.session.destroy()
  gate.resolve(f.binding)
  await pending
  assert.equal((await shutdown).state, 'released')
  assert.deepEqual(f.calls.slice(-3), ['manager.destroy', 'host.destroy', 'binding.destroy'])
})

test('binding construction failure preserves its cause and its already acquired manager', async () => {
  const f = fixture(),
    fault = new Error('binding constructor refused')
  f.factories.createBinding = () => {
    throw fault
  }
  await assert.rejects(f.session.binding(), error => error === fault)
  assert.equal((await f.session.destroy()).state, 'released')
  assert.deepEqual(f.calls.slice(-2), ['manager.destroy', 'host.destroy'])
})

test('ordinary compensated initialization can retry, retained initialization cannot reopen', async () => {
  const f = fixture(),
    fault = new Error('compensated')
  f.factories.createProcessHost = async () => {
    throw fault
  }
  await assert.rejects(f.session.processHost(), error => error === fault)
  assert.equal(await f.session.allocatedHost(), null)
  let retries = 0
  const retained = Object.assign(new Error('retained'), {
    retryCleanup: async () => {
      retries++
      return released()
    }
  })
  f.factories.createProcessHost = async () => {
    throw retained
  }
  await assert.rejects(f.session.processHost(), error => error === retained)
  f.factories.createProcessHost = async () => {
    throw new Error('must not reopen')
  }
  await assert.rejects(f.session.processHost(), error => error === retained)
  assert.equal((await f.session.destroy()).state, 'released')
  assert.equal(retries, 1)
  assert.equal(await f.session.allocatedHost(), null)
})

test('binding admission may retry an ordinary failed host initialization before any borrower existed', async () => {
  const f = fixture(),
    fault = new Error('no allocated host')
  f.factories.createProcessHost = async () => {
    throw fault
  }
  await assert.rejects(f.session.binding(), error => error === fault)
  f.factories.createProcessHost = async () => f.host
  assert.equal(await f.session.binding(), f.binding)
  assert.equal((await f.session.destroy()).state, 'released')
})

test('a late borrower is retained and released without constructing or publishing a binding', async () => {
  const f = fixture(),
    started = deferred(),
    gate = deferred()
  f.host.createInternalManager = () => {
    started.resolve()
    return gate.promise
  }
  const pending = assert.rejects(f.session.binding(), /closed/)
  await started.promise
  const shutdown = f.session.destroy()
  gate.resolve(f.manager)
  await pending
  assert.equal((await shutdown).state, 'released')
  assert.deepEqual(f.calls, ['open', 'host.destroy', 'manager.destroy'])
})

test('partial binding failure retains explicit retry and still attempts manager and host cleanup', async () => {
  const f = fixture()
  let retries = 0
  const fault = Object.assign(new Error('partial binding retained'), {
    retryCleanup: async () => {
      retries++
      return retries === 1 ? { state: 'release-failed', failures: [{ reason: 'retained' }] } : released()
    }
  })
  f.factories.createBinding = () => {
    throw fault
  }
  await assert.rejects(f.session.binding(), error => error === fault)
  const result = await f.session.destroy()
  assert.equal(result.state, 'release-failed')
  assert.equal(result.outcomes[0].name, 'binding.initialization')
  assert.deepEqual(f.calls.slice(-2), ['manager.destroy', 'host.destroy'])
  assert.equal((await f.session.destroy()).state, 'released')
  assert.equal(retries, 2)
})

for (const stage of ['borrower', 'binding']) {
  test(`shutdown enters the known native owner while ${stage} initialization is held`, async () => {
    const f = fixture(),
      started = deferred(),
      gate = deferred()
    let closeEntered = false
    f.host.destroy = async () => {
      closeEntered = true
      f.calls.push('host.destroy')
      return released()
    }
    if (stage === 'borrower')
      f.host.createInternalManager = () => {
        started.resolve()
        return gate.promise
      }
    else
      f.factories.createBinding = () => {
        started.resolve()
        return gate.promise
      }
    const pending = assert.rejects(f.session.binding(), /closed/)
    await started.promise
    const shutdown = f.session.destroy()
    try {
      assert.equal(closeEntered, true)
    } finally {
      gate.resolve(stage === 'borrower' ? f.manager : f.binding)
      await pending
      assert.equal((await shutdown).state, 'released')
    }
    assert.equal(f.calls.filter(call => call === 'host.destroy').length, 1)
    assert.equal(f.calls.filter(call => call === 'manager.destroy').length, 1)
    assert.equal(f.calls.filter(call => call === 'binding.destroy').length, stage === 'binding' ? 1 : 0)
  })
}
