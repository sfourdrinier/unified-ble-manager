// __tests__/backends/desktop/desktop-public-release-observation.test.js
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

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))

async function waitUntil(condition, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs
  while (!(await condition())) {
    if (Date.now() > deadline) throw new Error(`${label} did not happen within ${timeoutMs} ms`)
    await sleep(1)
  }
}

/**
 * Run one native call of the next opened central only when `resume()` is
 * called, starting with the first call after `arm()`. A deterministic stand-in
 * for the native worker running the call later than the JS call that issued it
 * (`take*` polls and `disconnect` are all dispatched that way).
 */
function parkNextNativeCall(harness, method, { retainNonNull = false } = {}) {
  let armed = false
  let resume
  const resumed = new Promise(resolve => {
    resume = resolve
  })
  let signalParked
  const parked = new Promise(resolve => {
    signalParked = resolve
  })
  const open = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (...openArgs) => {
    const central = await open(...openArgs)
    return new Proxy(central, {
      get(target, property) {
        const value = Reflect.get(target, property)
        if (property === method) {
          return async (...args) => {
            if (!armed) return Reflect.apply(value, target, args)
            armed = false
            signalParked()
            await resumed
            let result = await Reflect.apply(value, target, args)
            if (!retainNonNull || (result !== null && result !== undefined)) return result
            // The Rust reset report is published after native link teardown.
            // Retain the actual report here before returning to an outer gate;
            // a null poll is a valid early answer, not the report itself.
            const deadline = Date.now() + 5000
            while (result === null || result === undefined) {
              if (Date.now() > deadline) throw new Error(`${method} did not return a native report within 5000 ms`)
              await sleep(1)
              result = await Reflect.apply(value, target, args)
            }
            return result
          }
        }
        return typeof value === 'function' ? (...args) => Reflect.apply(value, target, args) : value
      }
    })
  }
  return {
    arm() {
      armed = true
    },
    parked: () => h.withTimeout(parked, 5000, `native ${method} reached`),
    resume
  }
}

/**
 * The actual public manager/provider/NAPI/Rust flow over the synthetic radio,
 * up to one live connection. `install(harness)` wraps the binding before the
 * provider opens it and returns what `body` also receives; its optional
 * `beforeDestroy` runs ahead of the manager's own cleanup.
 */
async function withConnectedPeer(platform, install, body) {
  const harness = h.realBinding(platform)
  const installed = install(harness) ?? {}
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
  return runWithCleanup(
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
      return body({ ...installed, harness, manager, stage, connection, events })
    },
    async () => {
      installed.beforeDestroy?.()
      const cleanup = await manager.destroy()
      expect(cleanup).toEqual({ state: 'released', failures: [] })
      return cleanup
    }
  )
}

/** The native core cleared the connection record its reset ended (the OS answer a later release reads). */
async function awaitNativeLinkCleared(stage) {
  await waitUntil(
    async () => !(await stage.peerRecords()).some(record => record.connectionState != null),
    'native reset clearing the original connection'
  )
}

test.each(
  PLATFORMS.flatMap(platform => [
    ...['disconnect', 'release'].flatMap(method =>
      ['reply-first', 'event-first', 'unknown-reason'].map(order => [platform, method, order])
    ),
    [platform, 'release', 'reset-before-release'],
    [platform, 'release', 'failed-disconnect-then-release'],
    [platform, 'disconnect', 'concurrent-opposite-intent']
  ])
)('%s %s carries its own OS answer (%s)', async (platform, method, order) => {
  let nativeTerminalObserved = false
  const eventFirst =
    order === 'event-first' || order === 'failed-disconnect-then-release' || order === 'concurrent-opposite-intent'
  let resumeReply
  const replyGate = new Promise(resolve => {
    resumeReply = resolve
  })
  await withConnectedPeer(
    platform,
    harness => {
      // Controlled queue delivery: the gate holds the OS lifecycle/adapter
      // queues at the native answer, so nothing reaches the provider early.
      const gate = h.gateNativeEventQueues(harness, {
        onDeliver(queue, event) {
          if (queue === 'takeLifecycleEvent' && event?.kind === 'released') {
            nativeTerminalObserved = true
            if (eventFirst && method === 'release') resumeReply()
          }
        }
      })
      const open = harness.binding.openSynthetic
      harness.binding.openSynthetic = async (...openArgs) => {
        const central = await open(...openArgs)
        return new Proxy(central, {
          get(target, property) {
            const value = Reflect.get(target, property)
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
      return {
        gate,
        beforeDestroy() {
          gate.open()
          resumeReply()
        }
      }
    },
    async ({ gate, stage, connection, events }) => {
      const knownReason = order !== 'unknown-reason' && order !== 'reset-before-release'
      if (knownReason)
        await stage.stageDisconnectObservation('peer-1', {
          domain: 'bluez-mgmt',
          code: '2',
          metadata: { disconnectReason: 2 }
        })
      if (order === 'reply-first' || order === 'reset-before-release') gate.hold()
      if (order === 'reset-before-release') {
        await stage.stageAdapterReset('powered-off')
        await awaitNativeLinkCleared(stage)
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
      gate.open()
      expect(await events.next()).toEqual({ done: true, value: undefined })
    }
  )
})

// The native event polls run on a worker after the JS call that issued them.
// Model an in-flight poll whose real reset report reaches the delivery gate
// after it closes. Rust can clear the link before publishing that report, so
// this fixture waits for a positive native report before completing the
// controlled response; ordinary native polls may correctly return empty.
test.each(PLATFORMS)(
  '%s a reset poll already in flight when delivery is held stays held until the gate opens',
  async platform => {
    await withConnectedPeer(
      platform,
      harness => {
        const reset = parkNextNativeCall(harness, 'takeAdapterResetEvent', { retainNonNull: true })
        const delivered = []
        const gate = h.gateNativeEventQueues(harness, { onDeliver: queue => delivered.push(queue) })
        return {
          reset,
          gate,
          delivered,
          beforeDestroy: () => {
            gate.open()
            reset.resume()
          }
        }
      },
      async ({ reset, gate, delivered, stage, connection, events }) => {
        reset.arm()
        // A real native wake: the provider's pump turn polls the reset queue.
        await stage.stageAdapterState('powered-on', true)
        await reset.parked()
        gate.hold()
        await stage.stageAdapterReset('powered-off')
        await awaitNativeLinkCleared(stage)
        reset.resume()
        await waitUntil(
          () => gate.keptEvents() > 0 || delivered.includes('takeAdapterResetEvent'),
          'the in-flight reset poll answering with the staged reset'
        )
        // Never handed to the provider while the gate is closed.
        expect(delivered).not.toContain('takeAdapterResetEvent')
        const terminalPending = events.next()
        expect(await connection.release()).toEqual({ state: 'released', failures: [] })
        const terminal = await terminalPending
        expect(terminal.value).toMatchObject({
          cause: 'released',
          current: 'disconnected',
          connectionGeneration: connection.connectionGeneration
        })
        expect(terminal.value.platform).toBeUndefined()
        // Held, not dropped: the reset is delivered, in order, once the gate opens.
        gate.open()
        await waitUntil(() => gate.keptEvents() === 0, 'the held reset being delivered')
        expect(await events.next()).toEqual({ done: true, value: undefined })
      }
    )
  }
)

// A failed setup must free its controlled native call before destroying the
// manager; otherwise cleanup itself waits on the poll the fixture parked.
test.each(PLATFORMS)('%s cleanup unblocks a parked reset poll after a fixture failure', async platform => {
  const failure = new Error('controlled fixture failure before reset resume')
  let parkedReset
  let nativeHarness
  const operation = withConnectedPeer(
    platform,
    harness => {
      nativeHarness = harness
      const reset = parkNextNativeCall(harness, 'takeAdapterResetEvent', { retainNonNull: true })
      parkedReset = reset
      const gate = h.gateNativeEventQueues(harness)
      return {
        reset,
        gate,
        beforeDestroy: () => {
          gate.open()
          reset.resume()
        }
      }
    },
    async ({ reset, gate, stage }) => {
      reset.arm()
      await stage.stageAdapterState('powered-on', true)
      await reset.parked()
      gate.hold()
      await stage.stageAdapterReset('powered-off')
      await awaitNativeLinkCleared(stage)
      throw failure
    }
  )
  try {
    await expect(h.withTimeout(operation, 5000, 'fixture failure cleanup')).rejects.toBe(failure)
    expect(nativeHarness.calls).toContainEqual(['close', []])
  } finally {
    // Free the test-owned barrier even when the regression detects a failure.
    parkedReset?.resume()
    await expect(operation).rejects.toBe(failure)
  }
})

// The other order: the OS reset reaches the provider while the caller's own
// release is still waiting for its native answer. The loss ended the link, so
// the connection ends adapter-loss on every platform; the release answers
// released. CoreBluetooth and WinRT announce the loss as
// `connection-state-changed`, whose `previous` has to be the last state the
// core was told (`connected`): the provider's own `disconnecting` is private.
test.each(PLATFORMS.flatMap(platform => ['release', 'disconnect'].map(method => [platform, method])))(
  '%s %s with the adapter reset delivered while its native answer is pending ends adapter-loss',
  async (platform, method) => {
    await withConnectedPeer(
      platform,
      harness => {
        const nativeDisconnect = parkNextNativeCall(harness, 'disconnect')
        const gate = h.gateNativeEventQueues(harness)
        return {
          nativeDisconnect,
          gate,
          beforeDestroy() {
            gate.open()
            nativeDisconnect.resume()
          }
        }
      },
      async ({ nativeDisconnect, gate, stage, connection, events }) => {
        gate.hold()
        await stage.stageAdapterReset('powered-off')
        await awaitNativeLinkCleared(stage)
        nativeDisconnect.arm()
        const terminalPending = events.next()
        const call = connection[method]()
        await nativeDisconnect.parked()
        // The reset becomes deliverable while the native answer is outstanding.
        gate.open()
        const early = await Promise.race([terminalPending, sleep(1000).then(() => null)])
        nativeDisconnect.resume()
        const terminal = early ?? (await terminalPending)
        expect(terminal.value).toMatchObject({
          cause: 'adapter-loss',
          current: 'lost',
          connectionGeneration: connection.connectionGeneration
        })
        expect(await call).toEqual({ state: 'released', failures: [] })
        // A lost connection's lifecycle stream ends by reporting the loss.
        await expect(events.next()).rejects.toMatchObject({ normalized: { code: 'connection.lost' } })
      }
    )
  }
)

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
