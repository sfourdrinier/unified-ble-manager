import { after, before, test } from 'node:test'
import assert from 'node:assert/strict'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

let server, readBatteryLevel, createBatteryProof, runBatteryProofButton
before(async () => {
  server = await createServer({
    configFile: fileURLToPath(new URL('../vite.config.mts', import.meta.url)),
    logLevel: 'silent',
    server: { middlewareMode: true, hmr: false, watch: null }
  })
  ;({ readBatteryLevel } = await server.ssrLoadModule('/src/battery.ts'))
  ;({ createBatteryProof, runBatteryProofButton } = await server.ssrLoadModule('/src/proof.ts'))
})
after(() => server?.close())

const released = { state: 'released', failures: [] }
function fixture(fail = {}) {
  const calls = []
  const peer = { id: 'observed-peer' }
  const clean = name => async () => {
    calls.push(name)
    if (fail[name] instanceof Error) throw fail[name]
    return fail[name] ?? released
  }
  const gatt = { characteristic: () => ({ read: async () => new Uint8Array([82]) }) }
  const connection = { discover: async () => gatt, release: clean('connection') }
  const scan = {
    observations: {
      [Symbol.asyncIterator]: () => ({ next: async () => ({ done: false, value: { kind: 'value', value: { peer } } }) })
    },
    stop: clean('scan')
  }
  const manager = {
    adapter: { state: async () => 'powered-on' },
    capabilities: { list: () => [] },
    scan: async () => scan,
    connect: async target => {
      assert.equal(target, peer)
      calls.push('connect')
      return connection
    },
    destroy: clean('manager')
  }
  return { calls, manager, fail }
}

test('actual stream envelope supplies the peer and battery read succeeds', async () => {
  const f = fixture(),
    values = []
  await createBatteryProof(async () => f.manager).run(value => values.push(value))
  assert.deepEqual(f.calls, ['scan', 'connect', 'connection', 'manager'])
  assert.equal(values.includes(82), true)
})

test('factory rejection is reported without invented cleanup', async () => {
  const failure = new Error('factory refused')
  await assert.rejects(
    createBatteryProof(async () => {
      throw failure
    }).run(() => {}),
    error => error === failure
  )
  const button = { disabled: false },
    errors = []
  await runBatteryProofButton(
    button,
    createBatteryProof(async () => {
      assert.equal(button.disabled, true)
      throw failure
    }),
    error => errors.push(error)
  )
  assert.equal(button.disabled, false)
  assert.deepEqual(errors, [failure])
})

for (const step of ['scan', 'connection', 'manager']) {
  for (const rejected of [true, false]) {
    test(`${step} ${rejected ? 'rejects' : 'fails receipt'} and remaining cleanup is attempted and retryable`, async () => {
      const f = fixture({
        [step]: rejected ? new Error('held owner') : { state: 'release-failed', failures: [{ error: 'held owner' }] }
      })
      let factories = 0
      const proof = createBatteryProof(async () => {
        factories += 1
        return f.manager
      })
      await assert.rejects(proof.run(() => {}))
      assert.equal(f.calls.includes('manager'), true)
      const previousFactories = factories
      await assert.rejects(proof.run(() => {}))
      assert.equal(factories, previousFactories)
      delete f.fail[step]
      await proof.run(() => {})
      assert.equal(factories, previousFactories + 1)
    })
  }
}

test('battery read forwards selector/options and rejects malformed percentages', async () => {
  const options = { timeoutMs: 123 }
  for (const bytes of [new Uint8Array(), new Uint8Array([101]), new Uint8Array([50, 1])]) {
    const gatt = {
      characteristic: (service, characteristic) => {
        assert.equal(service, '180f')
        assert.equal(characteristic, '2a19')
        return {
          read: async received => {
            assert.equal(received, options)
            return bytes
          }
        }
      }
    }
    await assert.rejects(readBatteryLevel(gatt, options))
  }
})

test('overflow is reported before the next actual peer and source terminal error is preserved', async () => {
  const f = fixture(),
    notices = []
  const original = await f.manager.scan()
  const peerItem = await original.observations[Symbol.asyncIterator]().next()
  const notice = { kind: 'overflow', policy: 'drop-oldest', droppedItems: 1, droppedBytes: 10, replacedItems: 0 }
  const items = [{ done: false, value: notice }, peerItem]
  original.observations[Symbol.asyncIterator] = () => ({ next: async () => items.shift() })
  await createBatteryProof(async () => f.manager).run(value => notices.push(value))
  assert.equal(notices.includes(notice), true)
  const failure = { code: 'permission.denied', domain: 'scan', operation: 'native.scan' }
  original.observations[Symbol.asyncIterator] = () => ({
    next: async () => ({ done: false, value: { kind: 'terminal', reason: 'source-failed', error: failure } })
  })
  await assert.rejects(
    createBatteryProof(async () => f.manager).run(() => {}),
    error => error.errors[0] === failure
  )
})

test('no peer times out and releases every owner without a wall-clock wait', async context => {
  context.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1000 })
  const f = fixture()
  let waiting
  const observed = new Promise(resolve => {
    waiting = resolve
  })
  const scan = await f.manager.scan()
  scan.observations[Symbol.asyncIterator] = () => ({
    next: () => {
      waiting()
      return new Promise(() => {})
    }
  })
  const result = createBatteryProof(async () => f.manager).run(() => {})
  const refused = assert.rejects(result, error => error.errors[0].code === 'operation.timed-out')
  await observed
  context.mock.timers.tick(10000)
  await refused
  assert.deepEqual(f.calls, ['scan', 'manager'])
})
