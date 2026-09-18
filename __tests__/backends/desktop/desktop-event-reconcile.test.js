'use strict'

// LEGACY-AUDIT-2 N5 / N6 over the REAL addon on its synthetic radio. The
// core publishes lifecycle, scan-terminal, security and write-readiness
// events on bounded broadcasts; a receiver that falls behind sees `lagged`.
// Legacy delivered those by direct callback, so a lag must never leave a
// lost link "connected" or a scan stream open forever: the provider re-reads
// the core's own facts and emits the transitions it missed. A notification
// value the binding hands over malformed ends that stream `source-failed`
// (as React Native does, #56), never drop-and-continue.
//
// The only double is a `take*Event` wrapper that swallows the real events
// (what a lagged broadcast receiver loses) and reports the gap; every fact the
// provider re-reads is the Rust core's own answer.

const {
  HRM_MEASUREMENT,
  HRM_SERVICE,
  connectAndDiscover,
  drainFor,
  nextEvent,
  nextItem,
  nextValue,
  observePeer,
  realBinding,
  scanOptions,
  subscribeOptions,
  HOST_PLATFORM
} = require('../../helpers/desktop-rust-core-harness')
const { createDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

/**
 * A backend whose central reports a lag on `method` while `control[method]`
 * is armed: every real event queued meanwhile is swallowed and one
 * `{ kind: 'lagged', missed }` is returned in their place. `pollOverride`
 * replaces one notification poll result when set.
 */
async function openLaggingBackend(platform) {
  const harness = realBinding(platform)
  const control = { armed: new Set(), swallowed: {}, pollOverride: null, lagWithoutEvents: null, holdPolls: false, wakes: 0 }
  const original = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (owner, options) => {
    const central = await original(owner, options)
    return new Proxy(central, {
      get(target, property) {
        const value = Reflect.get(target, property)
        if (typeof value !== 'function') return value
        if (property === 'setEventWaker') {
          return wake =>
            Reflect.apply(value, target, [
              () => {
                control.wakes += 1
                wake()
              }
            ])
        }
        if (property === 'pollNotification') {
          return async (...args) => {
            // Held: the poll has not come round yet, so the core keeps the
            // values queued until the hold is released.
            while (control.holdPolls) await new Promise(resolve => setTimeout(resolve, 2))
            const real = await Reflect.apply(value, target, args)
            if (control.pollOverride !== null && real.kind === 'value') {
              const replaced = control.pollOverride
              control.pollOverride = null
              return replaced
            }
            return real
          }
        }
        if (typeof property === 'string' && property.startsWith('take') && property.endsWith('Event')) {
          return async () => {
            if (control.lagWithoutEvents === property) {
              control.lagWithoutEvents = null
              return { kind: 'lagged', missed: 1 }
            }
            if (!control.armed.has(property)) return Reflect.apply(value, target, [])
            let missed = 0
            for (;;) {
              const event = await Reflect.apply(value, target, [])
              if (event === null || event === undefined) break
              missed += 1
            }
            if (missed === 0) return null
            control.armed.delete(property)
            control.swallowed[property] = (control.swallowed[property] ?? 0) + missed
            return { kind: 'lagged', missed }
          }
        }
        return (...args) => Reflect.apply(value, target, args)
      }
    })
  }
  const provider = createDesktopRustCoreBackendProvider({
    platform,
    owner: `reconcile-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: HOST_PLATFORM[platform]
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  const stage = harness.opened[harness.opened.length - 1]
  return { backend, stage, control }
}

async function withLaggingBackend(platform, run) {
  const opened = await openLaggingBackend(platform)
  try {
    return await run(opened)
  } finally {
    await opened.backend.destroy()
  }
}

async function eventuallySwallowed(control, method, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs
  while (control.armed.has(method)) {
    if (Date.now() > deadline) throw new Error(`${method} never reported a lag`)
    await new Promise(resolve => setTimeout(resolve, 5))
  }
  expect(control.swallowed[method]).toBeGreaterThan(0)
}

async function eventuallyLagged(control, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs
  while (control.lagWithoutEvents !== null) {
    if (Date.now() > deadline) throw new Error('the lag was never taken')
    await new Promise(resolve => setTimeout(resolve, 5))
  }
  // One more pump turn so the reconciliation it triggered has run.
  await new Promise(resolve => setTimeout(resolve, 50))
}

describe('lifecycle lag reconciles from the core (N5)', () => {
  test.each(PLATFORMS)('%s: a link lost while lagged is announced connection-lost', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { lease } = await connectAndDiscover(backend, stage)
      control.armed.add('takeLifecycleEvent')
      await stage.stageLinkLoss('peer-1')
      const lost = await nextEvent(events, event => event.kind === 'connection-lost', 5000)
      expect(lost.connection.connectionId).toEqual(lease.connection.connectionId)
      await eventuallySwallowed(control, 'takeLifecycleEvent')
    })
  })

  test.each(PLATFORMS)('%s: a database change missed while lagged is announced database-changed', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { lease } = await connectAndDiscover(backend, stage)
      control.armed.add('takeLifecycleEvent')
      await stage.stageServicesChanged('peer-1')
      const changed = await nextEvent(
        events,
        event => event.kind === 'database-changed' || event.kind === 'connection-lost',
        5000
      )
      expect(changed.kind).toBe('database-changed')
      expect(changed.database.connectionId).toEqual(lease.connection.connectionId)
      await eventuallySwallowed(control, 'takeLifecycleEvent')
    })
  })

  test("a lag that missed only another peer's release fabricates no transition on a live link", async () => {
    await withLaggingBackend('bluez', async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const otherPeer = await observePeer(backend, stage, { peerId: 'peer-2' })
      await stage.stageMtu('peer-2', 185)
      const other = await backend.connections.connect(otherPeer, 'client-2', { signal: null, deadline: null })
      control.armed.add('takeLifecycleEvent')
      expect(await other.release()).toEqual({ state: 'released', failures: [] })
      await eventuallySwallowed(control, 'takeLifecycleEvent')
      const seen = (await drainFor(events, 300)).filter(item => item.kind === 'value').map(item => item.value.kind)
      expect(seen.filter(kind => kind === 'connection-lost' || kind === 'database-changed')).toEqual([])
      expect((await database.read(measurement.path, { signal: null, deadline: null })).length).toBeGreaterThan(0)
    })
  })
})

/** The legacy per-OS adapter-loss sequence (desktop-adapter-loss.test.js). */
const ADAPTER_LOSS = {
  corebluetooth: { adapterReason: true, restarted: true },
  winrt: { adapterReason: true, restarted: false },
  bluez: { adapterReason: false, restarted: true }
}

describe('adapter-reset lag reconciles from the core (N5)', () => {
  test.each(PLATFORMS)(
    '%s: a reset missed with its link events ends links and scan as the loss does',
    async platform => {
      await withLaggingBackend(platform, async ({ backend, stage, control }) => {
        const events = backend.events()[Symbol.asyncIterator]()
        const generation = backend.identity.attachment.backendGeneration
        const { lease } = await connectAndDiscover(backend, stage)
        const scan = await backend.scanner.start(scanOptions(), 'client-1')
        const observations = scan.observations[Symbol.asyncIterator]()
        control.armed.add('takeLifecycleEvent')
        control.armed.add('takeAdapterResetEvent')
        control.armed.add('takeScanTerminalEvent')
        await stage.stageAdapterReset('removed')
        let item
        do {
          item = await nextItem(observations, 5000)
        } while (item.kind === 'value')
        expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
        const seen = []
        const deadline = Date.now() + 3000
        while (Date.now() < deadline && backend.identity.attachment.backendGeneration === generation) {
          const next = await nextItem(events, 3000)
          if (next.kind === 'value') seen.push(next.value)
        }
        await eventuallySwallowed(control, 'takeAdapterResetEvent')
        seen.push(...(await drainFor(events, 200)).filter(entry => entry.kind === 'value').map(entry => entry.value))
        expect(backend.identity.attachment.backendGeneration).not.toBe(generation)
        expect(seen.map(event => event.kind)).not.toContain('connection-lost')
        const ended = seen.filter(event => event.kind === 'connection-state-changed')
        if (ADAPTER_LOSS[platform].adapterReason) {
          expect(ended).toEqual([
            expect.objectContaining({ current: 'lost', reason: 'adapter', previous: 'connected' })
          ])
          expect(ended[0].connection.connectionId).toBe(lease.connection.connectionId)
        } else {
          expect(ended).toEqual([])
        }
        expect(seen.some(event => event.kind === 'backend-restarted')).toBe(ADAPTER_LOSS[platform].restarted)
      })
    }
  )
})

describe('scan-terminal lag reconciles from the core (N5)', () => {
  test.each(PLATFORMS)('%s: a scan the OS ended while lagged ends its streams source-failed', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      control.armed.add('takeScanTerminalEvent')
      await stage.stageScanTerminated(false, 'the OS stopped the scan')
      let item
      do {
        item = await nextItem(iterator, 5000)
      } while (item.kind === 'value')
      expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
      await eventuallySwallowed(control, 'takeScanTerminalEvent')
      const next = await backend.scanner.start(scanOptions(), 'client-1')
      await next.stop()
    })
  })

  test('a lag while the scan is still owned leaves its stream open', async () => {
    await withLaggingBackend('winrt', async ({ backend, stage, control }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      control.lagWithoutEvents = 'takeScanTerminalEvent'
      await eventuallyLagged(control)
      await stage.stageAdvertisement({ peerId: 'peer-3', rssi: -50, localName: 'still scanning' })
      expect((await nextValue(iterator, 5000)).device.id).toBeDefined()
      await iterator.return?.()
      expect(await lease.stop()).toEqual({ state: 'released', failures: [] })
    })
  })
})

describe('security and write-readiness lag re-read the OS state (N5)', () => {
  test.each(['winrt', 'bluez'])('%s: a bond change missed while lagged is re-read for each watch', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const peerId = String(await observePeer(backend, stage))
      await stage.stageSecurity('peer-1', 'not-bonded', true)
      const watch = backend.security.watch(peerId)[Symbol.asyncIterator]()
      expect(await nextValue(watch, 5000)).toMatchObject({ kind: 'state', peerId, state: { bond: 'not-bonded' } })
      control.armed.add('takeSecurityEvent')
      await stage.stagePairOutcome('peer-1', 'paired')
      await backend.security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'le',
        protection: 'system-default',
        ceremony: 'system'
      })
      expect(await nextValue(watch, 5000)).toMatchObject({ kind: 'state', peerId, state: { bond: 'bonded' } })
      await eventuallySwallowed(control, 'takeSecurityEvent')
    })
  })

  test('CoreBluetooth: a readiness change missed while lagged is re-read for each watch', async () => {
    await withLaggingBackend('corebluetooth', async ({ backend, stage, control }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const watch = await backend.connections.writeWithoutResponseReadiness(lease.connection)
      const events = watch.events[Symbol.asyncIterator]()
      expect(await nextValue(events, 3000)).toMatchObject({ ready: false, ordinal: 1 })
      control.armed.add('takeWriteReadinessEvent')
      await stage.stageWriteReadiness('peer-1', true, true)
      expect(await nextValue(events, 5000)).toMatchObject({ ready: true, ordinal: 2 })
      await eventuallySwallowed(control, 'takeWriteReadinessEvent')
      await watch.close()
    })
  })
})

describe('a malformed notification value ends the stream source-failed (N6)', () => {
  test.each(PLATFORMS)('%s', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      control.pollOverride = { kind: 'value', value: 'not bytes' }
      await stage.stageNotification({
        peerId: 'peer-1',
        serviceUuid: HRM_SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: HRM_MEASUREMENT,
        characteristicOccurrence: 0,
        value: Buffer.from([0x01, 0x02])
      })
      expect(await nextItem(values, 5000)).toMatchObject({
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'protocol.malformed' }
      })
      expect(control.pollOverride).toBeNull()
    })
  })
})

describe('values the core still holds reach the stream before its lifecycle terminal (LEGACY-AUDIT-4 R2)', () => {
  const CASES = PLATFORMS.flatMap(platform => [
    [platform, 'link loss', stage => stage.stageLinkLoss('peer-1'), 'connection-lost', 'connection-lost'],
    [platform, 'services changed', stage => stage.stageServicesChanged('peer-1'), 'database-changed', 'service-changed']
  ])
  test.each(CASES)('%s, %s: queued values, then the terminal', async (platform, _name, trigger, eventKind, reason) => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      control.holdPolls = true
      for (const byte of [0x01, 0x02, 0x03]) {
        await stage.stageNotification({
          peerId: 'peer-1',
          serviceUuid: HRM_SERVICE,
          serviceOccurrence: 0,
          characteristicUuid: HRM_MEASUREMENT,
          characteristicOccurrence: 0,
          value: Buffer.from([byte])
        })
      }
      await new Promise(resolve => setTimeout(resolve, 30))
      await trigger(stage)
      await nextEvent(events, event => event.kind === eventKind, 5000)
      control.holdPolls = false
      // Nobody is reading while the pump drains and ends the stream: the
      // values must still be there, ahead of the terminal, when read later.
      await new Promise(resolve => setTimeout(resolve, 100))
      const delivered = []
      for (;;) {
        const item = await nextItem(values, 5000)
        if (item.kind !== 'value') {
          expect(item).toMatchObject({ kind: 'terminal', reason })
          break
        }
        delivered.push(item.value.value[0])
      }
      expect(delivered).toEqual([0x01, 0x02, 0x03])
    })
  })
})

describe('the addon wakes the provider instead of waiting for the next poll (LEGACY-AUDIT-4 R2)', () => {
  test.each(PLATFORMS)('%s: a notification and a link loss each wake the provider', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      const beforeValue = control.wakes
      await stage.stageNotification({
        peerId: 'peer-1',
        serviceUuid: HRM_SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: HRM_MEASUREMENT,
        characteristicOccurrence: 0,
        value: Buffer.from([0x2a])
      })
      expect([...(await nextValue(values, 5000)).value]).toEqual([0x2a])
      expect(control.wakes).toBeGreaterThan(beforeValue)
      const beforeLoss = control.wakes
      await stage.stageLinkLoss('peer-1')
      await nextEvent(events, event => event.kind === 'connection-lost', 5000)
      expect(control.wakes).toBeGreaterThan(beforeLoss)
      expect(stage.eventWakeFailures()).toBe(0)
    })
  })
})

// LEGACY-AUDIT-4 R2 / finding 118: every event kind is wake-driven, not only
// values and lifecycle; the poll interval is a safety net.
describe('security, write-readiness and scan-end reports wake the provider (finding 118)', () => {
  async function woke(control, before, timeoutMs = 2000) {
    const deadline = Date.now() + timeoutMs
    while (control.wakes === before) {
      if (Date.now() > deadline) return false
      await new Promise(resolve => setTimeout(resolve, 1))
    }
    return true
  }

  test.each(['winrt', 'bluez'])('%s: a bond change wakes and reaches the security watch', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const peerId = String(await observePeer(backend, stage))
      await stage.stageSecurity('peer-1', 'not-bonded', true)
      const watch = backend.security.watch(peerId)[Symbol.asyncIterator]()
      expect(await nextValue(watch, 5000)).toMatchObject({ state: { bond: 'not-bonded' } })
      await stage.stagePairOutcome('peer-1', 'paired')
      const before = control.wakes
      await backend.security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'le',
        protection: 'system-default',
        ceremony: 'system'
      })
      expect(await woke(control, before)).toBe(true)
      expect(await nextValue(watch, 5000)).toMatchObject({ kind: 'state', state: { bond: 'bonded' } })
    })
  })

  test('CoreBluetooth: a readiness report wakes and reaches the watch', async () => {
    await withLaggingBackend('corebluetooth', async ({ backend, stage, control }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const watch = await backend.connections.writeWithoutResponseReadiness(lease.connection)
      const events = watch.events[Symbol.asyncIterator]()
      expect(await nextValue(events, 3000)).toMatchObject({ ready: false })
      const before = control.wakes
      await stage.stageWriteReadiness('peer-1', true, true)
      expect(await woke(control, before)).toBe(true)
      expect(await nextValue(events, 5000)).toMatchObject({ ready: true })
      await watch.close()
    })
  })

  test.each(PLATFORMS)('%s: an OS-ended scan wakes and ends its stream', async platform => {
    await withLaggingBackend(platform, async ({ backend, stage, control }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      const before = control.wakes
      await stage.stageScanTerminated(true, 'the radio was turned off')
      expect(await woke(control, before)).toBe(true)
      let item
      do {
        item = await nextItem(iterator, 5000)
      } while (item.kind === 'value')
      expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    })
  })
})
