// __tests__/ipc/connection-release-lifecycle.test.js
//
// Finding F4: when the app releases an IPC connection (Electron or Tauri —
// both share `IpcConnection`), the renderer's lifecycle stream must deliver
// the final `disconnected` / `requested-disconnect` value event and end with
// the `owner-released` terminal, exactly the vocabulary names the reference
// host delivers. Ending bare makes the supervisor read `stream.closed` and
// STOP, while the vocabulary (`requested-disconnect` → reconnect) says the
// supervisor reconnects.

'use strict'

const { IpcConnection } = require('../../src/ipc/manager')
const { mapIpcConnectionEvents } = require('../../src/ipc/public-manager')

const IDENTITY = Object.freeze({
  attachmentId: 'attachment-1',
  peerId: 'peer-1',
  connectionId: 'connection-1',
  ownerLeaseId: 'lease-1',
  connectionGeneration: 'generation-1'
})

function controlledStream() {
  const queued = []
  const waiters = []
  let closed = false
  const settle = () => {
    while (waiters.length > 0 && (queued.length > 0 || closed)) {
      const waiter = waiters.shift()
      if (queued.length > 0) waiter.resolve({ done: false, value: queued.shift() })
      else waiter.resolve({ done: true, value: undefined })
    }
  }
  return {
    push(value) {
      queued.push(value)
      settle()
    },
    close() {
      closed = true
      settle()
    },
    [Symbol.asyncIterator]() {
      return {
        next() {
          if (queued.length > 0) return Promise.resolve({ done: false, value: queued.shift() })
          if (closed) return Promise.resolve({ done: true, value: undefined })
          return new Promise(resolve => waiters.push({ resolve }))
        },
        return: async () => {
          closed = true
          settle()
          return { done: true, value: undefined }
        }
      }
    }
  }
}

function stubManager(subscription) {
  return {
    bootstrap: { attachment: { attachmentId: IDENTITY.attachmentId } },
    subscribeConnectionEvents: async (_handle, _identity, _signal, _released, publish) => {
      publish(subscription)
      return subscription
    },
    route: async command => {
      if (command === 'connection.disconnect') return { state: 'released', failures: [] }
      throw new Error(`unexpected route ${command}`)
    },
    retryUnresolvedAdmissionCleanup: async () => [],
    confirmConnectionAdmissionRelease: () => undefined
  }
}

async function flushPump() {
  for (let attempt = 0; attempt < 16; attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

async function releaseWithBareHostClose() {
  const events = controlledStream()
  let unsubscribed = false
  const subscription = {
    events,
    unsubscribe: async () => {
      unsubscribed = true
      events.close()
      return { state: 'released', failures: [] }
    }
  }
  const manager = stubManager(subscription)
  const connection = new IpcConnection(
    manager,
    'handle-1',
    IDENTITY.peerId,
    IDENTITY.connectionId,
    IDENTITY.ownerLeaseId,
    IDENTITY.connectionGeneration
  )
  const mapped = mapIpcConnectionEvents(connection.events, { ...IDENTITY })
  const iterator = mapped[Symbol.asyncIterator]()
  const first = iterator.next()
  await flushPump()
  const cleanup = await connection.disconnect()
  return { cleanup, first, iterator, unsubscribed: () => unsubscribed }
}

describe('IPC connection app release lifecycle (finding F4)', () => {
  function gatedRelease(reason = 'owner-released') {
    const events = controlledStream()
    let settleParent
    let parentEntered
    const entered = new Promise(resolve => {
      parentEntered = resolve
    })
    const parent = new Promise((resolve, reject) => {
      settleParent = { resolve, reject }
    })
    const subscription = {
      events,
      unsubscribe: async () => {
        events.push({
          kind: 'terminal',
          reason,
          error:
            reason === 'source-failed'
              ? {
                  code: 'platform.transport',
                  domain: 'connection',
                  operation: 'lifecycle-events',
                  platform: null,
                  retryability: 'caller-decides'
                }
              : null
        })
        return { state: 'released', failures: [] }
      }
    }
    const manager = stubManager(subscription)
    manager.route = async () => {
      parentEntered()
      return parent
    }
    const connection = new IpcConnection(
      manager,
      'handle-1',
      IDENTITY.peerId,
      IDENTITY.connectionId,
      IDENTITY.ownerLeaseId,
      IDENTITY.connectionGeneration
    )
    return { connection, events, entered, settleParent }
  }

  test('an explicit owner terminal waits for a held parent and then delivers one confirmed transition', async () => {
    const fixture = gatedRelease()
    const iterator = fixture.connection.events[Symbol.asyncIterator]()
    let firstSettled = false
    const first = iterator.next().then(item => {
      firstSettled = true
      return item
    })
    await flushPump()
    const cleanup = fixture.connection.disconnect()
    await fixture.entered
    await flushPump()
    expect(firstSettled).toBe(false)
    fixture.settleParent.resolve({ state: 'released', failures: [] })
    expect(await cleanup).toEqual({ state: 'released', failures: [] })
    expect((await first).value.value).toMatchObject({ cause: 'requested-disconnect', current: 'disconnected' })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    expect((await iterator.next()).done).toBe(true)
  })

  test.each(['refused', 'rejected'])(
    'an explicit owner terminal does not fabricate parent release when %s',
    async outcome => {
      const fixture = gatedRelease()
      const iterator = fixture.connection.events[Symbol.asyncIterator]()
      const first = iterator.next()
      await flushPump()
      const cleanup = fixture.connection.disconnect().then(
        value => ({ value }),
        error => ({ error })
      )
      await fixture.entered
      if (outcome === 'refused')
        fixture.settleParent.resolve({
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'connection',
              error: {
                code: 'platform.transport',
                domain: 'connection',
                operation: 'disconnect',
                platform: null,
                retryability: 'caller-decides'
              }
            }
          ]
        })
      else fixture.settleParent.reject(new Error('parent refused'))
      const result = await cleanup
      if (outcome === 'refused') expect(result.value.state).toBe('release-failed')
      else expect(result.error.message).toBe('parent refused')
      expect((await first).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
      expect((await iterator.next()).done).toBe(true)
    }
  )

  test('an owner terminal without an app release gate settles without a synthetic transition', async () => {
    const fixture = gatedRelease()
    const iterator = fixture.connection.events[Symbol.asyncIterator]()
    const first = iterator.next()
    await flushPump()
    fixture.events.push({ kind: 'terminal', reason: 'owner-released', error: null })
    expect((await first).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    expect((await iterator.next()).done).toBe(true)
  })

  test.each(['source-failed', 'overflow'])(
    'a winning %s terminal does not wait for or become app release',
    async reason => {
      const fixture = gatedRelease(reason)
      const iterator = fixture.connection.events[Symbol.asyncIterator]()
      const first = iterator.next()
      await flushPump()
      const cleanup = fixture.connection.disconnect()
      await fixture.entered
      expect((await first).value).toMatchObject({ kind: 'terminal', reason })
      fixture.settleParent.resolve({ state: 'released', failures: [] })
      expect((await cleanup).state).toBe('released')
      expect((await iterator.next()).done).toBe(true)
    }
  )

  test('an app-requested disconnect delivers the disconnected event, not a bare end', async () => {
    const { cleanup, first } = await releaseWithBareHostClose()
    expect(cleanup).toMatchObject({ state: 'released' })
    const result = await first
    expect(result.done).toBe(false)
    expect(result.value).toMatchObject({ current: 'disconnected', cause: 'requested-disconnect' })
  })

  test('the released stream ends owner-released after the final event', async () => {
    const { first, iterator } = await releaseWithBareHostClose()
    const event = await first
    expect(event.done).toBe(false)
    const terminal = await iterator.next().then(
      () => null,
      error => error
    )
    expect(terminal).not.toBeNull()
    expect(terminal.name).toBe('ExpectedConnectionEventEnd')
  })
})
