// __tests__/electron/ordered-group-cleanup.test.js

const { ElectronMainBleRouter } = require('../../src/electron/main-router')
const { OrderedDelivery } = require('../../src/ipc/ordered-delivery')
const { ElectronOrderedMarkerRegistry } = require('../../src/electron/ordered-marker-registry')

const released = { state: 'released', failures: [] }
const tick = () => new Promise(resolve => setImmediate(resolve))

test('renderer cleanup retains an unacknowledged record and retries cleanup', async () => {
  const stream = (async function* () {
    yield { kind: 'terminal', reason: 'closed', uncertainTail: true }
  })()
  const delivery = new OrderedDelivery(stream)
  delivery.poll()
  await tick()
  const retained = delivery.poll()
  let attempts = 0
  const close = jest.fn(async () => ({
    group:
      ++attempts === 1
        ? {
            state: 'release-failed',
            failures: [
              {
                resourceKind: 'ordered',
                error: {
                  code: 'platform.failure',
                  domain: 'cleanup',
                  operation: 'test',
                  platform: null,
                  retryability: 'caller-decides'
                }
              }
            ]
          }
        : released,
    members: []
  }))
  const resources = {
    orderedGroups: new Map([
      [
        'group',
        { databaseHandle: 'db', setup: { state: 'ready', stream: { close } }, delivery, cleanupReleased: false }
      ]
    ]),
    releasedHandles: new Set()
  }
  const router = Object.create(ElectronMainBleRouter.prototype)
  router.orderedMarkers = new ElectronOrderedMarkerRegistry()
  expect((await router.releaseOrderedGroups(resources)).state).toBe('release-failed')
  const undrained = await router.releaseOrderedGroups(resources)
  expect(undrained).toMatchObject({
    state: 'release-failed',
    failures: [{ resourceKind: 'ordered-group-uncertain-tail' }]
  })
  expect(resources.orderedGroups.has('group')).toBe(true)
  expect(delivery.poll()).toEqual(retained)
  delivery.poll(retained.deliveryId)
  await tick()
  expect(delivery.poll()).toEqual({ state: 'closed' })
  expect((await router.releaseOrderedGroups(resources)).state).toBe('released')
  expect(resources.orderedGroups.size).toBe(0)
  expect(close).toHaveBeenCalledTimes(3)
})

test('an oversized IPC value and later evidence remain owned until recovery', async () => {
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const stream = new CoreOrderedNotificationStream(
    { itemCapacity: 8, byteCapacity: 2048, reservedControlCapacity: 2 },
    () => {},
    async () => ({ group: released, members: [] })
  )
  const value = (ordinal, bytes) => ({
    kind: 'value',
    collectionId: 'c',
    connectionGeneration: 'g',
    databaseGeneration: 'd',
    memberId: 'm',
    ordinal,
    value: new Uint8Array(bytes),
    delivery: 'notification',
    provenance: 'notification'
  })
  stream.emit(value('1', 1000))
  stream.emit(value('2', 1))
  stream.emit({
    kind: 'loss',
    scope: { kind: 'unknown' },
    reason: 'source-loss',
    lostValues: null,
    lostBytes: null,
    lostSourceRecords: '2'
  })
  stream.finish({ kind: 'terminal', reason: 'source-failed', uncertainTail: true })
  const delivery = new OrderedDelivery(stream[Symbol.asyncIterator]())
  const group = {
    databaseHandle: 'db',
    setup: { state: 'ready', stream },
    delivery,
    pendingSetupObservations: [],
    cleanupReleased: false,
    cleanupResponse: null
  }
  const resources = {
    orderedGroups: new Map([['group', group]]),
    releasedOrderedGroups: new Map(),
    releasedHandles: new Set()
  }
  const router = Object.create(ElectronMainBleRouter.prototype)
  router.orderedMarkers = new ElectronOrderedMarkerRegistry()
  router.maximumMessageBytes = 512
  expect(await router.nextOrdered(resources, { orderedHandle: 'group' })).toEqual({ state: 'empty' })
  await tick()
  const retained = delivery.current
  for (let attempt = 0; attempt < 2; attempt++) {
    await expect(router.nextOrdered(resources, { orderedHandle: 'group' })).rejects.toMatchObject({
      normalized: { code: 'bytes.too-large' }
    })
    expect(delivery.current).toEqual(retained)
  }
  expect(resources.orderedGroups.get('group')).toBe(group)
})

test('renderer abandonment seals and exposes a payload-free receipt to the same authenticated owner', async () => {
  const stream = (async function* () {
    yield { kind: 'value', memberId: 'm', ordinal: '1', value: Uint8Array.of(42, 43) }
  })()
  const delivery = new OrderedDelivery(stream)
  delivery.poll()
  await tick()
  const resources = {
    ownerKey: '["client","window"]',
    orderedGroups: new Map([
      [
        'group',
        {
          databaseHandle: 'db',
          setup: {
            state: 'ready',
            stream: {
              discard: async () => ({
                group: released,
                members: [],
                discarded: {
                  values: '1',
                  bytes: '1',
                  controls: '1',
                  uncertainTail: false,
                  terminal: { kind: 'terminal', reason: 'closed', uncertainTail: false },
                  losses: []
                }
              })
            }
          },
          delivery
        }
      ]
    ]),
    releasedHandles: new Set()
  }
  const router = Object.create(ElectronMainBleRouter.prototype)
  router.orderedMarkers = new ElectronOrderedMarkerRegistry()
  router.orphanReceipts = new Map()
  router.orphanReservations = 0
  router.resources = new Map()
  router.maximumOutstandingOperations = 8
  router.maximumMessageBytes = 4096
  router.nextHandle = 1
  expect(await router.releaseOrderedGroups(resources, () => true, true)).toMatchObject({ state: 'released' })
  const receipt = router.inspectOrderedOrphans(resources.ownerKey).receipts[0]
  expect(receipt).toMatchObject({ format: 'public', values: '2', bytes: '3', controls: '1', uncertainTail: true })
  expect(JSON.stringify(receipt)).not.toContain('42')
  expect(router.inspectOrderedOrphans('["other","window"]').receipts).toEqual([])
  expect(() => router.acknowledgeOrderedOrphan(resources.ownerKey, receipt.receiptId)).toThrow()
  let offset = 0
  let encoded = ''
  for (;;) {
    const page = router.pageOrderedOrphan(resources.ownerKey, { receiptId: receipt.receiptId, offset })
    if (page.done) break
    encoded += page.chunk
    offset = page.nextOffset
  }
  expect(JSON.parse(encoded)).toMatchObject({ terminal: { kind: 'terminal' } })
  expect(router.acknowledgeOrderedOrphan(resources.ownerKey, receipt.receiptId)).toEqual({ state: 'forgotten' })
  expect(router.inspectOrderedOrphans(resources.ownerKey).receipts).toEqual([])
})

test('large receipt evidence is addressable, replayable and cannot be ACKed before the final page', async () => {
  const losses = Array.from({ length: 8 }, (_, index) => ({
    kind: 'loss',
    scope: { kind: 'member', memberId: `${index}-${'x'.repeat(180)}` },
    reason: 'source-loss',
    lostValues: null,
    lostBytes: null,
    lostSourceRecords: '1'
  }))
  const stream = (async function* () {})()
  const resources = {
    ownerKey: 'owner',
    orderedGroups: new Map([
      [
        'group',
        {
          databaseHandle: 'db',
          setup: {
            state: 'ready',
            stream: {
              discard: async () => ({
                group: released,
                members: [],
                discarded: {
                  values: '0',
                  bytes: '0',
                  controls: '9',
                  uncertainTail: true,
                  losses,
                  trustedValues: [
                    {
                      memberId: 'A',
                      ordinal: '18446744073709551615',
                      sourceMembership: { member: '0', epoch: '9' },
                      bytes: '4'
                    }
                  ],
                  terminal: { kind: 'terminal', reason: 'source-failed', uncertainTail: true }
                }
              })
            }
          },
          delivery: new OrderedDelivery(stream),
          pendingSetupObservations: []
        }
      ]
    ]),
    releasedHandles: new Set()
  }
  const router = Object.create(ElectronMainBleRouter.prototype)
  router.orderedMarkers = new ElectronOrderedMarkerRegistry()
  router.orphanReceipts = new Map()
  router.orphanReservations = 0
  router.resources = new Map()
  router.maximumOutstandingOperations = 8
  router.maximumMessageBytes = 512
  router.nextHandle = 1
  expect((await router.releaseOrderedGroups(resources, () => true, true)).state).toBe('released')
  const summary = router.inspectOrderedOrphans('owner').receipts[0]
  expect(summary).toMatchObject({ controls: '9', uncertainTail: true })
  expect(() => router.acknowledgeOrderedOrphan('owner', summary.receiptId)).toThrow()
  let offset = 0
  let encoded = ''
  let pages = 0
  for (;;) {
    const page = router.pageOrderedOrphan('owner', { receiptId: summary.receiptId, offset })
    expect(router.pageOrderedOrphan('owner', { receiptId: summary.receiptId, offset })).toEqual(page)
    if (page.done) break
    encoded += page.chunk
    offset = page.nextOffset
    pages += 1
  }
  expect(pages).toBeGreaterThan(1)
  expect(JSON.parse(encoded)).toMatchObject({
    losses,
    trustedValues: [
      { memberId: 'A', ordinal: '18446744073709551615', sourceMembership: { member: '0', epoch: '9' }, bytes: '4' }
    ],
    terminal: { kind: 'terminal', reason: 'source-failed' }
  })
  expect(router.acknowledgeOrderedOrphan('owner', summary.receiptId)).toEqual({ state: 'forgotten' })
})
