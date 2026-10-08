const { RustCoreSecurityBackend } = require('../../../src/backends/reactnative/react-native-rust-core-security')
const { contractError } = require('../../../src/backend-contract/errors')
const { settle } = require('../../../test-support/react-native/rust-core-harness')
const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const state = encryption => ({
  bond: 'bonded',
  encryption,
  authentication: 'unsupported',
  secureConnections: 'unsupported',
  pairingPossible: true
})

function heldSource() {
  let resolve
  const pending = new Promise(answer => {
    resolve = answer
  })
  const backend = new RustCoreSecurityBackend({
    now: () => 1000,
    nativePeerId: peer => peer,
    mintOperationId: () => 'probe',
    budget: () => ({}),
    securityState: () => pending,
    watchAbort: () => () => {}
  })
  return { backend, resolve }
}

test('an encryption event wins over an older delayed opening snapshot without inventing authentication', async () => {
  const { backend, resolve } = heldSource()
  const watch = backend.watch('peer')[Symbol.asyncIterator]()
  backend.observe('peer', state('encrypted'))
  const observed = await watch.next()
  expect(observed.value).toMatchObject({
    kind: 'value',
    value: { state: { encryption: 'encrypted', authentication: 'unsupported', secureConnections: 'unsupported' } }
  })
  resolve(state('not-encrypted'))
  await settle()
  backend.close()
  expect((await watch.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
})

test.each(['peer', null])(
  'a typed source failure settles the watch and fences a held opening snapshot: %s',
  async peer => {
    const { backend, resolve } = heldSource()
    const watch = backend.watch('peer')[Symbol.asyncIterator]()
    const waiting = watch.next()
    const error = contractError('platform.failure', 'platform', 'security.state', {
      domain: 'android',
      code: 'ENCRYPTION_CHANGE_FAILED',
      safeMessage: 'HCI status 5',
      metadata: { androidEncryptionStatus: 5 }
    })
    backend.sourceFailed(peer, error)
    expect((await waiting).value).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: { code: 'platform.failure', platform: { metadata: { androidEncryptionStatus: 5 } } }
    })
    resolve(state('encrypted'))
    await settle()
    expect(backend.watchCount()).toBe(0)
    expect((await watch.next()).done).toBe(true)
    backend.close()
  }
)

test.each([false, true])(
  'typed encryption failure crosses the active factory/wire/provider route, including reconciliation: %s',
  async loseRecord => {
    const h = rustCoreHarness({ platform: 'android' })
    const manager = await createReactNativeBleManagerWithEnvironment(environment(h))
    const backend = manager.attachedBackend.backend
    const nativePeer = 'A0:9E:1A:00:00:01'
    const peer = backend.connections.peerFromAddress({ address: nativePeer, addressType: 'public' })
    const watch = manager.securityBackend().watch(peer)[Symbol.asyncIterator]()
    try {
      await settle(60)
      await watch.next()
      h.native.reportSecurity(nativePeer, state('encrypted'))
      await settle(60)
      expect((await watch.next()).value).toMatchObject({ kind: 'value', value: { state: { encryption: 'encrypted' } } })
      const fault = {
        code: 'platform.failure',
        domain: 'platform',
        operation: 'security.state',
        detail: 'controller failure',
        platform: {
          domain: 'android',
          code: 'ENCRYPTION_CHANGE_FAILED',
          message: 'HCI status 5',
          metadata: { androidEncryptionStatus: 5, nativeDomain: 'android.bluetooth.HciEncryptionChange' }
        }
      }
      const pending = watch.next()
      const report = () => h.native.reportSecurityFailure(nativePeer, fault)
      if (loseRecord) h.native.loseControl(report)
      else report()
      await settle(100)
      expect((await pending).value).toMatchObject({
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'platform.failure', platform: { metadata: { androidEncryptionStatus: 5 } } }
      })
      if (loseRecord) expect(h.native.opsInvoked('session.reconcile').length).toBeGreaterThan(0)
    } finally {
      await watch.return()
      await manager.destroy()
    }
  }
)

test.each([false, true])(
  'fresh unchanged security evidence recovers an unattributed failure through reconciliation: %s',
  async loseFailure => {
    const h = rustCoreHarness({ platform: 'android' })
    const manager = await createReactNativeBleManagerWithEnvironment(environment(h))
    const backend = manager.attachedBackend.backend
    const nativePeer = 'A0:9E:1A:00:00:01'
    const peer = backend.connections.peerFromAddress({ address: nativePeer, addressType: 'public' })
    const security = manager.securityBackend()
    const watch = security.watch(peer)[Symbol.asyncIterator]()
    try {
      await settle(60)
      await watch.next()
      h.native.reportSecurity(nativePeer, state('encrypted'))
      await settle(60)
      await watch.next()
      const fault = {
        code: 'permission.denied',
        domain: 'platform',
        operation: 'security.state',
        detail: 'unattributed getter refusal'
      }
      const terminal = watch.next()
      const fail = () => h.native.reportSecurityFailure(null, fault)
      if (loseFailure) h.native.loseControl(fail)
      else fail()
      await settle(100)
      expect((await terminal).value).toMatchObject({
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'permission.denied' }
      })
      // A new explicit request is a recovery boundary even without a native event.
      const before = h.native.opsInvoked('security.state').length
      await expect(security.state(peer, { signal: null, deadline: null })).resolves.toHaveProperty('encryption')
      expect(h.native.opsInvoked('security.state').length).toBe(before + 1)
      h.native.loseControl(() => h.native.reportSecurity(nativePeer, state('encrypted')))
      await settle(100)
      await expect(security.state(peer, { signal: null, deadline: null })).resolves.toHaveProperty('encryption')
      expect((await watch.next()).done).toBe(true)
      expect(h.native.securityFailures.size).toBe(0)
    } finally {
      await watch.return()
      await manager.destroy()
    }
  }
)

test.each(['peer', null])(
  'fresh security probes recover an older scoped failure without an unsolicited event: %s',
  async attribution => {
    let calls = 0
    const backend = new RustCoreSecurityBackend({
      now: () => 1000,
      nativePeerId: peer => peer,
      mintOperationId: () => 'probe',
      budget: () => ({}),
      securityState: async () => {
        calls++
        return state('encrypted')
      },
      watchAbort: () => () => {}
    })
    backend.sourceFailed(attribution, contractError('permission.denied', 'platform', 'security.state'))
    await expect(backend.state('peer', { signal: null, deadline: null })).resolves.toMatchObject({
      encryption: 'encrypted'
    })
    expect(calls).toBe(1)
    const iterator = backend.watch('peer')[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({
      kind: 'value',
      value: { state: { encryption: 'encrypted' } }
    })
    expect(calls).toBe(2)
    backend.close()
  }
)

test('an older successful security probe cannot clear a newer scoped failure', async () => {
  const { backend, resolve } = heldSource()
  backend.sourceFailed('peer', contractError('permission.denied', 'platform', 'older'))
  const probe = backend.state('peer', { signal: null, deadline: null })
  backend.sourceFailed('peer', contractError('platform.failure', 'platform', 'newer'))
  resolve(state('encrypted'))
  await expect(probe).rejects.toMatchObject({ normalized: { operation: 'newer' } })
  backend.close()
})

test('a native retry refusal remains visible, and a closed security source still refuses dispatch', async () => {
  let calls = 0
  const failure = contractError('permission.denied', 'platform', 'native-current')
  const backend = new RustCoreSecurityBackend({
    now: () => 1000,
    nativePeerId: peer => peer,
    mintOperationId: () => 'probe',
    budget: () => ({}),
    securityState: async () => {
      calls++
      throw failure
    },
    watchAbort: () => () => {}
  })
  backend.sourceFailed('peer', contractError('platform.failure', 'platform', 'historical'))
  await expect(backend.state('peer', { signal: null, deadline: null })).rejects.toBe(failure)
  expect(calls).toBe(1)
  backend.close()
  await expect(backend.state('peer', { signal: null, deadline: null })).rejects.toMatchObject({
    normalized: { code: 'lifecycle.destroyed' }
  })
  expect(calls).toBe(1)
})
