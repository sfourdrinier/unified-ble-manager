// __tests__/backends/reactnative/rust-core-exact-cancel.test.js
//
// Finding 109: op.cancel is classified exactly, as legacy's dispatch epoch
// did. Every invoke naming an operation carries the session's next
// `admission`; a cancel names its target's. No bound on remembered
// operations can misclassify a cancel: a late cancel after thousands of
// finished operations is already terminal, and an early cancel still refuses
// its operation after thousands of other early cancels.

const { rustCoreHarness, environment, settle } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

describe('exact cancellation by admission (109)', () => {
  test('the provider stamps strictly increasing admissions in send order', async () => {
    const harness = rustCoreHarness()
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const backend = manager.attachedBackend.backend
    const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const connection = await manager.connect(peerId, NO_OPTIONS)
    const database = await connection.discover(NO_OPTIONS)
    const path = (await database.snapshot()).characteristics[0].path
    await Promise.all([database.read(path, NO_OPTIONS), database.read(path, NO_OPTIONS)])
    const admissions = harness.native.calls
      .filter(call => call[0] === 'invoke')
      .map(call => JSON.parse(call[3]))
      .filter(args => args.admission !== undefined)
      .map(args => args.admission)
    expect(admissions.length).toBeGreaterThanOrEqual(4)
    expect(admissions).toEqual([...admissions].sort((a, b) => a - b))
    expect(new Set(admissions).size).toBe(admissions.length)
    await manager.destroy()
  })

  test('a cancel of a settled operation is already terminal after 5000 finished operations, with no owner call', async () => {
    const harness = rustCoreHarness()
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const backend = manager.attachedBackend.backend
    const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    const connection = await manager.connect(peerId, NO_OPTIONS)
    const database = await connection.discover(NO_OPTIONS)
    const path = (await database.snapshot()).characteristics[0].path
    const first = backend.gatt.read(path, { operation: { ...NO_OPTIONS, correlation: 'first' } })
    await first.completion
    for (let index = 0; index < 5000; index += 1) {
      await backend.gatt.read(path, { operation: { ...NO_OPTIONS, correlation: `r-${index}` } }).completion
    }
    const cancels = harness.native.opsInvoked('op.cancel').length
    expect((await first.requestCancellation()).state).toBe('already-terminal')
    expect(harness.native.opsInvoked('op.cancel')).toHaveLength(cancels)
    await manager.destroy()
  })

  test('the owner answers a late cancel by admission after 5000 finished operations: already terminal', async () => {
    const harness = rustCoreHarness()
    const session = await harness.binding.openSession('exact-cancel')
    for (let admission = 1; admission <= 5001; admission += 1) {
      await session.invoke('peers.bonded', { operationId: `b-${admission}`, admission })
    }
    expect(await session.invoke('op.cancel', { operationId: 'b-1', admission: 1 })).toEqual({
      state: 'already-terminal'
    })
    await session.invoke('session.dispose', {})
    await session.close()
  })

  test('an early cancel refuses its operation after 5000 other early cancels', async () => {
    const harness = rustCoreHarness()
    const session = await harness.binding.openSession('exact-cancel')
    expect(await session.invoke('op.cancel', { operationId: 'early', admission: 1 })).toEqual({
      state: 'cancellation-requested'
    })
    for (let admission = 2; admission <= 5002; admission += 1) {
      await session.invoke('op.cancel', { operationId: `t-${admission}`, admission })
    }
    await expect(session.invoke('peers.bonded', { operationId: 'early', admission: 1 })).rejects.toThrow(
      'operation.aborted'
    )
    await settle()
    await session.invoke('session.dispose', {})
    await session.close()
  })
})
