'use strict'

// Actual public manager/provider/NAPI/Rust flow over the synthetic radio.
// Controlled queue delivery proves ordering; this is not physical-radio evidence.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { runWithCleanup } = require('../../../src/public/error-bridge')
const { parseDesktopRustCoreReleaseReport } = require('../../../src/backends/desktop/desktop-rust-core-binding')

jest.setTimeout(30000)

test.each(
  ['bluez', 'corebluetooth', 'winrt'].flatMap(platform => [
    ...['disconnect', 'release'].flatMap(method =>
      ['reply-first', 'event-first', 'unknown-reason'].map(order => [platform, method, order])
    ),
    [platform, 'release', 'reset-before-release'],
    [platform, 'release', 'failed-disconnect-then-release'],
    [platform, 'disconnect', 'concurrent-opposite-intent']
  ])
)('%s %s carries its own OS answer (%s)', async (platform, method, order) => {
  const harness = h.realBinding(platform)
  let holdEvents = false
  let nativeTerminalObserved = false
  const eventFirst =
    order === 'event-first' || order === 'failed-disconnect-then-release' || order === 'concurrent-opposite-intent'
  let resumeReply
  const replyGate = new Promise(resolve => {
    resumeReply = resolve
  })
  const open = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (...openArgs) => {
    const central = await open(...openArgs)
    return new Proxy(central, {
      get(target, property) {
        const value = Reflect.get(target, property)
        if (property === 'takeAdapterResetEvent' || property === 'takeAdapterEvent')
          return (...args) => (holdEvents ? Promise.resolve(null) : Reflect.apply(value, target, args))
        if (property === 'takeLifecycleEvent')
          return async (...args) => {
            if (holdEvents) return null
            const event = await Reflect.apply(value, target, args)
            if (event?.kind === 'released') {
              nativeTerminalObserved = true
              if (eventFirst && method === 'release') resumeReply()
            }
            return event
          }
        if (property === 'disconnect')
          return async (...args) => {
            const answer = await Reflect.apply(value, target, args)
            if (eventFirst) {
              await replyGate
              expect(nativeTerminalObserved).toBe(true)
            }
            return answer
          }
        return typeof value === 'function' ? (...args) => Reflect.apply(value, target, args) : value
      }
    })
  }
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: `release-report-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: h.HOST_PLATFORM[platform]
  })
  const internal = await createNodeBleManagerFromProvider(
    provider,
    DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
    { now: () => performance.now() }
  )
  const manager = await createPublicBleManager(internal, () => performance.now())
  const stage = harness.opened.at(-1)
  await runWithCleanup(
    async () => {
      const scan = await manager.scan()
      const observations = scan.observations[Symbol.asyncIterator]()
      const pending = h.nextValue(observations, 5000)
      await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -50, localName: 'Release observation' })
      const observed = await pending
      await observations.return?.()
      await scan.stop()
      const connection = await manager.connect(observed.peer)
      const events = connection.lifecycleEvents[Symbol.asyncIterator]()
      const connected = await events.next()
      expect(connected.value.cause).toBe('connected')
      const knownReason = order !== 'unknown-reason' && order !== 'reset-before-release'
      if (knownReason)
        await stage.stageDisconnectObservation('peer-1', {
          domain: 'bluez-mgmt',
          code: '2',
          metadata: { disconnectReason: 2 }
        })
      holdEvents = order === 'reply-first' || order === 'reset-before-release'
      if (order === 'reset-before-release') {
        await stage.stageAdapterReset('powered-off')
        const deadline = Date.now() + 5000
        while ((await stage.peerRecords()).some(record => record.connectionState != null)) {
          if (Date.now() > deadline) throw new Error('native reset did not clear original connection')
          await new Promise(resolve => setTimeout(resolve, 1))
        }
      }
      if (order === 'failed-disconnect-then-release') {
        await stage.failNextRadioOp('disconnect', 'settled initial disconnect refused')
        expect(await connection.disconnect()).toMatchObject({ state: 'release-failed' })
      }
      const terminalPending = events.next()
      const release = connection[method]()
      const concurrent = order === 'concurrent-opposite-intent' ? connection.release() : null
      if (order === 'reset-before-release') expect(await release).toEqual({ state: 'released', failures: [] })
      const terminal = await terminalPending
      expect(terminal.value).toMatchObject({
        cause: method === 'disconnect' ? 'requested-disconnect' : 'released',
        current: 'disconnected',
        connectionGeneration: connection.connectionGeneration
      })
      expect(terminal.value.platform).toEqual(
        !knownReason
          ? undefined
          : { domain: 'bluez-mgmt', code: '2', safeMessage: '', metadata: { disconnectReason: 2 } }
      )
      resumeReply()
      expect(await release).toEqual({ state: 'released', failures: [] })
      if (concurrent !== null) expect(await concurrent).toEqual({ state: 'released', failures: [] })
      holdEvents = false
      expect(await events.next()).toEqual({ done: true, value: undefined })
    },
    async () => {
      holdEvents = false
      resumeReply()
      const cleanup = await manager.destroy()
      expect(cleanup).toEqual({ state: 'released', failures: [] })
      return cleanup
    }
  )
})

test.each([
  'released',
  {},
  { schema: 'ubm-desktop-release/0' },
  { schema: 'ubm-desktop-release/1', state: 'released', peerId: 'new', lease: 'lease', connectionGeneration: '7' },
  { schema: 'ubm-desktop-release/1', state: 'released', peerId: 'peer', lease: 'wrong', connectionGeneration: '7' },
  { schema: 'ubm-desktop-release/1', state: 'released', peerId: 'peer', lease: 'lease', connectionGeneration: '8' },
  {
    schema: 'ubm-desktop-release/1',
    state: 'released',
    peerId: 'peer',
    lease: 'lease',
    connectionGeneration: '7',
    platform: 2
  }
])('release report rejects malformed or unrelated own answers %#', value => {
  expect(() =>
    parseDesktopRustCoreReleaseReport(value, { peerId: 'peer', lease: 'lease', connectionGeneration: '7' })
  ).toThrow()
})
