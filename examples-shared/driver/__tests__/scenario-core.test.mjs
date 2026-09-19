import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  SNAPSHOT_MIN_INTERVAL_MS,
  ScenarioController,
  ScenarioError,
  ScenarioRegistry,
  args,
  defineCommand
} from '../scenario-core.ts'
import { createFakeRuntime } from './fake-runtime.mjs'

class CounterScenario extends ScenarioController {
  id = 'counter'
  title = 'Counter'
  description = 'test double'
  commands = {
    add: defineCommand({
      label: 'Add',
      description: 'adds n',
      presets: [{ label: 'Add 1', args: { n: 1 } }],
      parse: raw => ({ n: args.number(raw, 'n', 1, { min: 0 }) }),
      run: async ({ n }) => {
        this.patch({ total: this.snapshot().total + n })
        return { total: this.snapshot().total }
      }
    }),
    fail: defineCommand({
      label: 'Fail',
      description: 'always fails',
      parse: args.none,
      run: async () => {
        throw Object.assign(new Error('boom'), { code: 'gatt.failed' })
      }
    })
  }

  constructor(runtime) {
    super(runtime, { total: 0 })
  }
}

function collect(target) {
  const updates = []
  target.subscribe(update => updates.push(update))
  return updates
}

test('registry dispatch runs the command, brackets it with events and publishes the snapshot', async () => {
  const runtime = createFakeRuntime('expo/android')
  const registry = new ScenarioRegistry([new CounterScenario(runtime)])
  const updates = collect(registry)
  assert.deepEqual(await registry.dispatch('counter', 'add', { n: 2 }), { total: 2 })
  const kinds = updates.map(update => (update.type === 'event' ? update.event.kind : 'snapshot'))
  assert.deepEqual(kinds, ['command', 'snapshot', 'command-result', 'snapshot'])
  const lastSnapshot = updates.at(-1)
  assert.deepEqual(lastSnapshot.snapshot, { total: 2 })
  assert.equal(updates[0].event.host, 'expo/android')
  assert.deepEqual(updates[0].event.data, { command: 'add', args: { n: 2 } })
})

test('describe lists every command with its presets (what the UI renders as buttons)', () => {
  const registry = new ScenarioRegistry([new CounterScenario(createFakeRuntime())])
  assert.deepEqual(registry.describe()[0].commands, [
    { name: 'add', label: 'Add', description: 'adds n', presets: [{ label: 'Add 1', args: { n: 1 } }], acceptsDevice: false },
    { name: 'fail', label: 'Fail', description: 'always fails', presets: [{ label: 'Fail', args: {} }], acceptsDevice: false }
  ])
})

test('a failing command is reported as command-failed with its code and rethrown', async () => {
  const registry = new ScenarioRegistry([new CounterScenario(createFakeRuntime())])
  const updates = collect(registry)
  await assert.rejects(registry.dispatch('counter', 'fail', {}), { code: 'gatt.failed' })
  const failed = updates.find(update => update.type === 'event' && update.event.kind === 'command-failed')
  assert.deepEqual(failed.event.data, { command: 'fail', error: { code: 'gatt.failed', message: 'boom', detail: null } })
})

test('unknown scenario, unknown command and invalid arguments reject with typed codes', async () => {
  const registry = new ScenarioRegistry([new CounterScenario(createFakeRuntime())])
  await assert.rejects(() => registry.dispatch('nope', 'add', {}), { code: 'scenario.unknown' })
  await assert.rejects(registry.dispatch('counter', 'explode', {}), { code: 'scenario.unknown-command' })
  await assert.rejects(registry.dispatch('counter', 'add', { n: 'two' }), { code: 'scenario.invalid-argument' })
  await assert.rejects(registry.dispatch('counter', 'add', { n: -1 }), { code: 'scenario.invalid-argument' })
  await assert.rejects(registry.dispatch('counter', 'fail', { extra: 1 }), { code: 'scenario.invalid-argument' })
  await assert.rejects(registry.dispatch('counter', 'toString', {}), { code: 'scenario.unknown-command' })
})

test('duplicate scenario ids are refused', () => {
  const runtime = createFakeRuntime()
  assert.throws(() => new ScenarioRegistry([new CounterScenario(runtime), new CounterScenario(runtime)]), ScenarioError)
})

test('snapshot publishing is throttled to one per interval with a trailing publish of the latest state', () => {
  const runtime = createFakeRuntime()
  const scenario = new CounterScenario(runtime)
  const updates = collect(scenario)
  scenario.patch({ total: 1 })
  scenario.patch({ total: 2 })
  scenario.patch({ total: 3 })
  assert.deepEqual(updates.map(update => update.snapshot.total), [1])
  runtime.advance(SNAPSHOT_MIN_INTERVAL_MS)
  assert.deepEqual(updates.map(update => update.snapshot.total), [1, 3])
  assert.equal(runtime.pendingTimers(), 0)
})

test('a throwing listener is logged and does not stop other listeners', async () => {
  const runtime = createFakeRuntime()
  const scenario = new CounterScenario(runtime)
  scenario.subscribe(() => {
    throw new Error('ui crashed')
  })
  const updates = collect(scenario)
  await scenario.dispatch('add', {})
  assert.ok(updates.length > 0)
  assert.ok(runtime.logs.some(entry => entry.message === 'listener threw'))
})

class HoldingScenario extends ScenarioController {
  commands = {}
  title = 'Holding'
  description = 'holds a resource until stopped'

  constructor(runtime, id, { running = true, cleanup = [], throws = null } = {}) {
    super(runtime, {})
    this.id = id
    this.running = running
    this.cleanup = cleanup
    this.throws = throws
    this.stops = 0
  }

  async stop() {
    this.stops += 1
    if (this.throws !== null) throw this.throws
    const wasRunning = this.running
    this.running = false
    return { wasRunning, cleanup: wasRunning ? this.cleanup : [] }
  }
}

test('a plain controller has nothing to stop', async () => {
  assert.deepEqual(await new CounterScenario(createFakeRuntime()).stop(), { wasRunning: false, cleanup: [] })
})

test('stopAll stops every scenario and reports what each released', async () => {
  const runtime = createFakeRuntime()
  const holding = new HoldingScenario(runtime, 'holding', { cleanup: [{ step: 'connection.release', state: 'released', detail: null }] })
  const idle = new HoldingScenario(runtime, 'idle', { running: false })
  const registry = new ScenarioRegistry([holding, idle, new CounterScenario(runtime)])
  const report = await registry.stopAll()
  assert.deepEqual(report, {
    scenarios: [
      { scenario: 'holding', wasRunning: true, cleanup: [{ step: 'connection.release', state: 'released', detail: null }], error: null },
      { scenario: 'idle', wasRunning: false, cleanup: [], error: null },
      { scenario: 'counter', wasRunning: false, cleanup: [], error: null }
    ],
    failures: []
  })
  assert.deepEqual([holding.stops, idle.stops], [1, 1])
})

test('stopAll stops the others when one fails, then rejects with every failure instead of swallowing it', async () => {
  const runtime = createFakeRuntime()
  const leaking = new HoldingScenario(runtime, 'leaking', {
    cleanup: [
      { step: 'subscription.remove', state: 'released', detail: null },
      { step: 'connection.release', state: 'threw', detail: { code: 'platform.failure', message: 'gatt close failed', detail: null } }
    ]
  })
  const throwing = new HoldingScenario(runtime, 'throwing', { throws: Object.assign(new Error('teardown exploded'), { code: 'teardown.bug' }) })
  const fine = new HoldingScenario(runtime, 'fine')
  const registry = new ScenarioRegistry([leaking, throwing, fine])
  const error = await registry.stopAll().then(
    () => assert.fail('stopAll resolved although cleanup failed'),
    rejection => rejection
  )
  assert.ok(error instanceof ScenarioError)
  assert.equal(error.code, 'scenario.stop-all-failed')
  assert.equal(fine.stops, 1)
  assert.deepEqual(error.report.failures, [
    { scenario: 'leaking', step: 'connection.release', state: 'threw', detail: { code: 'platform.failure', message: 'gatt close failed', detail: null } },
    { scenario: 'throwing', step: 'stop', state: 'threw', detail: { code: 'teardown.bug', message: 'teardown exploded', detail: null } }
  ])
  assert.deepEqual(error.cause, error.report)
})
