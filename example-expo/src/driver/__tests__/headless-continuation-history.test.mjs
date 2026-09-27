import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createHeadlessHistory, HEADLESS_EVIDENCE_KEY } from '../headless-continuation-history.ts'

function fixture(text = null) {
  const writes = []
  return { writes, history: createHeadlessHistory({
    async getItem(key) { assert.equal(key, HEADLESS_EVIDENCE_KEY); return text },
    async setItem(key, value) { assert.equal(key, HEADLESS_EVIDENCE_KEY); text = value; writes.push(value) }
  }) }
}
const completed = { state: 'completed', peerId: 'AA:BB:CC:DD:EE:FF', observedAtMs: 1234,
  batteryPercent: 73, cleanup: { state: 'released', failures: [] } }
test('headless diagnostic read is bounded, explicit and omits peer/error payloads', async () => {
  const { history, writes } = fixture()
  assert.deepEqual(await history.read(), { records: [], count: 0 })
  await history.save(completed)
  await history.save({ ...completed, state: 'failed', error: { code: 'Error', message: 'secret', detail: null } })
  const read = await history.read()
  assert.equal(read.count, 2)
  assert.deepEqual(read.records[0], { state: 'completed', observedAtMs: 1234, batteryPercent: 73,
    cleanupState: 'released', cleanupFailures: 0, failed: false })
  assert.equal(JSON.stringify(read).includes('secret'), false)
  assert.equal(JSON.stringify(read).includes(completed.peerId), false)
  assert.equal(writes.length, 2)
  for (let index = 0; index < 20; index++) await history.save(completed)
  assert.equal((await history.read()).count, 16)
})
test('malformed entries and histories fail instead of filtering or truncating corruption', async () => {
  for (const value of ['invalid json', '{}', '[null]', JSON.stringify([{ ...completed, batteryPercent: 101 }]),
    JSON.stringify([{ ...completed, cleanup: { state: 'released', failures: [{}] } }]),
    JSON.stringify(Array(17).fill(completed))]) {
    const { history, writes } = fixture(value)
    await assert.rejects(history.read(), /invalid history/)
    await assert.rejects(history.save(completed), /invalid history/)
    assert.equal(writes.length, 0)
  }
})
test('failure summaries retain bounded error identities without messages or platform payloads', async () => {
  const { history } = fixture()
  await history.save({ ...completed, state: 'failed', error: {
    code: 'peer.not-found', message: 'secret address AA:BB:CC:DD:EE:FF',
    detail: { domain: 'connection', operation: 'react-native-rust-core.connection.connect', platform: { secret: 'payload' } }
  } })
  const record = (await history.read()).records[0]
  assert.equal(record.errorCode, 'peer.not-found')
  assert.equal(record.errorDomain, 'connection')
  assert.equal(record.errorOperation, 'react-native-rust-core.connection.connect')
  assert.equal(JSON.stringify(record).includes('secret'), false)
  assert.equal(JSON.stringify(record).includes(completed.peerId), false)
  await history.save({ ...completed, state: 'failed', error: {
    code: completed.peerId, message: 'secret', detail: { operation: 'x'.repeat(129) }
  } })
  const redacted = (await history.read()).records[1]
  assert.equal(redacted.errorIdentityRedacted, true)
  assert.equal(redacted.errorCode, undefined)
  assert.equal(redacted.errorOperation, undefined)
})
test('storage failures remain rejected and read never writes', async () => {
  const failure = new Error('storage unavailable')
  const history = createHeadlessHistory({ async getItem() { throw failure }, async setItem() { assert.fail('unexpected write') } })
  await assert.rejects(history.read(), error => error === failure)
  await assert.rejects(history.save(completed), error => error === failure)
  const writeFailure = createHeadlessHistory({ async getItem() { return null }, async setItem() { throw failure } })
  await assert.rejects(writeFailure.save(completed), error => error === failure)
})
