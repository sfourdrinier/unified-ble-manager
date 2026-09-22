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
    subscribeConnectionEvents: async () => subscription,
    route: async command => {
      if (command === 'connection.disconnect') return { state: 'released', failures: [] }
      throw new Error(`unexpected route ${command}`)
    },
    retryUnresolvedAdmissionCleanup: async () => []
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
