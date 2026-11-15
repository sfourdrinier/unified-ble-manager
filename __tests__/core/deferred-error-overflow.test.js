// __tests__/core/deferred-error-overflow.test.js

const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { capacity } = require('../../src/backend-contract/primitives')

function stream() {
  return new CoreBoundedStream(
    {
      itemCapacity: capacity(1),
      byteCapacity: capacity(8),
      reservedControlCapacity: capacity(1)
    },
    'error'
  )
}

test('retains the accepted prefix and withholds overflow until its owner publishes proof', async () => {
  const source = stream()
  source.emit('accepted', 4)
  const terminal = source.prepareErrorOverflow(5)
  expect(terminal).toEqual({ kind: 'terminal', reason: 'overflow', droppedItems: 1, droppedBytes: 5, replacedItems: 0 })
  expect(source.isTerminal()).toBe(false)
  const reader = source[Symbol.asyncIterator]()
  expect(await reader.next()).toEqual({ done: false, value: { kind: 'value', value: 'accepted' } })
  let delivered = false
  const next = reader.next().then(value => {
    delivered = true
    return value
  })
  await Promise.resolve()
  expect(delivered).toBe(false)
  const evidence = {
    producer: 'scan',
    ref: 'native',
    byteLength: 1,
    digest: 'digest',
    formatVersion: 1,
    durability: 'committed',
    scope: { managerId: 'm', operationId: 'o', incarnation: '1' }
  }
  source.finishWithReason('overflow', null, evidence)
  expect(await next).toEqual({ done: false, value: { ...terminal, evidence } })
  await reader.return()
})

test('does not reject a value that can be delivered directly to a waiting reader', async () => {
  const source = stream()
  const reader = source[Symbol.asyncIterator]()
  const next = reader.next()
  expect(source.prepareErrorOverflow(20)).toBeNull()
  source.emit('direct', 20)
  expect(await next).toEqual({ done: false, value: { kind: 'value', value: 'direct' } })
  expect(source.overflowCounters()).toEqual({ droppedItems: 0, droppedBytes: 0, replacedItems: 0 })
  await reader.return()
})

test.each([8, 512])('defers a host-global loss terminal with capacity %s without inventing per-stream drops', async byteCapacity => {
  const source = new CoreBoundedStream({ itemCapacity: capacity(1), byteCapacity: capacity(byteCapacity), reservedControlCapacity: capacity(1) }, 'error')
  source.emit('accepted', 4)
  const loss = { kind: 'source-loss', scope: 'host-global', source: 'advertisement-ingress', lostRecords: 999 }
  const terminal = source.prepareSourceLossTerminal(loss)
  expect(terminal).toMatchObject({ kind: 'terminal', reason: byteCapacity === 8 ? 'source-failed' : 'overflow', droppedItems: 0, droppedBytes: 0, replacedItems: 0 })
  if (byteCapacity === 8) expect(terminal.error).toMatchObject({ code: 'stream.quota', operation: 'bounded-stream.source-loss-control' })
  else expect(source.sourceLossNotices()).toEqual([loss])
  expect(source.isTerminal()).toBe(false)
  const reader = source[Symbol.asyncIterator]()
  expect(await reader.next()).toEqual({ done: false, value: { kind: 'value', value: 'accepted' } })
  let delivered = false
  const next = reader.next().then(value => { delivered = true; return value })
  await Promise.resolve()
  expect(delivered).toBe(false)
  const evidence = { producer: 'scan', ref: 'native', byteLength: 1, digest: 'digest', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: '1' } }
  source.finishWithReason(terminal.reason, terminal.error ?? null, evidence)
  expect(await next).toEqual({ done: false, value: { ...terminal, evidence } })
  await reader.return()
})
