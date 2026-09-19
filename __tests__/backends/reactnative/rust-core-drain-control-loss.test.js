// X-R5: the drain router reports the owner's cumulative control-loss
// counter promptly — once per increase — instead of waiting for the
// in-band `ingress-drop` record.

const {
  RustCoreDrainRouter
} = require('../../../src/backends/reactnative/react-native-rust-core-drain')

function stubSession(batches) {
  const queue = batches.map(batch => ({ ...batch }))
  return {
    drain: async () => queue.shift() ?? { more: false, records: [], controlLost: 0 },
    onWake: () => () => {}
  }
}

function flush() {
  return new Promise(resolve => setImmediate(resolve))
}

describe('drain router control-loss counter', () => {
  test('forwards each increase once, in total order', async () => {
    const seen = []
    const sink = {
      deliver: () => {},
      failed: error => {
        throw error
      },
      noteControlLoss: total => seen.push(total)
    }
    const router = new RustCoreDrainRouter(
      stubSession([
        { more: true, records: [], controlLost: 0 },
        { more: true, records: [], controlLost: 2 },
        { more: false, records: [], controlLost: 2 }
      ]),
      sink
    )
    router.start()
    await flush()
    router.wake()
    await flush()
    expect(seen).toEqual([2])
    await router.stop()
  })

  test('a sink without the hook still drains', async () => {
    const delivered = []
    const sink = { deliver: record => delivered.push(record), failed: () => {} }
    const router = new RustCoreDrainRouter(
      stubSession([{ more: false, records: [], controlLost: 9 }]),
      sink
    )
    router.start()
    await flush()
    expect(delivered).toEqual([])
    await router.stop()
  })
})
