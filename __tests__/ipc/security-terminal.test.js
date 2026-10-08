const { createIpcSecurityBackend } = require('../../src/ipc/security')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { contractError } = require('../../src/backend-contract/errors')
const { capacity } = require('../../src/backend-contract/primitives')

test.each(['overflow', 'source-failed'])(
  'ceremony %s terminal aborts outstanding pairing and preserves its cause',
  async reason => {
    let stream, notify
    const nativeCause = contractError('platform.transport', 'platform', 'native.security.event-source').normalized
    let aborted = false
    const ipc = {
      registerStream: jest.fn((_handle, _guard, _limits, policy, onTerminal) => {
        notify = onTerminal
        stream = new CoreBoundedStream(
          { itemCapacity: capacity(1), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
          policy ?? 'drop-oldest'
        )
        return stream
      }),
      route: jest.fn(
        (_command, _payload, _binary, signal) =>
          new Promise((_resolve, reject) =>
            signal.addEventListener(
              'abort',
              () => {
                aborted = true
                reject(contractError('operation.aborted', 'platform', 'native.security.pair'))
              },
              { once: true }
            )
          )
      ),
      closeStream: () => stream.closeWithReason('owner-released')
    }
    const pairing = createIpcSecurityBackend(ipc).pair('peer', {
      signal: null,
      deadline: null,
      transport: 'le',
      protection: 'system-default',
      ceremony: { kind: 'agent', agent: { onChallenge: async () => new Promise(() => {}) } }
    })
    const outcome = expect(pairing).rejects.toMatchObject({
      normalized: reason === 'overflow' ? { code: 'stream.overflow' } : nativeCause
    })
    expect(ipc.registerStream.mock.calls[0][3]).toBe('error')
    stream.finishWithReason(reason, reason === 'source-failed' ? nativeCause : null)
    if (notify) notify(reason, reason === 'source-failed' ? nativeCause : null)
    await outcome
    expect(aborted).toBe(true)
  }
)

test('security watch close aborts stalled IPC admission without masking unrelated source errors', async () => {
  let signal
  const ipc = {
    route: jest.fn((_command, _payload, _binary, current) => {
      signal = current
      return new Promise((_resolve, reject) =>
        current?.addEventListener(
          'abort',
          () => reject(contractError('operation.aborted', 'connection', 'controlled.security.admission')),
          { once: true }
        )
      )
    })
  }
  const watch = createIpcSecurityBackend(ipc).watch('peer')
  const iterator = watch[Symbol.asyncIterator]()
  const pending = iterator.next()
  const close = watch.close()
  expect(signal).toBeDefined()
  expect(signal.aborted).toBe(true)
  await expect(close).resolves.toMatchObject({ state: 'released' })
  await expect(pending).resolves.toMatchObject({ done: true })
})

test('security late handle cleanup is shared and failed unsubscribe remains retryable', async () => {
  let admit, finishClose
  const admission = new Promise(resolve => {
    admit = resolve
  })
  const cleanup = new Promise(resolve => {
    finishClose = resolve
  })
  let attempts = 0
  const ipc = {
    route: jest.fn(command =>
      command.endsWith('.subscribe') ? admission : ++attempts === 1 ? cleanup : Promise.resolve({ state: 'released' })
    ),
    registerStream: () => ({
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: false, value: 'stale' }),
        return: async () => ({ done: true })
      })
    }),
    closeStream: jest.fn()
  }
  const watch = createIpcSecurityBackend(ipc).watch('peer')
  const iterator = watch[Symbol.asyncIterator]()
  const pending = iterator.next()
  const first = watch.close().then(
    value => ({ value }),
    error => ({ error })
  )
  const second = iterator.return().then(
    value => ({ value }),
    error => ({ error })
  )
  admit({ handle: 'late' })
  await Promise.resolve()
  await Promise.resolve()
  await Promise.resolve()
  expect(attempts).toBe(1)
  finishClose({ state: 'release-failed' })
  expect((await first).error).toBeDefined()
  expect((await second).error).toBeDefined()
  await expect(pending).resolves.toMatchObject({ done: true })
  await expect(watch.close()).resolves.toMatchObject({ state: 'released' })
  expect(attempts).toBe(2)
  expect(ipc.closeStream).toHaveBeenCalledTimes(1)
})

test('security watch source admission failure stays specific when close overlaps', async () => {
  let fail
  const ipc = {
    route: () =>
      new Promise((_resolve, reject) => {
        fail = reject
      })
  }
  const watch = createIpcSecurityBackend(ipc).watch('peer')
  const iterator = watch[Symbol.asyncIterator]()
  const pending = iterator.next()
  const close = watch.close()
  fail(contractError('adapter.powered-off', 'adapter', 'controlled.security.source'))
  await expect(pending).rejects.toMatchObject({
    normalized: { code: 'adapter.powered-off', operation: 'controlled.security.source' }
  })
  await expect(close).resolves.toMatchObject({ state: 'released' })
})
