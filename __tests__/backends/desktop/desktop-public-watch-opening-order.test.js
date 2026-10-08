'use strict'

// Real public facade/provider/NAPI synthetic central; the wrapper only delays
// delivery of the native getter answer. This is not physical-radio evidence.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

async function until(predicate, label) {
  await h.withTimeout(
    (async () => {
      while (!predicate()) await new Promise(resolve => setImmediate(resolve))
    })(),
    5000,
    label
  )
}

test.each(['parameters', 'readiness'])(
  'public %s retains a newer native observation over delayed opening success',
  async kind => {
    const platform = kind === 'parameters' ? 'winrt' : 'corebluetooth'
    const harness = h.realBinding(platform)
    const open = harness.binding.openSynthetic
    const getter = kind === 'parameters' ? 'connectionParameters' : 'writeReadiness'
    const take = kind === 'parameters' ? 'takeConnectionParameterEvent' : 'takeWriteReadinessEvent'
    let release,
      getterAnswered = false,
      accepted = false
    let first = true
    harness.binding.openSynthetic = async (...args) => {
      const central = await open(...args)
      return new Proxy(central, {
        get(target, property) {
          const value = Reflect.get(target, property)
          if (property === getter)
            return async (...args) => {
              const answer = await value(...args)
              if (!first) return answer
              first = false
              getterAnswered = true
              return new Promise(resolve => {
                release = () => resolve(answer)
              })
            }
          if (property === take)
            return async (...args) => {
              const event = await value(...args)
              if (event?.kind === 'state') accepted = true
              return event
            }
          return value
        }
      })
    }
    const now = () => performance.now()
    const manager = await createPublicBleManager(
      await createNodeBleManagerFromProvider(
        createTestDesktopRustCoreBackendProvider({
          platform,
          owner: `opening-${kind}`,
          now,
          radio: 'synthetic',
          binding: harness.binding,
          hostPlatform: platform === 'winrt' ? 'win32' : 'darwin'
        }),
        DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
        { now }
      ),
      now
    )
    const stage = harness.opened.at(-1)
    let scan, connection, iterator
    try {
      scan = await manager.scan()
      const observations = scan.observations[Symbol.asyncIterator]()
      const peerResult = h.nextValue(observations, 5000)
      await stage.stageAdvertisement({ peerId: 'ordering-peer', localName: 'Ordering' })
      const peer = (await peerResult).peer
      await observations.return()
      await scan.stop()
      connection = await manager.connect(peer)
      if (kind === 'parameters') await stage.stageConnectionParameters('ordering-peer', 30_000, 2, 4_000_000, false)
      else await stage.stageWriteReadiness('ordering-peer', false, false)
      iterator = (
        kind === 'parameters'
          ? connection.controls.parameterEvents()
          : connection.controls.writeReadiness('without-response')
      )[Symbol.asyncIterator]()
      const firstValue = iterator.next()
      await until(() => getterAnswered, 'native opening getter answered')
      if (kind === 'parameters') await stage.stageConnectionParameters('ordering-peer', 90_000, 2, 4_000_000, true)
      else await stage.stageWriteReadiness('ordering-peer', true, true)
      await until(() => accepted, 'native event accepted during opening')
      release()
      await expect(firstValue).resolves.toMatchObject({
        done: false,
        value: kind === 'parameters' ? { intervalMs: 90 } : { ready: true }
      })
    } finally {
      release?.()
      await iterator?.return()
      await connection?.release()
      await scan?.stop()
      await manager.destroy()
    }
  },
  15000
)
