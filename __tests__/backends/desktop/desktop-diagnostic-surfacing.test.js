'use strict'

// Finding 102 over the REAL addon on its synthetic radio: every
// `diagnostic-warning` the desktop Rust provider emits reaches the public API.
// A fact with a typed home (an observation dropped from a stream, a pump that
// stopped, core events that were lost) surfaces on that typed channel; every
// warning also enters the public diagnostic trace
// (`manager.diagnostics.snapshot().trace`, through UnifiedBleCore) as
// `diagnostic-warning:<code>`.
//
// The only double is a wrapper on the native central that replaces or fails
// one `take*` / `poll*` answer; every other fact is the Rust core's own.

const path = require('node:path')

const {
  connectAndDiscover,
  nextEvent,
  nextItem,
  realBinding,
  scanOptions,
  subscribeOptions,
  HOST_PLATFORM,
  HRM_MEASUREMENT,
  HRM_SERVICE
} = require('../../helpers/desktop-rust-core-harness')
const { createDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']
const FACTORIES = {
  bluez: { os: 'linux', module: 'node-bluez', factory: 'createBluezBleManager' },
  corebluetooth: { os: 'darwin', module: 'node-corebluetooth', factory: 'createCoreBluetoothBleManager' },
  winrt: { os: 'win32', module: 'node-winrt', factory: 'createWinRtBleManager' }
}

/**
 * Wraps every central the harness opens. `control.replace[method]` is a queue
 * of answers served before the real ones; `control.throwOnce[method]` throws
 * once; `control.fail[method]` swallows the real events queued now, then
 * throws once; `control.advertisement` rewrites
 * the advertisement of the next real scan observation; `control.poll` replaces the next real
 * notification value poll.
 */
function interpose(harness) {
  const control = { replace: {}, fail: new Set(), throwOnce: new Set(), swallowed: {}, advertisement: null, poll: null }
  const wrap = central =>
    new Proxy(central, {
      get(target, property) {
        const value = Reflect.get(target, property)
        if (typeof value !== 'function') return value
        if (property === 'takeScanObservation') {
          return async (...args) => {
            const real = await Reflect.apply(value, target, args)
            if (real === null || real === undefined || control.advertisement === null) return real
            const rewrite = control.advertisement
            control.advertisement = null
            return { ...real, advertisement: rewrite(real.advertisement) }
          }
        }
        if (property === 'pollNotification') {
          return async (...args) => {
            const real = await Reflect.apply(value, target, args)
            if (control.poll === null || real.kind !== 'value') return real
            const replaced = control.poll
            control.poll = null
            return replaced
          }
        }
        if (typeof property === 'string' && property.startsWith('take') && property.endsWith('Event')) {
          return async () => {
            const queued = control.replace[property]
            if (queued !== undefined && queued.length > 0) return queued.shift()
            if (control.throwOnce.delete(property)) throw new Error(`${property} failed`)
            if (!control.fail.has(property)) return Reflect.apply(value, target, [])
            let swallowed = 0
            for (;;) {
              const event = await Reflect.apply(value, target, [])
              if (event === null || event === undefined) break
              swallowed += 1
            }
            if (swallowed === 0) return null
            control.fail.delete(property)
            control.swallowed[property] = swallowed
            throw new Error(`${property} failed after swallowing ${swallowed} event(s)`)
          }
        }
        return (...args) => Reflect.apply(value, target, args)
      }
    })
  const openSynthetic = harness.binding.openSynthetic
  const openProduction = harness.binding.openProduction
  harness.binding.openSynthetic = async (owner, options) => wrap(await openSynthetic(owner, options))
  harness.binding.openProduction = async options => wrap(await openProduction(options))
  return control
}

function withPlatform(platform, run) {
  const original = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: platform })
  const restore = () => Object.defineProperty(process, 'platform', original)
  let result
  try {
    result = run()
  } catch (error) {
    restore()
    throw error
  }
  return Promise.resolve(result).finally(restore)
}

async function publicManager(platform) {
  const harness = realBinding(platform)
  const control = interpose(harness)
  const { os, module, factory } = FACTORIES[platform]
  const load = require(path.join('..', '..', '..', 'src', module))
  const manager = await withPlatform(os, () => load[factory]({ binding: harness.binding }))
  return { manager, control, harness }
}

async function openBackend(platform) {
  const harness = realBinding(platform)
  const control = interpose(harness)
  const provider = createDesktopRustCoreBackendProvider({
    platform,
    owner: `diagnostics-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: HOST_PLATFORM[platform]
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  return { backend, stage: harness.opened[harness.opened.length - 1], control }
}

async function withBackend(platform, run) {
  const opened = await openBackend(platform)
  try {
    return await run(opened)
  } finally {
    await opened.backend.destroy()
  }
}

async function eventually(check, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const value = check()
    if (value) return value
    if (Date.now() > deadline) throw new Error(`${label} never happened`)
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

function traceEvents(manager) {
  return manager.diagnostics.snapshot().trace.records.map(record => record.event)
}

const LAGGED = Object.freeze({ kind: 'lagged', missed: 1 })

describe('every desktop diagnostic warning reaches the public diagnostic trace', () => {
  test.each(PLATFORMS)('%s: reconciled lags of every core event stream', async platform => {
    const { manager, control } = await publicManager(platform)
    try {
      control.replace.takeLifecycleEvent = [LAGGED]
      control.replace.takeSecurityEvent = [LAGGED]
      control.replace.takeWriteReadinessEvent = [LAGGED]
      control.replace.takeScanTerminalEvent = [LAGGED]
      control.replace.takeAdapterEvent = [LAGGED]
      const expected = [
        'diagnostic-warning:lifecycle-events-lagged',
        'diagnostic-warning:security-events-lagged',
        'diagnostic-warning:write-readiness-events-lagged',
        'diagnostic-warning:scan-terminal-events-lagged',
        'diagnostic-warning:adapter-events-lagged'
      ]
      await eventually(
        () => expected.every(event => traceEvents(manager).includes(event)),
        `${expected.join(', ')} in the trace`
      )
      expect(manager.diagnostics.snapshot().trace.records.find(record => record.event === expected[0])).toMatchObject({
        kind: 'attachment',
        cause: null,
        redactedPayload: true
      })
    } finally {
      await manager.destroy()
    }
  })

  test.each(PLATFORMS)('%s: a reconciled adapter-reset lag', async platform => {
    const { manager, control } = await publicManager(platform)
    try {
      control.replace.takeAdapterResetEvent = [LAGGED]
      await eventually(
        () => traceEvents(manager).includes('diagnostic-warning:adapter-reset-events-lagged'),
        'the adapter-reset lag warning'
      )
    } finally {
      await manager.destroy()
    }
  })

  test.each(PLATFORMS)('%s: a failed core event pump, with the core error as cause', async platform => {
    const { manager, control } = await publicManager(platform)
    try {
      control.throwOnce.add('takeSecurityEvent')
      const record = await eventually(
        () =>
          manager.diagnostics
            .snapshot()
            .trace.records.find(entry => entry.event === 'diagnostic-warning:core-event-pump-failed'),
        'the event-pump warning'
      )
      expect(record.cause).toBe('platform.transport')
    } finally {
      await manager.destroy()
    }
  })
})

describe('a lost core event stream is typed, never diagnostic-only', () => {
  test.each(PLATFORMS)('%s: a closed lifecycle stream fails the backend event source', async platform => {
    await withBackend(platform, async ({ backend, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      control.replace.takeLifecycleEvent = [{ kind: 'closed' }]
      const warning = await nextItem(events, 5000)
      expect(warning).toMatchObject({
        kind: 'value',
        value: { kind: 'diagnostic-warning', code: 'lifecycle-events-closed' }
      })
      expect(await nextItem(events, 5000)).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    })
  })

  test.each(PLATFORMS)(
    '%s: through the public manager, a closed stream fails the manager event source',
    async platform => {
      const { manager, control } = await publicManager(platform)
      try {
        control.replace.takeAdapterResetEvent = [{ kind: 'closed' }]
        await eventually(
          () =>
            traceEvents(manager).includes('diagnostic-warning:adapter-reset-events-closed') &&
            traceEvents(manager).includes('backend-event-stream-terminal'),
          'the closed-stream warning and the event-source terminal'
        )
      } finally {
        await manager.destroy()
      }
    }
  )

  test.each(PLATFORMS)('%s: a pump failure that lost a link loss is reconciled to connection-lost', async platform => {
    await withBackend(platform, async ({ backend, stage, control }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { lease } = await connectAndDiscover(backend, stage)
      control.fail.add('takeLifecycleEvent')
      await stage.stageLinkLoss('peer-1')
      const lost = await nextEvent(events, event => event.kind === 'connection-lost', 5000)
      expect(lost.connection.connectionId).toEqual(lease.connection.connectionId)
      expect(control.swallowed.takeLifecycleEvent).toBeGreaterThan(0)
    })
  })
})

describe('a dropped or stranded observation is typed on its stream', () => {
  test.each(PLATFORMS)('%s: a malformed advertisement counts in the scan drop accounting', async platform => {
    await withBackend(platform, async ({ backend, stage, control }) => {
      const scan = await backend.scanner.start(scanOptions(), 'client-1')
      const observations = scan.observations[Symbol.asyncIterator]()
      control.advertisement = real => ({ ...real, peerId: '', address: null })
      await stage.stageAdvertisement({ peerId: 'peer-1', localName: 'dropped' })
      expect(await nextItem(observations, 5000)).toMatchObject({ kind: 'overflow', droppedItems: 1 })
      await stage.stageAdvertisement({ peerId: 'peer-1', localName: 'kept' })
      expect(await nextItem(observations, 5000)).toMatchObject({ kind: 'value' })
      expect((await scan.stop()).state).toBe('released')
    })
  })

  test.each(PLATFORMS)('%s: a scan pump that stops ends its streams source-failed', async platform => {
    await withBackend(platform, async ({ backend, stage, control }) => {
      const scan = await backend.scanner.start(scanOptions(), 'client-1')
      const observations = scan.observations[Symbol.asyncIterator]()
      control.advertisement = real => {
        const broken = { ...real }
        Object.defineProperty(broken, 'address', {
          enumerable: true,
          get() {
            throw new Error('unreadable advertisement')
          }
        })
        return broken
      }
      await stage.stageAdvertisement({ peerId: 'peer-1', localName: 'breaks' })
      let item
      do {
        item = await nextItem(observations, 5000)
      } while (item.kind === 'value')
      expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    })
  })

  test.each(PLATFORMS)('%s: a notification pump that stops ends its stream source-failed', async platform => {
    await withBackend(platform, async ({ backend, stage, control }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const notifications = subscription.values[Symbol.asyncIterator]()
      control.poll = {
        get kind() {
          throw new Error('unreadable poll')
        }
      }
      await stage.stageNotification({
        peerId: 'peer-1',
        serviceUuid: HRM_SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: HRM_MEASUREMENT,
        characteristicOccurrence: 0,
        value: Buffer.from([0x00, 0x48])
      })
      let item
      do {
        item = await nextItem(notifications, 5000)
      } while (item.kind === 'value')
      expect(item).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    })
  })
})
