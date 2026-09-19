'use strict'

// Finding 194: a connect attempt that never settles (its caller moved on —
// the public core settles the deadline without cancelling the backend
// acquisition) must not wedge the peer. The next connect for the same peer,
// same manager or a new one, supersedes the stale acquisition instead of
// failing `connection.already-owned` from `connection.arbitration`.

const {
  observePeer,
  openBackend,
  connectAndDiscover
} = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms))
}

/** Settles with 'pending' when `promise` is still in flight after `ms`. */
async function settleState(promise, ms) {
  let settled = false
  let rejected = null
  promise.then(
    () => {
      settled = true
    },
    error => {
      settled = true
      rejected = error
    }
  )
  await sleep(ms)
  if (!settled) return 'pending'
  if (rejected !== null) throw rejected
  return 'resolved'
}

/** Wait until the synthetic core reports a non-terminal link for `peerId`. */
async function waitForConnecting(stage, nativePeerId = 'peer-1') {
  const deadline = Date.now() + 5000
  for (;;) {
    const peers = await stage.peerRecords()
    const peer = peers.find(entry => entry.peerId === nativePeerId)
    if (peer !== undefined && (peer.connectionState === 'connecting' || peer.connectionState === 'connected')) {
      return peer
    }
    if (Date.now() > deadline) throw new Error(`peer ${nativePeerId} never reached connecting`)
    await sleep(25)
  }
}

describe('finding 194: a stale connect acquisition never wedges its peer', () => {
  test('corebluetooth: retry after an abandoned attempt connects instead of already-owned', async () => {
    const opened = await openBackend('corebluetooth')
    const { backend, stage } = opened
    try {
      const peerId = await observePeer(backend, stage)
      await stage.blockRadioOp('connect')
      try {
        // Attempt 1 stalls on the radio; its caller abandons it (deadline
        // settles above the provider, the backend future stays in flight).
        const first = backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null })
        first.catch(() => undefined)
        await waitForConnecting(stage)
        // Attempt 2, the retry in the next tick: the stale acquisition is
        // superseded, so arbitration must admit it instead of refusing
        // `connection.already-owned`.
        const second = backend.connections.connect(peerId, 'client-2', { signal: null, deadline: null })
        second.catch(() => undefined)
        expect(await settleState(second, 400)).toBe('pending')
        // The superseded attempt ends aborted once its cancellation lands.
        await stage.unblockRadioOp('connect')
        const lease = await second
        expect(lease.connection.connectionId).toEqual(expect.any(String))
        await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
        await lease.release()
      } finally {
        await stage.unblockRadioOp('connect').catch(() => undefined)
      }
    } finally {
      await opened.backend.destroy()
    }
  })

  test('corebluetooth: release then connect in the next tick is admitted', async () => {
    const opened = await openBackend('corebluetooth')
    const { backend, stage } = opened
    try {
      const { peerId, lease } = await connectAndDiscover(backend, stage)
      const cleanup = await lease.release()
      expect(cleanup).toEqual({ state: 'released', failures: [] })
      await sleep(0)
      const retry = await backend.connections.connect(peerId, 'client-2', { signal: null, deadline: null })
      expect(retry.connection.connectionId).toEqual(expect.any(String))
      await retry.release()
    } finally {
      await opened.backend.destroy()
    }
  })

  test('corebluetooth: ownership never leaks across managers in one process', async () => {
    const first = await openBackend('corebluetooth')
    const second = await openBackend('corebluetooth')
    try {
      const peerA = await observePeer(first.backend, first.stage)
      const peerB = await observePeer(second.backend, second.stage)
      const leaseA = await first.backend.connections.connect(peerA, 'client-a', { signal: null, deadline: null })
      await expect(
        first.backend.connections.connect(peerA, 'client-a2', { signal: null, deadline: null })
      ).rejects.toMatchObject({ normalized: { code: 'connection.already-owned' } })
      await leaseA.release()
      // A second manager over its own central is unaffected by the first.
      const leaseB = await second.backend.connections.connect(peerB, 'client-b', { signal: null, deadline: null })
      expect(leaseB.connection.connectionId).toEqual(expect.any(String))
      await leaseB.release()
    } finally {
      await first.backend.destroy()
      await second.backend.destroy()
    }
  })
})
