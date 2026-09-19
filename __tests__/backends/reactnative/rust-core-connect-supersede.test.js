'use strict'

// __tests__/backends/reactnative/rust-core-connect-supersede.test.js
//
// Finding 194 (React Native route): a connect acquisition the caller
// abandoned — its backend call still in flight, no abort, no deadline — must
// not wedge its peer. A newer connect for the same peer supersedes the stale
// acquisition (cancel plus settle, matching the desktop
// `desktop-connect-acquisition-supersede` behaviour) so arbitration admits
// the retry instead of refusing `connection.already-owned`.

const { rustCoreHarness, environment, settle } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(harness) {
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  return { manager, backend, peerId }
}

describe('finding 194: a stale RN connect acquisition never wedges its peer', () => {
  test('a retry supersedes the abandoned acquisition instead of wedging', async () => {
    const harness = rustCoreHarness()
    const { manager, backend, peerId } = await openManager(harness)
    try {
      harness.native.hold('connection.connect')
      // Attempt 1 stalls on the radio; its caller abandons it (no abort, no
      // deadline — the backend future stays in flight).
      const first = backend.connections.connect(peerId, 'client-1', { ...NO_OPTIONS })
      first.catch(() => undefined)
      await settle()
      // Attempt 2, the retry in the next tick: the stale acquisition is
      // superseded, so the retry is admitted.
      const second = backend.connections.connect(peerId, 'client-2', { ...NO_OPTIONS })
      second.catch(() => undefined)
      await settle()
      // The superseded attempt ends aborted once its cancellation lands.
      await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      expect(harness.native.opsInvoked('op.cancel').length).toBeGreaterThanOrEqual(1)
      harness.native.release('connection.connect')
      const lease = await second
      expect(lease.connection.connectionId).toEqual(expect.any(String))
      await lease.release()
    } finally {
      harness.native.release('connection.connect')
      await manager.destroy()
    }
  })

  test('a caller abort cancels the native acquisition and the retry connects', async () => {
    const harness = rustCoreHarness()
    const { manager, backend, peerId } = await openManager(harness)
    try {
      harness.native.hold('connection.connect')
      const controller = new AbortController()
      const pending = backend.connections.connect(peerId, 'client-1', { signal: controller.signal, deadline: null })
      pending.catch(() => undefined)
      await settle()
      controller.abort()
      await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      expect(harness.native.opsInvoked('op.cancel').length).toBeGreaterThanOrEqual(1)
      harness.native.release('connection.connect')
      const retry = await backend.connections.connect(peerId, 'client-2', { ...NO_OPTIONS })
      expect(retry.connection.connectionId).toEqual(expect.any(String))
      await retry.release()
    } finally {
      harness.native.release('connection.connect')
      await manager.destroy()
    }
  })

  test('the caller deadline reaches the owner as budgetMs', async () => {
    const harness = rustCoreHarness()
    const { manager, backend, peerId } = await openManager(harness)
    try {
      const mark = harness.native.calls.length
      const lease = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: 1050 })
      await lease.release()
      const connects = harness.native
        .callsSince(mark)
        .filter(call => call[0] === 'invoke' && call[2] === 'connection.connect')
        .map(call => JSON.parse(call[3]))
      expect(connects.length).toBe(1)
      expect(connects[0].budgetMs).toBe(50)
    } finally {
      await manager.destroy()
    }
  })
})
