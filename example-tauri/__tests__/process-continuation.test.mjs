import { after, before, test } from 'node:test'
import assert from 'node:assert/strict'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

let server, createTauriProcessContinuation
before(async () => {
  server = await createServer({
    configFile: fileURLToPath(new URL('../vite.config.mts', import.meta.url)),
    logLevel: 'silent',
    server: { middlewareMode: true, hmr: false, watch: null }
  })
  ;({ createTauriProcessContinuation } = await server.ssrLoadModule('/src/process-continuation.ts'))
})
after(() => server?.close())

const selector = {
  serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
  serviceOccurrence: 1,
  characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
  characteristicOccurrence: 1
}
const prepared = {
  claimToken: 'retained',
  consumerCount: 1,
  selectors: [selector],
  batches: [
    JSON.stringify({
      more: false,
      controlLost: 0,
      records: [{ t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: 'AEg=', delivery: 'notification' }]
    })
  ],
  disposed: false,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: null
}

test('Tauri application control decodes before ACK and offline recording does not bootstrap BLE', async () => {
  const calls = []
  let malformed = true
  const control = createTauriProcessContinuation(async (command, { request }) => {
    assert.equal(command, 'reference_process_continuation')
    calls.push(request)
    const value =
      request.operation === 'prepare-claim'
        ? malformed
          ? { ...prepared, batches: ['bad'] }
          : prepared
        : request.operation === 'acknowledge-claim'
          ? { disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null }
          : request.operation === 'recording-prepare'
            ? { token: null, records: [], bytes: 0, more: false }
            : null
    return JSON.stringify({ ok: true, value })
  })
  await assert.rejects(control.claim(), error => error.code === 'protocol.malformed')
  assert.deepEqual(
    calls.map(call => call.operation),
    ['prepare-claim']
  )
  malformed = false
  const result = await control.claim()
  assert.deepEqual([...result.values[0].value], [0, 72])
  assert.equal(result.disposed, true)
  assert.equal(calls[2].args.token, 'retained')
  await (await control.recordings()).prepare('recording', { maxItems: 1, maxBytes: 1024 })
  assert.equal(calls.at(-1).operation, 'recording-prepare')
  assert.equal(JSON.stringify(calls).includes('directory'), false)
})
