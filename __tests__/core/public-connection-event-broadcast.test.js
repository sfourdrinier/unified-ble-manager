const { broadcastConnectionEvents, publicConnectionTerminalError } = require('../../src/public/ble-manager')

function event(sequence, cause, safeMessage = '') {
  return {
    kind: 'connection-lifecycle',
    previous: 'connected',
    current: cause === 'peer-link-loss' ? 'lost' : 'disconnected',
    cause,
    connectionGeneration: 'generation-1',
    sequence,
    platform: { domain: 'native', code: '2', safeMessage, metadata: { exact: '界' } }
  }
}

async function flush() {
  for (let turn = 0; turn < 16; turn += 1) await Promise.resolve()
}

test.each(['requested-disconnect', 'peer-link-loss'])(
  'buffered %s values drain FIFO before their source terminal',
  async cause => {
    const values = [event(1, 'connected'), event(2, cause, '界'.repeat(4096))]
    const source = broadcastConnectionEvents(
      (async function* () {
        yield* values
        throw publicConnectionTerminalError(cause === 'peer-link-loss' ? 'connection-lost' : 'owner-released')
      })()
    )
    const iterator = source[Symbol.asyncIterator]()
    await flush()
    for (const value of values) await expect(iterator.next()).resolves.toEqual({ done: false, value })
    if (cause === 'peer-link-loss') {
      await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'connection.lost' } })
      await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'connection.lost' } })
      await expect(source[Symbol.asyncIterator]().next()).rejects.toMatchObject({
        normalized: { code: 'connection.lost' }
      })
      await iterator.return()
    }
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
  }
)

test('local byte overflow remains the winning terminal despite a later source failure', async () => {
  const source = broadcastConnectionEvents(
    (async function* () {
      for (let sequence = 1; sequence <= 6; sequence += 1)
        yield event(sequence, 'backend-transition', '界'.repeat(5000))
      throw publicConnectionTerminalError('connection-lost')
    })()
  )
  const iterator = source[Symbol.asyncIterator]()
  await flush()
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'stream.overflow' } })
  await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'stream.overflow' } })
  await iterator.return()
  await expect(iterator.next()).resolves.toMatchObject({ done: true })
})

test('returning a subscriber remains closed after a later source failure', async () => {
  let resume
  const gate = new Promise(resolve => {
    resume = resolve
  })
  const source = broadcastConnectionEvents(
    (async function* () {
      await gate
      throw publicConnectionTerminalError('connection-lost')
    })()
  )
  const iterator = source[Symbol.asyncIterator]()
  await iterator.return()
  resume()
  await flush()
  await expect(iterator.next()).resolves.toMatchObject({ done: true })
  await expect(source[Symbol.asyncIterator]().next()).rejects.toMatchObject({ normalized: { code: 'connection.lost' } })
})
