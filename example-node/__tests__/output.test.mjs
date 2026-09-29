import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Writable } from 'node:stream'
import { flushDriverOutput } from '../output.ts'

function held() {
  let complete
  return {
    stream: new Writable({
      write(_value, _encoding, callback) {
        complete = callback
      }
    }),
    complete: error => complete(error)
  }
}

test('both output completion callbacks are required before shutdown continues', async () => {
  const out = held(),
    err = held()
  let complete = false
  const result = flushDriverOutput(out.stream, err.stream).then(() => {
    complete = true
  })
  out.complete()
  assert.equal(complete, false)
  err.complete()
  await result
  assert.equal(complete, true)
})

test('stream write failure retains its identity', async () => {
  const out = held(),
    err = held(),
    failure = new Error('pipe refused')
  const result = flushDriverOutput(out.stream, err.stream)
  const rejected = assert.rejects(result, error => error === failure)
  out.complete(failure)
  err.complete()
  await rejected
})

test('real Writable callback failure and its subsequent error event are observed', async () => {
  const failure = new Error('actual writable refused')
  const out = new Writable({
    write(_chunk, _encoding, callback) {
      callback(failure)
    }
  })
  const err = new Writable({
    write(_chunk, _encoding, callback) {
      callback()
    }
  })
  await assert.rejects(flushDriverOutput(out, err), error => error === failure)
  await new Promise(resolve => setImmediate(resolve))
  assert.equal(out.listenerCount('error'), 0)
})

test('held output has a finite deadline with no machine-speed verdict', async context => {
  context.mock.timers.enable({ apis: ['setTimeout'] })
  const out = held(),
    err = held()
  const rejected = assert.rejects(flushDriverOutput(out.stream, err.stream, 5000), /shutdown deadline/)
  context.mock.timers.tick(5000)
  await rejected
  out.complete(new Error('late write failure after deadline'))
  err.complete()
  await new Promise(resolve => setImmediate(resolve))
  assert.equal(out.stream.listenerCount('error'), 0)
})
