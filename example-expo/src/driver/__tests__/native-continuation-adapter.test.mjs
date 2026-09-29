import assert from 'node:assert/strict'
import test from 'node:test'
import { createReferenceNativeContinuation } from '../native-continuation.ts'

test('direct process controls verify each call, preserve receiver and never implicitly declare or ACK', async () => {
  const calls = []
  const native = { async invoke(...args) { assert.equal(this, native); calls.push(args); return 'native-envelope' } }
  let access
  const recordings = {}
  const result = createReferenceNativeContinuation(native, async () => { calls.push('verify') },
    (supplied, context) => { access = supplied; assert.deepEqual(context, { hostDomain: 'ubm-mobile', scope: 'react-native-native', format: 'mobile' }); return { execute() {}, status() {}, claim() {} } },
    () => recordings)
  assert.equal(await access.execute('peer', 'declaration'), 'native-envelope')
  assert.deepEqual(calls, ['verify', ['execute', 'peer', 'declaration', '', 0, 0]])
  calls.length = 0
  await access.prepareClaim(7, 8192)
  assert.deepEqual(calls, ['verify', ['prepare', '', '', '', 7, 8192]])
  assert.equal(await result.recordings(), recordings)
})

test('identity refusal prevents native dispatch and remains the original error', async () => {
  const error = new Error('identity mismatch')
  let access
  createReferenceNativeContinuation({ invoke() { assert.fail('must not dispatch') } }, async () => { throw error },
    supplied => { access = supplied; return { execute() {}, status() {}, claim() {} } }, () => ({}))
  await assert.rejects(access.describeBacklog(), actual => actual === error)
})
