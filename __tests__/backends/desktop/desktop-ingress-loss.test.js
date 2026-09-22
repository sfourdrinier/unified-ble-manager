'use strict'

// Finding 131: notifications the radio lost before the central could hold
// them are surfaced on the public subscription exactly like core-held
// overflow, by the consumer's own overflow policy: `error` ends the stream
// with an overflow terminal carrying the counts; lossy policies report an
// overflow notice with the counts and keep delivering.

const {
  HRM_MEASUREMENT,
  HRM_SERVICE,
  connectAndDiscover,
  delivery,
  nextItem,
  nextValue,
  openBackend,
  subscribeOptions
} = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']
const TARGET = { peerId: 'peer-1', serviceUuid: HRM_SERVICE, characteristicUuid: HRM_MEASUREMENT }

async function withSubscription(platform, overflowPolicy, run) {
  const { backend, stage } = await openBackend(platform)
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const subscription = await database.subscribe(
      measurement.path,
      subscribeOptions({ delivery: { ...delivery(), overflowPolicy } })
    )
    await run({ stage, values: subscription.values[Symbol.asyncIterator]() })
  } finally {
    await backend.destroy()
  }
}

describe('ingress drops follow the consumer overflow policy', () => {
  test.each(PLATFORMS)('%s, error: the stream ends overflow with the counts', async platform => {
    await withSubscription(platform, 'error', async ({ stage, values }) => {
      await stage.stageNotificationsLost(TARGET, 3)
      expect(await nextItem(values, 5000)).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 3 })
    })
  })

  test.each(PLATFORMS.flatMap(platform => ['drop-oldest', 'drop-newest', 'latest'].map(policy => [platform, policy])))(
    '%s, %s: an overflow notice with the counts, and delivery continues',
    async (platform, policy) => {
      await withSubscription(platform, policy, async ({ stage, values }) => {
        await stage.stageNotificationsLost(TARGET, 3)
        expect(await nextItem(values, 5000)).toMatchObject({ kind: 'overflow', droppedItems: 3 })
        await stage.stageNotification({ ...TARGET, value: Buffer.from([7]) })
        expect([...(await nextValue(values, 5000)).value]).toEqual([7])
        await stage.stageNotificationsLost(TARGET, 2)
        expect(await nextItem(values, 5000)).toMatchObject({ kind: 'overflow', droppedItems: 5 })
      })
    }
  )
})

test('the central counts its radio intake drops', async () => {
  const { backend, stage } = await openBackend('bluez')
  try {
    const drops = await stage.ingressNotificationDrops()
    expect(Number.isSafeInteger(drops) && drops >= 0).toBe(true)
  } finally {
    await backend.destroy()
  }
})
