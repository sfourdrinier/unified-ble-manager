'use strict'

// LEGACY-AUDIT-1 #57–60 and #64 on the Rust path (real N-API addon,
// synthetic radio modelling each OS's legacy admission and adapter-loss
// teardown): the adapter-loss sequence per OS, the admission errors, the
// CoreBluetooth first-usable-state wait, resetting / unsupported states and
// the merged duplicate policy. Parity rows: adapter.loss-teardown,
// adapter.admission-errors, adapter.first-state-wait,
// adapter.power-resetting-unsupported, scan.duplicate-merged.

const {
  connectAndDiscover,
  nextEvent,
  nextItem,
  openStagedBackend,
  realBinding,
  scanOptions,
  subscribeOptions
} = require('../../helpers/desktop-rust-core-harness')
const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

jest.setTimeout(30000)

const poweredOn = central => central.stageAdapterState('powered-on')

async function withStaged(platform, stage, run, options = {}) {
  const opened = await openStagedBackend(platform, stage, options)
  try {
    return await run(opened)
  } finally {
    await opened.backend.destroy()
  }
}

/** Wait until the core has applied a staged adapter fact (its event loop runs asynchronously). */
async function coreSees(stage, predicate) {
  const until = Date.now() + 5000
  while (!predicate(stage.adapterStatus()) && Date.now() < until) {
    await new Promise(resolve => setTimeout(resolve, 5))
  }
  expect(predicate(stage.adapterStatus())).toBe(true)
}

async function generationAdvanced(backend, generation) {
  const until = Date.now() + 5000
  while (backend.identity.attachment.backendGeneration === generation && Date.now() < until) {
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  return backend.identity.attachment.backendGeneration !== generation
}

// What each legacy backend announced (corebluetooth-backend.ts,
// winrt-backend.ts, bluez-backend-runtime.ts).
const LEGACY_SEQUENCE = {
  corebluetooth: { adapterReason: true, restarted: true },
  winrt: { adapterReason: true, restarted: false },
  bluez: { adapterReason: false, restarted: true }
}

// The adapter powering off, and a reset the power state does not show
// (adapter removed, bluetoothd restarted) that only the core's reset reports.
const TRIGGERS = {
  'powered-off': stage => stage.stageAdapterState('powered-off', true),
  'daemon-restarted': stage => stage.stageAdapterReset('daemon-restarted'),
  removed: stage => stage.stageAdapterReset('removed'),
  'authorization revoked': stage => stage.stageAdapterAuthorization('denied', true)
}

const CASES = Object.keys(LEGACY_SEQUENCE).flatMap(platform =>
  Object.keys(TRIGGERS).map(trigger => [platform, trigger])
)

describe('adapter loss follows the legacy per-OS sequence (LEGACY-AUDIT-1 #57)', () => {
  test.each(CASES)('%s, %s', async (platform, trigger) => {
    const sequence = LEGACY_SEQUENCE[platform]
    await withStaged(platform, poweredOn, async ({ backend, stage }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const generation = backend.identity.attachment.backendGeneration
      const { lease, database, measurement, peerId } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      const scan = await backend.scanner.start(scanOptions(), 'client-1')
      const observations = scan.observations[Symbol.asyncIterator]()
      await TRIGGERS[trigger](stage)
      expect(await nextItem(observations, 5000)).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
      expect(await nextItem(values, 5000)).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
      if (sequence.adapterReason) {
        const ended = await nextEvent(events, event => event.kind === 'connection-state-changed', 5000)
        expect(ended).toMatchObject({ current: 'lost', reason: 'adapter', previous: 'connected' })
        expect(ended.connection.connectionId).toBe(lease.connection.connectionId)
      }
      if (sequence.restarted) {
        const restarted = await nextEvent(events, event => event.kind === 'backend-restarted', 5000)
        expect(restarted.attachment.backendGeneration).not.toBe(generation)
      }
      expect(await generationAdvanced(backend, generation)).toBe(true)
      // Handles minted before the reset are stale: the database.
      await expect(database.read(measurement.path, { signal: null, deadline: null })).rejects.toMatchObject({
        normalized: { code: 'gatt.stale-handle' }
      })
      // 5.0 keeps the peer handle across the loss so a supervisor can
      // reconnect (legacy cleared it, W-R3, and a connect was
      // `connection.not-found`): the adapter's own admission answers now.
      const attempt = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null }).then(
        async lease => {
          await lease.release()
          return null
        },
        error => error.normalized.code
      )
      expect(attempt).not.toBe('connection.not-found')
      expect(backend.resourceCounters()).toMatchObject({ activeScanControllers: 0, subscriptionConsumers: 0 })
      if (!sequence.adapterReason) {
        // BlueZ legacy invalidated links silently (operation.reset).
        const extra = []
        for (let index = 0; index < 5; index += 1) {
          const item = await nextItem(events, 100).catch(() => null)
          if (item?.kind === 'value') extra.push(item.value.kind)
        }
        expect(extra).not.toContain('connection-state-changed')
      }
    })
  })

  test('an operation in flight when the adapter is lost settles operation.reset', async () => {
    await withStaged('corebluetooth', poweredOn, async ({ backend, stage }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      await stage.blockRadioOp('read')
      const read = database.read(measurement.path, { signal: null, deadline: null })
      // The rejection is observed from the start: the reset can settle the
      // read while `stageAdapterReset` is still in flight, and a rejection
      // with no handler yet fails the test as unhandled (finding 94).
      const settled = expect(read).rejects.toMatchObject({ normalized: { code: 'operation.reset' } })
      await new Promise(resolve => setTimeout(resolve, 50))
      await stage.stageAdapterReset('removed')
      await settled
      await stage.unblockRadioOp('read')
    })
  })

  test('the adapter-state watch reports the loss and the advanced generation', async () => {
    await withStaged('bluez', poweredOn, async ({ backend, stage }) => {
      const watch = await backend.adapter.watchState()
      const transitions = watch.transitions[Symbol.asyncIterator]()
      await stage.stageAdapterState('powered-off', true)
      const seen = []
      for (let index = 0; index < 3; index += 1) {
        const item = await nextItem(transitions, 2000).catch(() => null)
        if (item?.kind === 'value') seen.push(item.value)
      }
      expect(seen.some(state => state.power === 'off')).toBe(true)
      expect(seen.at(-1).backendGeneration).not.toBe(watch.initial.backendGeneration)
    })
  })
})

describe('admission errors (LEGACY-AUDIT-1 #58)', () => {
  test.each([
    ['corebluetooth', 'powered-off', 'adapter.powered-off'],
    ['corebluetooth', 'resetting', 'adapter.resetting'],
    ['winrt', 'powered-off', 'adapter.powered-off'],
    ['winrt', 'resetting', 'adapter.resetting']
  ])('%s adapter %s refuses radio work with %s before any effect', async (platform, state, code) => {
    await withStaged(platform, poweredOn, async ({ backend, stage }) => {
      await stage.stageAdapterState(state, true)
      await coreSees(stage, status => status.power === state)
      // Refused before the radio: nothing was dispatched, the caller may retry.
      await expect(backend.scanner.start(scanOptions(), 'client-1')).rejects.toMatchObject({
        normalized: { code, domain: 'adapter', retryability: 'caller-decides', commit: 'not-dispatched' }
      })
    })
  })

  test.each([
    ['denied', 'permission.denied'],
    ['restricted', 'permission.restricted'],
    ['not-determined', 'permission.not-determined']
  ])('CoreBluetooth authorization %s refuses with %s', async (authorization, code) => {
    await withStaged('corebluetooth', poweredOn, async ({ backend, stage }) => {
      await stage.stageAdapterAuthorization(authorization, true)
      await coreSees(stage, status => status.authorization === authorization)
      await expect(backend.scanner.start(scanOptions(), 'client-1')).rejects.toMatchObject({
        normalized: { code, domain: 'adapter' }
      })
    })
  })

  test('BlueZ keeps its legacy lifecycle-only admission: a powered-off adapter is not pre-refused', async () => {
    await withStaged('bluez', poweredOn, async ({ backend, stage }) => {
      await stage.stageAdapterState('powered-off', true)
      await coreSees(stage, status => status.power === 'powered-off')
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      await lease.stop()
    })
  })
})

describe('CoreBluetooth first usable state (LEGACY-AUDIT-1 #59)', () => {
  function provider(harness, firstStateTimeoutMs) {
    return createTestDesktopRustCoreBackendProvider({
      platform: 'corebluetooth',
      owner: 'first-state',
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: 'darwin',
      firstStateTimeoutMs
    })
  }

  test('no usable state before the deadline is capability.unavailable / adapter-initialization-timed-out', async () => {
    const harness = realBinding('corebluetooth')
    harness.stageFirstState = null
    await expect(provider(harness, 200).listAdapters()).rejects.toMatchObject({
      normalized: {
        code: 'capability.unavailable',
        domain: 'platform',
        platform: { domain: 'corebluetooth', code: 'adapter-initialization-timed-out' }
      }
    })
  })

  test('a powered-off adapter at creation times out too (legacy: not usable)', async () => {
    const harness = realBinding('corebluetooth')
    harness.stageFirstState = 'powered-off'
    await expect(provider(harness, 200).listAdapters()).rejects.toMatchObject({
      normalized: { platform: { code: 'adapter-initialization-timed-out' } }
    })
  })

  test('a usable state reported late is accepted within the deadline', async () => {
    const harness = realBinding('corebluetooth')
    harness.stageFirstState = null
    const original = harness.binding.openSynthetic
    harness.binding.openSynthetic = async (owner, options) => {
      const central = await original(owner, options)
      const native = harness.opened[harness.opened.length - 1]
      setTimeout(() => native.stageAdapterState('powered-on', true), 50)
      return central
    }
    const adapters = await provider(harness, 5000).listAdapters()
    expect(adapters[0].state.power).toBe('on')
  })

  test.each(['bluez', 'winrt'])('%s does not wait (legacy had no first-state wait)', async platform => {
    const harness = realBinding(platform)
    const adapters = await createTestDesktopRustCoreBackendProvider({
      platform,
      owner: 'no-wait',
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: platform === 'bluez' ? 'linux' : 'win32',
      firstStateTimeoutMs: 1
    }).listAdapters()
    expect(adapters).toHaveLength(1)
  })
})

describe('resetting and unsupported adapter states (LEGACY-AUDIT-1 #60)', () => {
  test('CoreBluetooth resetting, unsupported and unauthorized map to the legacy snapshot', async () => {
    await withStaged('corebluetooth', poweredOn, async ({ backend, stage }) => {
      await stage.stageAdapterState('resetting', true)
      expect(await backend.adapter.currentState()).toMatchObject({ availability: 'available', power: 'resetting' })
      await stage.stageAdapterState('unsupported', true)
      expect(await backend.adapter.currentState()).toMatchObject({
        availability: 'unsupported',
        power: 'unsupported',
        authorization: 'unavailable'
      })
      await stage.stageAdapterState('unauthorized', true)
      expect(await backend.adapter.currentState()).toMatchObject({ authorization: 'denied', power: 'unknown' })
    })
  })
})

describe("duplicatePolicy 'merged' (LEGACY-AUDIT-1 #64)", () => {
  test.each(['bluez', 'corebluetooth', 'winrt'])(
    '%s accepts merged and carries the policy to the OS scan',
    async platform => {
      await withStaged(platform, poweredOn, async ({ backend, stage }) => {
        const lease = await backend.scanner.start(scanOptions({ duplicatePolicy: 'merged' }), 'client-1')
        try {
          await stage.stageAdvertisement({ peerId: 'peer-1', localName: 'A' })
          expect(await nextItem(lease.observations[Symbol.asyncIterator](), 5000)).toMatchObject({ kind: 'value' })
        } finally {
          await lease.stop()
        }
        expect(await stage.stagedScanDuplicatePolicies()).toEqual(['merged'])
      })
    }
  )

  test('all and first reach the OS scan as themselves', async () => {
    await withStaged('bluez', poweredOn, async ({ backend, stage }) => {
      for (const policy of ['all', 'first']) {
        const lease = await backend.scanner.start(scanOptions({ duplicatePolicy: policy }), 'client-1')
        await lease.stop()
      }
      expect(await stage.stagedScanDuplicatePolicies()).toEqual(['all', 'first'])
    })
  })
})
