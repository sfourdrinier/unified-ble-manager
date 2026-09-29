const { createNativeContinuationControl } = require('../src/core/native-continuation-control')
const { createReferenceNativeContinuation } = require('../example-expo/src/driver/native-continuation')
const { ProcessContinuationScenario } = require('../examples-shared/driver/scenarios/process-continuation')
const golden = require('../crates/ubm-mobile/golden/wire-vectors.json')
const ok = value => JSON.stringify({ ok: true, value })
const nativeCounters = JSON.parse(golden.invokes.find(row => row.op === 'counters.describe').envelope).value
const status = () => ({ ...structuredClone(nativeCounters), continuationOutcome: null })
const emptyClaim = {
  consumerCount: 0,
  selectors: [],
  batches: [],
  disposed: false,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: null
}

function controller(invoke) {
  return createReferenceNativeContinuation(
    { invoke },
    async () => {},
    createNativeContinuationControl,
    () => ({})
  )
}

test('mobile app status preserves actual native counter shape without inventing desktop facts', async () => {
  const value = status()
  value.counters.retainedByteBuffers = 7
  const control = controller(async () => ok(value))
  expect(await control.status()).toEqual(value)
  expect(await control.status()).not.toHaveProperty('lastError')
  expect(await control.status()).not.toHaveProperty('queuedData')
})

test('fresh scenario stop preflight reads mobile status then hands off the surviving owner', async () => {
  const calls = []
  let owned = true
  const control = controller(async operation => {
    calls.push(operation)
    if (operation === 'status') return ok(owned ? status() : null)
    if (operation === 'prepare') {
      owned = false
      return ok(emptyClaim)
    }
    throw new Error(`unexpected ${operation}`)
  })
  const runtime = { now: () => 1000, log: () => {}, host: 'test', schedule: () => () => {} }
  const scenario = new ProcessContinuationScenario({ runtime, nativeContinuation: control })
  const result = await scenario.stop()
  expect(result.cleanup[0].state).toBe('released')
  expect(calls).toEqual(['status', 'prepare', 'status'])
  expect(scenario.snapshot().owned).toBe(false)
})

test.each(['unknown', 'counter', 'desktop'])('mobile status fails closed for %s mismatch', async kind => {
  const value = status()
  if (kind === 'unknown') value.unexpected = true
  if (kind === 'counter') value.counters.retainedByteBuffers = -1
  const response = kind === 'desktop' ? { queuedData: 0, lastError: null, continuationOutcome: null } : value
  await expect(controller(async () => ok(response)).status()).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test.each(['aa:bb:cc:dd:ee:ff', '9828347e-45df-2eeb-e928-6e443f4065e3'])(
  'mobile execute canonicalizes %s before dispatch and completion comparison',
  async peerId => {
    const control = controller(async (operation, peer, declaration) => {
      expect(operation).toBe('execute')
      expect(peer).toBe(peerId.toUpperCase())
      expect(JSON.parse(declaration).peerId).toBe(peer)
      return ok({
        event: 'continuation.completed',
        strategy: 'native',
        peerAddress: peerId.toUpperCase(),
        resubscribed: 0
      })
    })
    await expect(control.execute({ onAppearance: 'native', peerId, resubscribe: [] })).resolves.toMatchObject({
      peerAddress: peerId.toUpperCase()
    })
  }
)
