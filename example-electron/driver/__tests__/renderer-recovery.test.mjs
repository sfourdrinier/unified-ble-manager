import test from 'node:test'
import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const { installRendererRecovery } = require('../renderer-recovery.cjs')

function fixture(load = async () => {}) {
  const window = new EventEmitter()
  window.webContents = new EventEmitter()
  window.isDestroyed = () => false
  const reports = [],
    scheduled = []
  let closing = false,
    loads = 0
  const recovery = installRendererRecovery({
    window,
    load: () => {
      loads++
      return load()
    },
    isShuttingDown: () => closing,
    report: result => reports.push(result),
    schedule: fn => {
      scheduled.push(fn)
      return () => {}
    },
    timeoutMs: 20
  })
  return {
    window,
    reports,
    scheduled,
    recovery,
    resume: () => {
      closing = false
      recovery.resume()
    },
    get loads() {
      return loads
    },
    close: () => {
      closing = true
    },
    crash: reason => window.webContents.emit('render-process-gone', {}, { reason, exitCode: 9 })
  }
}
test('crash during refused shutdown resumes without losing its only automatic attempt', async () => {
  const f = fixture()
  f.close()
  f.crash('crashed')
  assert.equal(f.scheduled.length, 0)
  f.resume()
  assert.equal(f.scheduled.length, 1)
  await f.scheduled.shift()()
  assert.equal(f.loads, 1)
})
test('scheduled recovery deferred by shutdown does not consume the automatic attempt', async () => {
  const f = fixture()
  f.crash('crashed')
  f.close()
  await f.scheduled.shift()()
  assert.equal(f.loads, 0)
  f.resume()
  await f.scheduled.shift()()
  assert.equal(f.loads, 1)
})
test('explicit retry after exhausted automatic recovery loads once and never automatically loops', async () => {
  const f = fixture(async () => {
    throw new Error('refused')
  })
  f.crash('crashed')
  await f.scheduled.shift()()
  assert.equal(f.recovery.retry(), true)
  assert.equal(f.recovery.retry(), false)
  await f.scheduled.shift()()
  assert.equal(f.loads, 2)
  assert.equal(f.scheduled.length, 0)
})
test('unexpected crash reloads once, deferred, on the same window without ownership operations', async () => {
  const f = fixture()
  f.crash('killed')
  assert.equal(f.loads, 0)
  assert.equal(f.scheduled.length, 1)
  await f.scheduled.shift()()
  assert.equal(f.loads, 1)
  assert.equal(f.reports.at(-1).state, 'reloaded')
  f.crash('crashed')
  assert.equal(f.scheduled.length, 0)
  assert.equal(f.reports.at(-1).state, 'failed')
})
test('clean exit and intentional shutdown never restart a renderer', async () => {
  const f = fixture()
  f.crash('clean-exit')
  assert.equal(f.scheduled.length, 0)
  f.crash('crashed')
  f.close()
  await f.scheduled.shift()()
  assert.equal(f.loads, 0)
  f.crash('killed')
  assert.equal(f.scheduled.length, 0)
})
test('failed load keeps original error and cannot crash-loop', async () => {
  const error = new Error('load refused')
  const f = fixture(async () => {
    throw error
  })
  f.crash('crashed')
  await f.scheduled.shift()()
  assert.equal(f.reports.at(-1).error, error)
  assert.equal(f.reports.at(-1).state, 'failed')
  f.crash('crashed')
  assert.equal(f.loads, 1)
  assert.equal(f.scheduled.length, 0)
})
test('second crash during load cannot be overwritten by late successful load', async () => {
  let finish, entered
  const started = new Promise(resolve => {
    entered = resolve
  })
  const f = fixture(
    () =>
      new Promise(resolve => {
        finish = resolve
        entered()
      })
  )
  f.crash('crashed')
  const pending = f.scheduled.shift()()
  await started
  f.crash('oom')
  finish()
  await pending
  assert.equal(f.loads, 1)
  assert.equal(f.reports.at(-1).state, 'failed')
  assert.equal(
    f.reports.some(r => r.state === 'reloaded'),
    false
  )
})
test('held load is bounded; late completion never reports recovered', async () => {
  let finish
  const f = fixture(
    () =>
      new Promise(resolve => {
        finish = resolve
      })
  )
  f.crash('crashed')
  await f.scheduled.shift()()
  assert.equal(f.reports.at(-1).state, 'failed')
  assert.match(f.reports.at(-1).error.message, /deadline/)
  finish()
  await Promise.resolve()
  assert.equal(
    f.reports.some(r => r.state === 'reloaded'),
    false
  )
})
test('closed window cancels scheduled recovery and retires its listener', async () => {
  const f = fixture()
  f.crash('crashed')
  f.window.emit('closed')
  await f.scheduled.shift()()
  assert.equal(f.loads, 0)
  assert.equal(f.window.webContents.listenerCount('render-process-gone'), 0)
})
test('closed BrowserWindow may throw from webContents getter; disposal uses stable sender and is idempotent', () => {
  const window = new EventEmitter(),
    sender = new EventEmitter()
  let destroyed = false
  Object.defineProperty(window, 'webContents', {
    get() {
      if (destroyed) throw new TypeError('Object has been destroyed')
      return sender
    }
  })
  window.isDestroyed = () => destroyed
  const recovery = installRendererRecovery({ window, load: async () => {}, isShuttingDown: () => false, report() {} })
  destroyed = true
  assert.doesNotThrow(() => window.emit('closed'))
  assert.doesNotThrow(() => recovery.dispose())
  assert.equal(sender.listenerCount('render-process-gone'), 0)
  assert.equal(window.listenerCount('closed'), 0)
})
