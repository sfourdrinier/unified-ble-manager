'use strict'

// PR210-02 / 30 / 31 + desktop parity: the one desktop Rust provider over
// the REAL N-API addon on its synthetic radio. Every operation here executes
// DesktopCentral in Rust (the spy only records which UbmCentral methods
// TypeScript called). Legacy-equivalent behaviour each row of
// DESKTOP_RUST_CORE_PARITY marks `implemented` is proven here; `blocked`
// rows fail in desktop-parity-blockers.test.js.

const {
  DESKTOP_RUST_CORE_PROFILES,
  assertDesktopRustCorePlatform,
  createDesktopRustCoreBackendProvider
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const {
  cleanupRecordFromCloseReport,
  desktopRustCoreError,
  parseDesktopRustCoreWireError
} = require('../../../src/backends/desktop/desktop-rust-core-binding')
const { DESKTOP_RUST_CORE_PARITY } = require('../../../src/backends/desktop/desktop-rust-core-parity')
const { normalizeScanQuery } = require('../../../src/public/scan-query')
const { BUILT_IN_FEATURE_IDS } = require('../../../src/backend-contract/capabilities')
const {
  HRM_MEASUREMENT,
  HRM_SERVICE,
  callNames,
  connectAndDiscover,
  dispatchCalls,
  drainFor,
  nextEvent,
  nextItem,
  nextValue,
  hrmServices,
  observePeer,
  openBackend,
  realBinding,
  scanOptions,
  subscribeOptions
} = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']
/** The legacy 4.x backend and provider ids the Rust path keeps (LEGACY-AUDIT-1 #67). */
const LEGACY_IDS = Object.freeze({
  bluez: { backendId: 'unified-ble:bluez-dbus', providerId: 'unified-ble:bluez-dbus-provider' },
  corebluetooth: { backendId: 'unified-ble:corebluetooth', providerId: 'unified-ble:corebluetooth-provider' },
  winrt: { backendId: 'unified-ble:winrt', providerId: 'unified-ble:winrt-provider' }
})

async function withBackend(platform, run) {
  const opened = await openBackend(platform)
  try {
    return await run(opened)
  } finally {
    const cleanup = await opened.backend.destroy()
    expect(cleanup).toEqual({ state: 'released', failures: [] })
  }
}

describe('profiles and the pre-load platform guard (PR210-02, PR210-29)', () => {
  test.each(PLATFORMS)('%s profile carries its own identity, compatibility and required OS', platform => {
    const profile = DESKTOP_RUST_CORE_PROFILES[platform]
    expect(profile.backendId).toBe(LEGACY_IDS[platform].backendId)
    expect(profile.providerId).toBe(LEGACY_IDS[platform].providerId)
    expect({ bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' }[platform]).toBe(profile.requiredProcessPlatform)
  })

  test.each(PLATFORMS)('%s refuses a foreign host before anything loads', platform => {
    const loadBinding = jest.fn()
    const foreign = platform === 'bluez' ? 'darwin' : 'linux'
    let thrown = null
    try {
      createDesktopRustCoreBackendProvider({
        platform,
        owner: 'guard',
        now: () => 1,
        loadBinding,
        hostPlatform: foreign
      })
    } catch (error) {
      thrown = error
    }
    expect(thrown).toMatchObject({
      normalized: {
        code: 'capability.unavailable',
        domain: 'platform',
        operation: `${DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix}.native-boundary.load`,
        platform: { code: DESKTOP_RUST_CORE_PROFILES[platform].requiredPlatformCode }
      }
    })
    expect(loadBinding).not.toHaveBeenCalled()
  })

  test('the guard defaults to the running process platform', () => {
    const own = PLATFORMS.find(
      platform => DESKTOP_RUST_CORE_PROFILES[platform].requiredProcessPlatform === process.platform
    )
    if (own !== undefined) expect(() => assertDesktopRustCorePlatform(own)).not.toThrow()
    const foreign = PLATFORMS.find(
      platform => DESKTOP_RUST_CORE_PROFILES[platform].requiredProcessPlatform !== process.platform
    )
    expect(() => assertDesktopRustCorePlatform(foreign)).toThrow()
  })

  test('the addon itself refuses a platform it does not drive (Rust-side check)', async () => {
    const { loadAddon } = require('../../helpers/desktop-rust-core-harness')
    const addon = loadAddon()
    const foreign = process.platform === 'darwin' ? 'bluez' : 'corebluetooth'
    await expect(addon.UbmCentral.open({ owner: 'platform-probe', platform: foreign })).rejects.toThrow(
      /^capability\.unavailable\|platform\|dispatch\.open\.platform\|never\|\|/
    )
    await expect(addon.UbmCentral.open({ owner: 'platform-probe', platform: 'android' })).rejects.toThrow(
      /^argument\.invalid\|core\|dispatch\.open\.platform\|/
    )
  })

  test('a missing packaged core fails loudly with the loader cause, never a fallback', async () => {
    const provider = createDesktopRustCoreBackendProvider({
      platform: 'bluez',
      owner: 'missing',
      now: () => 1,
      hostPlatform: 'linux',
      loadBinding: async () => {
        const error = new Error('no ubm_desktop_core prebuild for linux-x64')
        error.code = 'no-prebuilt-for-target'
        const { loadDesktopCoreBinding } = require('../../../src/desktop-core-addon')
        return loadDesktopCoreBinding({ platform: 'bluez', operationPrefix: 'bluez' }, async () => () => {
          throw error
        })
      }
    })
    await expect(provider.listAdapters()).rejects.toMatchObject({
      normalized: {
        code: 'capability.unavailable',
        domain: 'platform',
        operation: 'bluez.native-boundary.load',
        platform: { code: 'no-prebuilt-for-target' }
      }
    })
  })
})

describe('5.0 read while notifying over the real addon: the result reports what the radio said', () => {
  test.each(PLATFORMS)(
    '%s: a read on a subscribed characteristic runs, carries the radio provenance, and the subscriber still receives values',
    async platform => {
      await withBackend(platform, async ({ backend, stage }) => {
        const { lease, database, measurement } = await connectAndDiscover(backend, stage)
        const subscription = await database.subscribe(measurement.path, subscribeOptions())
        const values = subscription.values[Symbol.asyncIterator]()
        await stage.stageReadProvenance('read-or-notification')
        const fused = await database.read(measurement.path, { signal: null, deadline: null })
        expect(fused.provenance).toBe('read-or-notification')
        expect(fused.value.length).toBeGreaterThan(0)
        const dispatched = await backend.gatt.read(measurement.path, {
          operation: { signal: null, deadline: null, correlation: 'read-while-notifying' }
        }).completion
        expect(dispatched.provenance).toBe('read-or-notification')
        await stage.stageNotification({
          peerId: 'peer-1',
          serviceUuid: HRM_SERVICE,
          serviceOccurrence: 0,
          characteristicUuid: HRM_MEASUREMENT,
          characteristicOccurrence: 0,
          value: Buffer.from([0x0f, 0x01])
        })
        expect([...(await nextValue(values, 5000)).value]).toEqual([0x0f, 0x01])
        await stage.stageReadProvenance('read-response')
        expect((await database.read(measurement.path, { signal: null, deadline: null })).provenance).toBe(
          'read-response'
        )
        expect(await subscription.remove()).toEqual({ state: 'released', failures: [] })
        expect(await lease.release()).toEqual({ state: 'released', failures: [] })
      })
    }
  )
})

describe('real addon, synthetic radio: every verb executes Rust', () => {
  test.each(PLATFORMS)(
    '%s: scan/connect/discover/read/write/subscribe/notify/unsubscribe/disconnect',
    async platform => {
      await withBackend(platform, async ({ backend, stage, harness }) => {
        expect(backend.identity.registeredBackendId).toBe(LEGACY_IDS[platform].backendId)
        expect(backend.identity.runtime.diagnostics).toMatchObject({
          transport: 'napi-UbmCentral',
          platform,
          radio: 'synthetic'
        })
        const { lease, database, measurement, control } = await connectAndDiscover(backend, stage)
        expect(measurement.properties.notify).toBe(true)
        const { value, provenance } = await database.read(measurement.path, { signal: null, deadline: null })
        expect(value.length).toBeGreaterThan(0)
        expect(provenance).toBe('read-response')
        const confirmed = await database.write(control.path, new Uint8Array([0x01]), {
          signal: null,
          deadline: null,
          mode: 'with-response'
        })
        expect(confirmed.commitState).toBe('confirmed')
        const subscription = await database.subscribe(measurement.path, subscribeOptions())
        const values = subscription.values[Symbol.asyncIterator]()
        await stage.stageNotification({
          peerId: 'peer-1',
          serviceUuid: HRM_SERVICE,
          serviceOccurrence: 0,
          characteristicUuid: HRM_MEASUREMENT,
          characteristicOccurrence: 0,
          value: Buffer.from([0x06, 0x40])
        })
        const notification = await nextValue(values, 5000)
        expect([...notification.value]).toEqual([0x06, 0x40])
        expect(await subscription.remove()).toEqual({ state: 'released', failures: [] })
        expect(await lease.release()).toEqual({ state: 'released', failures: [] })
        const counters = backend.dispatchCounters()
        for (const verb of [
          'scanStart',
          'scanStop',
          'connect',
          'discover',
          'read',
          'write',
          'subscribe',
          'unsubscribe',
          'disconnect'
        ]) {
          expect({ verb, count: counters[verb] }).toEqual({ verb, count: expect.any(Number) })
          expect(counters[verb]).toBeGreaterThan(0)
        }
        expect(counters.notificationValues).toBeGreaterThan(0)
        expect(callNames(harness.calls)).toEqual(
          expect.arrayContaining(['pollNotification', 'createTicket', 'releaseTicket'])
        )
      })
    }
  )

  test('a without-response write reports commitState unknown, never confirmed (PR210-31)', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const receipt = await database.write(measurement.path, new Uint8Array([0x02]), {
        signal: null,
        deadline: null,
        mode: 'without-response'
      })
      expect(receipt.commitState).toBe('unknown')
    })
  })

  test('descriptors read and write through the core', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const { database, snapshot } = await connectAndDiscover(backend, stage)
      expect(snapshot.descriptors).toHaveLength(1)
      const [descriptor] = snapshot.descriptors
      const read = await database.readDescriptor(descriptor.path, { signal: null, deadline: null })
      expect(read).toBeInstanceOf(Uint8Array)
      const receipt = await database.writeDescriptor(descriptor.path, new Uint8Array([0x41]), {
        signal: null,
        deadline: null,
        mode: 'with-response'
      })
      expect(receipt.commitState).toBe('confirmed')
    })
  })

  test('a without-response descriptor write fails with the legacy WinRT identity (W-R1)', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const { database, snapshot } = await connectAndDiscover(backend, stage)
      const [descriptor] = snapshot.descriptors
      // Fail-closed as now, but with the legacy code and operation id the
      // native boundary's refusal carried: the mode never reaches the core.
      await expect(
        database.writeDescriptor(descriptor.path, new Uint8Array([0x41]), {
          signal: null,
          deadline: null,
          mode: 'without-response'
        })
      ).rejects.toMatchObject({
        normalized: { code: 'gatt.write-failed', domain: 'gatt', operation: 'winrt.gatt.write-descriptor' }
      })
    })
  })

  test('CoreBluetooth serializes GATT verbs per connection (F6: fail-fast lifecycle.invalid-state)', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      await stage.blockRadioOp('read')
      const first = database.read(measurement.path, { signal: null, deadline: null })
      const firstSettled = first.then(
        () => null,
        error => error
      )
      // A second verb on the same connection fails fast, before any dispatch —
      // as the legacy dispatcher refused it — with the verb's own operation id.
      await expect(database.read(measurement.path, { signal: null, deadline: null })).rejects.toMatchObject({
        normalized: { code: 'lifecycle.invalid-state', operation: 'direct-gatt.gatt.read' }
      })
      await stage.unblockRadioOp('read')
      expect(await firstSettled).toBeNull()
      // Once the first verb settles the connection admits again.
      const retry = await database.read(measurement.path, { signal: null, deadline: null })
      expect(retry.value.length).toBeGreaterThan(0)
    })
  })

  test('the caller budget crosses as timeoutMs and the core owns the timeout outcome', async () => {
    await withBackend('bluez', async ({ backend, stage, harness }) => {
      const peerId = await observePeer(backend, stage)
      await stage.blockRadioOp('connect')
      try {
        await expect(
          backend.connections.connect(peerId, 'client-1', { signal: null, deadline: performance.now() + 250 })
        ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
      } finally {
        await stage.unblockRadioOp('connect')
      }
      const connect = harness.calls.find(([name]) => name === 'connect')
      expect(connect[1][0].timeoutMs).toBeGreaterThan(0)
      expect(connect[1][0].timeoutMs).toBeLessThanOrEqual(250)
    })
  })

  test('no caller deadline sends no budget: the core liveness backstop applies (decision 2)', async () => {
    await withBackend('bluez', async ({ backend, stage, harness }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      const connect = harness.calls.find(([name]) => name === 'connect')
      expect(connect[1][0]).not.toHaveProperty('timeoutMs')
      await lease.release()
    })
  })
})

describe('in-flight cancellation by ticket (parity: operation.cancel-in-flight)', () => {
  test('aborting an admitted read cancels exactly that core operation', async () => {
    await withBackend('corebluetooth', async ({ backend, stage, harness }) => {
      const { measurement } = await connectAndDiscover(backend, stage)
      await stage.blockRadioOp('read')
      const controller = new AbortController()
      const dispatch = backend.gatt.read(measurement.path, {
        operation: { signal: controller.signal, deadline: null, correlation: 'corr-cancel-read' }
      })
      await new Promise(resolve => setTimeout(resolve, 50))
      controller.abort()
      await expect(dispatch.completion).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      await stage.unblockRadioOp('read')
      expect(callNames(harness.calls)).toContain('cancelTicket')
      expect(backend.dispatchCounters().cancel).toBeGreaterThan(0)
    })
  })

  test('requestCancellation on the dispatch reaches the core', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const { measurement } = await connectAndDiscover(backend, stage)
      await stage.blockRadioOp('read')
      const dispatch = backend.gatt.read(measurement.path, {
        operation: { signal: null, deadline: null, correlation: 'corr-request-cancel' }
      })
      const settled = dispatch.completion.then(
        () => null,
        error => error
      )
      await new Promise(resolve => setTimeout(resolve, 50))
      const ack = await dispatch.requestCancellation()
      expect(ack.state).toBe('cancellation-requested')
      expect(await settled).toMatchObject({ normalized: { code: 'operation.aborted' } })
      await stage.unblockRadioOp('read')
    })
  })

  test('an already-aborted signal refuses before any dispatch', async () => {
    await withBackend('bluez', async ({ backend, stage, harness }) => {
      const peerId = await observePeer(backend, stage)
      const before = dispatchCalls(harness.calls).length
      const controller = new AbortController()
      controller.abort()
      await expect(
        backend.connections.connect(peerId, 'client-1', { signal: controller.signal, deadline: null })
      ).rejects.toMatchObject({
        // F4: the pre-admission refusal carries the bare operation id, as
        // the legacy dispatcher reported it — no `.aborted` suffix.
        normalized: { code: 'operation.aborted', operation: 'bluez.connect' }
      })
      expect(dispatchCalls(harness.calls)).toHaveLength(before)
    })
  })
})

describe('scan: plan, native filter, software predicates, share/join (PR210-30)', () => {
  test('scanner.plan pushes required service UUIDs to the core scan', async () => {
    await withBackend('corebluetooth', async ({ backend, harness }) => {
      const query = normalizeScanQuery({ anyOf: [{ services: { all: [HRM_SERVICE] } }] })
      const plan = backend.scanner.plan(query)
      expect(plan.native.predicates.length).toBeGreaterThan(0)
      const lease = await backend.scanner.start(scanOptions({ query, plan }), 'client-1')
      await lease.stop()
      const start = harness.calls.find(([name]) => name === 'startScan')
      expect(start[1][0].serviceUuids).toEqual([HRM_SERVICE])
    })
  })

  test('name-prefix and manufacturer filters match in software instead of failing', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(
        scanOptions({
          filter: {
            serviceUuids: [],
            manufacturerData: [{ companyIdentifier: 107, dataPrefix: null }],
            localNamePrefix: 'Polar'
          }
        }),
        'client-1'
      )
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        await stage.stageAdvertisement({
          peerId: 'other',
          localName: 'Garmin',
          manufacturerData: [{ companyId: 107, payload: Buffer.from([1]) }]
        })
        await stage.stageAdvertisement({ peerId: 'nameless', localName: 'Polar H10' })
        await stage.stageAdvertisement({
          peerId: 'match',
          localName: 'Polar H10',
          manufacturerData: [{ companyId: 107, payload: Buffer.from([2]) }]
        })
        const observation = await nextValue(iterator, 5000)
        expect(observation.localName.value).toBe('Polar H10')
        expect(observation.manufacturerData.value[0].companyIdentifier).toBe(107)
        const rest = await drainFor(iterator, 150)
        expect(rest.filter(item => item.kind === 'value')).toHaveLength(0)
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test('a shared scan fans one core scan out to joined leases; the owner stop ends both', async () => {
    await withBackend('bluez', async ({ backend, stage, harness }) => {
      const owner = await backend.scanner.start(
        scanOptions({ sharing: { mode: 'owner', allowSharing: true } }),
        'client-1'
      )
      expect(owner.shareToken).not.toBeNull()
      const joined = await backend.scanner.join(owner.leaseId, owner.shareToken, 'client-2')
      const ownerItems = owner.observations[Symbol.asyncIterator]()
      const joinedItems = joined.observations[Symbol.asyncIterator]()
      await stage.stageAdvertisement({ peerId: 'peer-shared', localName: 'Shared' })
      expect((await nextValue(ownerItems, 5000)).localName.value).toBe('Shared')
      expect((await nextValue(joinedItems, 5000)).localName.value).toBe('Shared')
      expect(harness.calls.filter(([name]) => name === 'startScan')).toHaveLength(1)
      await expect(backend.scanner.join(owner.leaseId, 'forged-token', 'client-3')).rejects.toMatchObject({
        normalized: { code: 'ownership.denied' }
      })
      expect(await owner.stop()).toEqual({ state: 'released', failures: [] })
      expect(await nextItem(joinedItems, 2000)).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    })
  })

  test('duplicate policy first delivers one sighting per peer (legacy BlueZ DuplicateData=false)', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions({ duplicatePolicy: 'first' }), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        await stage.stageAdvertisement({ peerId: 'dup', rssi: -50 })
        await stage.stageAdvertisement({ peerId: 'dup', rssi: -51 })
        await stage.stageAdvertisement({ peerId: 'other', rssi: -52 })
        const first = await nextValue(iterator, 5000)
        const second = await nextValue(iterator, 5000)
        expect([first.rssi.value, second.rssi.value]).toEqual([-50, -52])
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })
})

describe('lifecycle and adapter events (parity rows connection.lost-event, database-changed, adapter power)', () => {
  test('an OS link loss emits connection-lost for that connection and ends its streams', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { lease, database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      await stage.stageLinkLoss('peer-1')
      const lost = await nextEvent(events, event => event.kind === 'connection-lost', 5000)
      expect(lost.connection.connectionId).toBe(lease.connection.connectionId)
      expect(lost.connection.ownerLeaseId).toBe(lease.leaseId)
      expect(await nextItem(values, 3000)).toMatchObject({ kind: 'terminal', reason: 'connection-lost' })
      await expect(database.read(measurement.path, { signal: null, deadline: null })).rejects.toMatchObject({
        normalized: { code: 'gatt.stale-handle' }
      })
      expect(await lease.release()).toEqual({ state: 'released', failures: [] })
    })
  })

  test('a GATT database change emits database-changed and ends subscriptions as service-changed', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const events = backend.events()[Symbol.asyncIterator]()
      const { lease, database, measurement } = await connectAndDiscover(backend, stage)
      const subscription = await database.subscribe(measurement.path, subscribeOptions())
      const values = subscription.values[Symbol.asyncIterator]()
      await stage.stageServicesChanged('peer-1')
      const changed = await nextEvent(events, event => event.kind === 'database-changed', 5000)
      expect(changed.database.databaseId).toBe(database.path.databaseId)
      expect(await nextItem(values, 3000)).toMatchObject({ kind: 'terminal', reason: 'service-changed' })
      await lease.release()
    })
  })

  test('adapter power is read from the OS and its changes are watched', async () => {
    const harness = realBinding('bluez')
    const originalOpen = harness.binding.openSynthetic
    harness.binding.openSynthetic = async owner => {
      const central = await originalOpen(owner)
      await harness.opened[harness.opened.length - 1].stageAdapterState('powered-on')
      return central
    }
    const provider = createDesktopRustCoreBackendProvider({
      platform: 'bluez',
      owner: 'adapter-power',
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: 'linux'
    })
    const [adapter] = await provider.listAdapters()
    expect(adapter.state).toMatchObject({ availability: 'available', power: 'on', authorization: 'unknown' })
    const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
    const stage = harness.opened[harness.opened.length - 1]
    try {
      const events = backend.events()[Symbol.asyncIterator]()
      const watch = await backend.adapter.watchState()
      expect(watch.initial.power).toBe('on')
      const transitions = watch.transitions[Symbol.asyncIterator]()
      await stage.stageAdapterState('powered-off', true)
      expect((await nextValue(transitions, 5000)).power).toBe('off')
      await nextEvent(events, event => event.kind === 'adapter-state', 5000)
      expect((await backend.adapter.currentState()).power).toBe('off')
    } finally {
      await backend.destroy()
    }
  })

  test('backend and adapter streams keep their legacy quotas (F10)', async () => {
    await withBackend('corebluetooth', async ({ backend }) => {
      // Legacy CoreBluetooth stream quotas: backend events 64/64KiB/1,
      // adapter transitions 16/16KiB/1.
      const events = backend.events()
      try {
        expect(events.limits).toMatchObject({ itemCapacity: 64, byteCapacity: 64 * 1024, reservedControlCapacity: 1 })
      } finally {
        await events.close()
      }
      const watch = await backend.adapter.watchState()
      try {
        expect(watch.transitions.limits).toMatchObject({
          itemCapacity: 16,
          byteCapacity: 16 * 1024,
          reservedControlCapacity: 1
        })
      } finally {
        await watch.transitions.close()
      }
    })
  })

  test('an unreadable adapter power reports unknown with the reason, never a guess', async () => {
    await withBackend('winrt', async ({ backend }) => {
      const state = await backend.adapter.currentState()
      expect(state.availability).toBe('available')
      expect(state.power).toBe('unknown')
      expect(state.safeReason).toMatch(/not readable/)
    })
  })
})

describe('connected RSSI (parity row connection.rssi)', () => {
  test('CoreBluetooth registers connection:rssi and reads it through the core', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      expect(backend.features.registrations.map(registration => registration.id)).toContain(
        BUILT_IN_FEATURE_IDS.connectionRssi
      )
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageRssi('peer-1', -47)
      const measurement = await backend.connections.readRssi(lease.connection, {
        operation: { signal: null, deadline: null, correlation: 'corr-rssi' }
      }).completion
      expect(measurement.rssi).toBe(-47)
      expect(backend.dispatchCounters().readRssi).toBe(1)
    })
  })

  test.each(['bluez', 'winrt'])('%s neither registers nor exposes RSSI (legacy did not either)', async platform => {
    await withBackend(platform, async ({ backend }) => {
      expect(backend.connections.readRssi).toBeUndefined()
      expect(backend.features.registrations.map(registration => registration.id)).not.toContain(
        BUILT_IN_FEATURE_IDS.connectionRssi
      )
    })
  })

  test('BlueZ keeps the priority/parameters registrations with their reasons', async () => {
    await withBackend('bluez', async ({ backend }) => {
      const byId = new Map(backend.features.registrations.map(registration => [registration.id, registration]))
      expect(byId.get(BUILT_IN_FEATURE_IDS.connectionPriority)).toMatchObject({ state: 'unsupported' })
      expect(byId.get(BUILT_IN_FEATURE_IDS.connectionParameters)).toMatchObject({ state: 'unsupported' })
    })
  })
})

describe('adapter enumeration and selection (parity row adapter.enumerate-select)', () => {
  test('each OS adapter is listed; the selected label reaches the core open', async () => {
    const harness = realBinding('winrt')
    harness.binding.listAdapters = async () => [
      { index: 0, label: 'adapter-a', error: null },
      { index: 1, label: 'adapter-b', error: null },
      { index: 2, label: null, error: 'adapter.unavailable|adapter|adapter.info|never|||busy' }
    ]
    const provider = createDesktopRustCoreBackendProvider({
      platform: 'winrt',
      owner: 'adapters',
      now: () => performance.now(),
      radio: 'production',
      binding: harness.binding,
      hostPlatform: 'win32'
    })
    const adapters = await provider.listAdapters()
    expect(adapters).toHaveLength(3)
    // Legacy WinRT ids are the raw Windows device ids (LEGACY-AUDIT-1 #67).
    expect(adapters.slice(0, 2).map(adapter => String(adapter.adapterId))).toEqual(['adapter-a', 'adapter-b'])
    expect(adapters[2].state).toMatchObject({ availability: 'unavailable' })
    expect(adapters[2].state.safeReason).toMatch(/busy/)
    const backend = await provider.create({ selectedAdapterId: adapters[1].adapterId })
    try {
      expect(harness.productionRequests.at(-1)).toMatchObject({ platform: 'winrt', adapterId: 'adapter-b' })
    } finally {
      await backend.destroy()
    }
    await expect(provider.create({ selectedAdapterId: adapters[2].adapterId })).rejects.toMatchObject({
      normalized: { code: 'adapter.unavailable' }
    })
  })
})

describe('legacy public ids (LEGACY-AUDIT-1 #67)', () => {
  function listingProvider(platform, listings) {
    const harness = realBinding(platform)
    let current = listings[0]
    harness.binding.listAdapters = async () => current
    const provider = createDesktopRustCoreBackendProvider({
      platform,
      owner: `ids-${platform}`,
      now: () => performance.now(),
      radio: 'production',
      binding: harness.binding,
      hostPlatform: { bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' }[platform]
    })
    return { provider, harness, next: index => (current = listings[index]) }
  }

  test.each([
    ['bluez', 'hci1', '/org/bluez/hci1'],
    ['winrt', 'BTHENUM\\{guid}\\radio', 'BTHENUM\\{guid}\\radio'],
    ['corebluetooth', 'default', 'corebluetooth-default-adapter']
  ])('%s adapter %s is listed as %s and selects that label', async (platform, label, legacyId) => {
    const { provider, harness } = listingProvider(platform, [[{ index: 0, label, error: null, default: true }]])
    const [adapter] = await provider.listAdapters()
    expect(String(adapter.adapterId)).toBe(legacyId)
    const backend = await provider.create({ selectedAdapterId: legacyId })
    try {
      expect(String(backend.identity.attachment.adapter.adapterId)).toBe(legacyId)
      expect(harness.productionRequests.at(-1)).toMatchObject({ adapterId: label })
    } finally {
      await backend.destroy()
    }
  })

  test('an adapter id does not change when another adapter is added or removed, or the default moves', async () => {
    const { provider, next } = listingProvider('bluez', [
      [{ index: 0, label: 'hci0', error: null, default: true }],
      [
        { index: 0, label: 'hci0', error: null, default: false },
        { index: 1, label: 'hci1', error: null, default: true }
      ]
    ])
    const before = (await provider.listAdapters()).map(adapter => String(adapter.adapterId))
    next(1)
    const after = (await provider.listAdapters()).map(adapter => String(adapter.adapterId))
    expect(before).toEqual(['/org/bluez/hci0'])
    expect(after).toEqual(['/org/bluez/hci0', '/org/bluez/hci1'])
  })
})

describe('unsupported rows keep their legacy reasons (LEGACY-AUDIT-1 #66)', () => {
  test('CoreBluetooth request-mtu, effective-mtu and phy are unsupported with the legacy codes', async () => {
    await withBackend('corebluetooth', async ({ backend }) => {
      const byId = new Map(backend.features.registrations.map(entry => [entry.id, entry]))
      for (const [id, code] of [
        [BUILT_IN_FEATURE_IDS.connectionRequestMtu, 'corebluetooth-auto-negotiated-mtu'],
        [BUILT_IN_FEATURE_IDS.connectionEffectiveMtu, 'effective-mtu-boundary-unavailable'],
        [BUILT_IN_FEATURE_IDS.connectionPhy, 'corebluetooth-phy-runtime-unavailable']
      ]) {
        expect(byId.get(id)).toMatchObject({ state: 'unsupported', limitations: [expect.objectContaining({ code })] })
      }
    })
  })

  test('BlueZ pairing generation without a host controller is unsupported with the privilege explanation', async () => {
    await withBackend('bluez', async ({ backend }) => {
      const row = backend.features.registrations.find(
        entry => entry.id === BUILT_IN_FEATURE_IDS.securityPairingGeneration
      )
      expect(row).toMatchObject({
        state: 'unsupported',
        limitations: [
          expect.objectContaining({ code: 'bluez-pairing-generation-needs-a-host-supplied-privileged-operation' })
        ]
      })
      expect(row.limitations[0].explanation).toMatch(/CAP_NET_ADMIN/)
    })
  })
})

describe('delivery mode (decision 3, PR210-31)', () => {
  test('require-indication on a notify-only characteristic fails before any dispatch', async () => {
    await withBackend('corebluetooth', async ({ backend, stage, harness }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      const before = dispatchCalls(harness.calls).length
      await expect(
        database.subscribe(measurement.path, subscribeOptions({ deliveryMode: 'require-indication' }))
      ).rejects.toMatchObject({ normalized: { code: 'gatt.property-not-supported' } })
      expect(dispatchCalls(harness.calls)).toHaveLength(before)
    })
  })

  test('require-notification on a notify characteristic subscribes; the value delivery is what the core observed', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const { database, measurement } = await connectAndDiscover(backend, stage)
      await stage.stageObservedDelivery('indication')
      const subscription = await database.subscribe(
        measurement.path,
        subscribeOptions({ deliveryMode: 'require-notification' })
      )
      const values = subscription.values[Symbol.asyncIterator]()
      await stage.stageNotification({
        peerId: 'peer-1',
        serviceUuid: HRM_SERVICE,
        characteristicUuid: HRM_MEASUREMENT,
        value: Buffer.from([9])
      })
      expect((await nextValue(values, 5000)).delivery).toBe('indication')
      await subscription.remove()
    })
  })
})

describe('error and cleanup mapping', () => {
  test('the wire form carries the core retryability and commit verbatim', () => {
    const error = desktopRustCoreError(
      new Error('operation.timed-out|gatt|gatt.write|never|unknown||late'),
      'winrt.gatt.write'
    )
    // The public id is the caller's 4.x operation; the core's stays internal.
    expect(error.normalized).toMatchObject({
      code: 'operation.timed-out',
      domain: 'gatt',
      operation: 'winrt.gatt.write',
      retryability: 'never',
      commit: 'uncertain',
      platform: { metadata: { commit: 'unknown', coreOperation: 'gatt.write' }, safeMessage: 'late' }
    })
    const retry = desktopRustCoreError(
      new Error('operation.aborted|gatt|gatt.read|caller-decides|not-dispatched||'),
      'x'
    )
    expect(retry.normalized.retryability).toBe('caller-decides')
    expect(retry.normalized.commit).toBe('not-dispatched')
    expect(parseDesktopRustCoreWireError('not-a-code|gatt|x|never|||')).toBeNull()
    expect(desktopRustCoreError(new Error('garbage'), 'fallback-op').normalized).toMatchObject({
      code: 'platform.transport',
      operation: 'fallback-op'
    })
  })

  // LEGACY-AUDIT-4 B2: the OS's own answer crosses as the 4.x platform
  // identity per host (legacy winrt-backend-helpers.ts winRtPlatformError,
  // bluez-operation-dispatcher.ts normalizeBluezFailure, the CoreBluetooth
  // NSError detail), not as free text.
  const wire = (code, domain, operation, platform, detail) =>
    new Error(`${code}|${domain}|${operation}|never||${JSON.stringify(platform).replace(/\|/g, '\\u007c')}|${detail}`)

  test.each([
    [
      'CoreBluetooth',
      { domain: 'corebluetooth', code: '14', message: 'Peer removed pairing information', metadata: {} },
      { domain: 'corebluetooth', code: '14', safeMessage: 'Peer removed pairing information', metadata: {} }
    ],
    [
      'WinRT',
      {
        domain: 'winrt',
        code: 'unreachable',
        message: 'The device is unreachable | retry later',
        metadata: { hresult: '0x80650002', gattStatus: 'unreachable' }
      },
      {
        domain: 'winrt',
        code: 'unreachable',
        safeMessage: 'The device is unreachable | retry later',
        metadata: { hresult: '0x80650002', gattStatus: 'unreachable' }
      }
    ],
    [
      'BlueZ',
      { domain: 'bluez-dbus', code: 'org.bluez.Error.NotPermitted', message: 'Read not permitted', metadata: {} },
      { domain: 'bluez-dbus', code: 'org.bluez.Error.NotPermitted', safeMessage: 'Read not permitted', metadata: {} }
    ]
  ])('%s platform detail restores the legacy error identity', (_host, platform, expected) => {
    const error = desktopRustCoreError(wire('gatt.read-failed', 'gatt', 'gatt.read', platform, 'core text'), 'x.gatt.read')
    expect(error.normalized).toMatchObject({ code: 'gatt.read-failed', domain: 'gatt', operation: 'x.gatt.read' })
    expect(error.normalized.platform).toEqual(expected)
  })

  test.each([
    [
      'corebluetooth',
      { domain: 'corebluetooth', code: '15', message: 'Encryption is insufficient.', metadata: {} },
      { domain: 'corebluetooth', code: '15', safeMessage: 'Encryption is insufficient.', metadata: {} }
    ],
    [
      'winrt',
      {
        domain: 'winrt',
        code: 'protocol-error',
        message: 'GATT protocol error',
        metadata: { hresult: '0x80650005', gattStatus: 'protocol-error' }
      },
      {
        domain: 'winrt',
        code: 'protocol-error',
        safeMessage: 'GATT protocol error',
        metadata: { hresult: '0x80650005', gattStatus: 'protocol-error' }
      }
    ],
    [
      'bluez',
      {
        domain: 'bluez-dbus',
        code: 'org.bluez.Error.NotAuthorized',
        message: 'Operation Not Authorized',
        metadata: {}
      },
      {
        domain: 'bluez-dbus',
        code: 'org.bluez.Error.NotAuthorized',
        safeMessage: 'Operation Not Authorized',
        metadata: {}
      }
    ]
  ])(
    '%s: an OS read failure reaches the caller with the legacy platform identity, end to end',
    async (platform, staged, expected) => {
      await withBackend(platform, async ({ backend, stage }) => {
        const { database, measurement, lease } = await connectAndDiscover(backend, stage)
        await stage.failNextRadioOpWithPlatform('read', 'os read failed', staged)
        const failure = await database.read(measurement.path, { signal: null, deadline: null }).then(
          () => null,
          error => error
        )
        expect(failure).not.toBeNull()
        expect(failure.normalized.platform).toEqual(expected)
        await lease.release()
      })
    }
  )

  test('a platform detail without a message keeps the core detail as its message', () => {
    const error = desktopRustCoreError(
      wire(
        'platform.failure',
        'platform',
        'security.pair',
        { domain: 'bluez-dbus', code: 'org.bluez.Error.Failed', message: null, metadata: {} },
        'Pair failed'
      ),
      'fallback'
    )
    expect(error.normalized.platform).toEqual({
      domain: 'bluez-dbus',
      code: 'org.bluez.Error.Failed',
      safeMessage: 'Pair failed',
      metadata: {}
    })
  })

  test('a malformed platform field is a transport fault, never a guessed identity', () => {
    for (const field of [
      '{',
      '[]',
      '{"domain":"winrt","code":7,"message":null,"metadata":{}}',
      '{"domain":"winrt","code":"x","message":null,"metadata":{"a":{}}}'
    ]) {
      expect(parseDesktopRustCoreWireError(`gatt.read-failed|gatt|gatt.read|never||${field}|x`)).toBeNull()
      expect(
        desktopRustCoreError(new Error(`gatt.read-failed|gatt|gatt.read|never||${field}|x`), 'op').normalized.code
      ).toBe('platform.transport')
    }
  })

  test('a failed shutdown release is reported, never flattened to released', () => {
    const record = cleanupRecordFromCloseReport(
      {
        state: 'release-failed',
        failures: [{ resourceKind: 'connection', error: 'platform.failure|cleanup|dispatch.close|never|||stuck' }]
      },
      'dispose'
    )
    expect(record.state).toBe('release-failed')
    expect(record.failures[0]).toMatchObject({ resourceKind: 'connection', error: { code: 'platform.failure' } })
  })

  test('destroy retires later operations loudly', async () => {
    const { backend } = await openBackend('bluez')
    await backend.destroy()
    await expect(backend.scanner.start(scanOptions(), 'client-1')).rejects.toMatchObject({
      normalized: { code: 'lifecycle.destroyed' }
    })
  })
})

describe('parity rows closed by the core OS adapters (PARITY-INVENTORY §1–3)', () => {
  async function openWithStaging(platform, stageBeforeOpen) {
    const harness = realBinding(platform)
    const original = harness.binding.openSynthetic
    harness.binding.openSynthetic = async owner => {
      const central = await original(owner)
      await stageBeforeOpen(harness.opened[harness.opened.length - 1])
      return central
    }
    const provider = createDesktopRustCoreBackendProvider({
      platform,
      owner: `staged-${platform}`,
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: { bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' }[platform]
    })
    const [adapter] = await provider.listAdapters()
    const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
    return { backend, stage: harness.opened[harness.opened.length - 1], harness, adapter }
  }

  test('WinRT carries require-* to the core; CoreBluetooth and BlueZ keep the legacy property check', async () => {
    for (const [platform, expected] of [
      ['winrt', ['notification']],
      ['corebluetooth', [null]],
      ['bluez', [null]]
    ]) {
      await withBackend(platform, async ({ backend, stage }) => {
        const { database, measurement } = await connectAndDiscover(backend, stage)
        const subscription = await database.subscribe(
          measurement.path,
          subscribeOptions({ deliveryMode: 'require-notification' })
        )
        await subscription.remove()
        expect({ platform, requests: await stage.stagedDeliveryRequests() }).toEqual({ platform, requests: expected })
      })
    }
  })

  test('CoreBluetooth maximum write length: registration, connection dispatch, and the long-write feature', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const ids = backend.features.registrations.map(registration => registration.id)
      expect(ids).toContain(BUILT_IN_FEATURE_IDS.maximumWriteLength)
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteLimits('peer-1', 244, 182)
      const measured = await backend.connections.maximumWriteLength(lease.connection, {
        operation: { signal: null, deadline: null, correlation: 'corr-mwl' },
        mode: 'without-response'
      }).completion
      expect(measured).toMatchObject({ mode: 'without-response', maximumWriteLength: 182 })
      const registration = backend.features.registrations.find(
        entry => entry.id === BUILT_IN_FEATURE_IDS.maximumWriteLength
      )
      const observed = await registration.implementation.invoke({
        connectionId: String(lease.connection.connectionId),
        connectionGeneration: String(lease.connection.connectionGeneration),
        mode: 'with-response'
      })
      expect(observed.maximumWriteLength).toBe(244)
      expect(backend.dispatchCounters().maximumWriteLength).toBe(2)
    })
  })

  test('maximum write length is measured on the link before any discovery (LEGACY-AUDIT-1 #65)', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const peerId = await observePeer(backend, stage)
      await stage.stageWriteLimits('peer-1', 244, 182)
      const lease = await backend.connections.connect(peerId, 'c', { signal: null, deadline: null })
      try {
        for (const [mode, expected] of [
          ['with-response', 244],
          ['without-response', 182]
        ]) {
          const measured = await backend.connections.maximumWriteLength(lease.connection, {
            operation: { signal: null, deadline: null, correlation: `corr-mwl-${mode}` },
            mode
          }).completion
          expect(measured).toMatchObject({ mode, maximumWriteLength: expected })
        }
      } finally {
        await lease.release()
      }
    })
  })

  test('an unmeasured write limit is capability.unavailable, never a guess', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const peerId = await observePeer(backend, stage)
      const lease = await backend.connections.connect(peerId, 'c', { signal: null, deadline: null })
      try {
        await expect(
          backend.connections.maximumWriteLength(lease.connection, {
            operation: { signal: null, deadline: null, correlation: 'corr-mwl-unmeasured' },
            mode: 'with-response'
          }).completion
        ).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
      } finally {
        await lease.release()
      }
    })
  })

  test.each([
    [
      'attribute-instances',
      'repeated service / characteristic / descriptor UUIDs keep their instances (LEGACY-AUDIT-1 #61)'
    ],
    ['winrt-uncached-discovery', 'WinRT discovery is uncached and reports failures (LEGACY-AUDIT-1 #62)'],
    ['scan-policy', 'BlueZ LE transport and the duplicate policy reach the OS scan (LEGACY-AUDIT-1 #63)'],
    ['central-state-detail', 'CoreBluetooth resetting / unsupported / unauthorized states (LEGACY-AUDIT-1 #60)'],
    ['winrt-adapter-by-id', 'WinRT opens a non-default adapter by id (LEGACY-AUDIT-1 #68)']
  ])('the loaded core links the vendored btleplug patch %s: %s', patch => {
    const { binding } = realBinding('bluez')
    expect(binding.diagnostics.btleplugPatches.split(',')).toContain(patch)
  })

  test('WinRT: any listed adapter is selectable and the deployment is reported (LEGACY-AUDIT-1 #68)', async () => {
    const harness = realBinding('winrt')
    harness.binding.listAdapters = async () => [
      { index: 0, label: 'adapter-default', error: null, default: true, deployment: 'unpackaged' },
      { index: 1, label: 'adapter-other', error: null, default: false, deployment: 'unpackaged' }
    ]
    const provider = createDesktopRustCoreBackendProvider({
      platform: 'winrt',
      owner: 'winrt-select',
      now: () => performance.now(),
      radio: 'production',
      binding: harness.binding,
      hostPlatform: 'win32'
    })
    const adapters = await provider.listAdapters()
    expect(adapters.map(adapter => adapter.state.availability)).toEqual(['available', 'available'])
    expect(adapters[1].limitations).toContain('Selected through unpackaged Windows application deployment semantics')
    const backend = await provider.create({ selectedAdapterId: 'adapter-other' })
    try {
      expect(harness.productionRequests.at(-1)).toMatchObject({ adapterId: 'adapter-other' })
      expect(backend.identity.runtime.diagnostics.deployment).toBe('unpackaged')
    } finally {
      await backend.destroy()
    }
  })

  test('a malformed deployment in the listing is refused', async () => {
    const { bindDesktopCore } = require('../../../src/desktop-core-addon')
    const { addonPath, loadAddon } = require('../../helpers/desktop-rust-core-harness')
    const addon = loadAddon()
    const forged = {
      ...addon,
      UbmCentral: {
        listAdapters: async () => [{ index: 0, label: 'a', default: true, deployment: 'sideloaded' }],
        open: (...args) => addon.UbmCentral.open(...args),
        openSynthetic: (...args) => addon.UbmCentral.openSynthetic(...args),
        capabilityStates: (...args) => addon.UbmCentral.capabilityStates(...args),
        vendoredBtleplugPatches: () => addon.UbmCentral.vendoredBtleplugPatches()
      },
      nativeBuildIdentity: addon.nativeBuildIdentity
    }
    const bound = bindDesktopCore(
      { platform: 'winrt', operationPrefix: 'winrt' },
      { module: forged, path: addonPath, mode: 'source', sidecar: null }
    )
    await expect(bound.listAdapters()).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'winrt.provider.list-adapters.deployment' }
    })
  })

  test('adapter authorization is the OS answer (macOS, Windows); unknown elsewhere', async () => {
    const { backend } = await openWithStaging('corebluetooth', central => central.stageAdapterAuthorization('denied'))
    try {
      expect((await backend.adapter.currentState()).authorization).toBe('denied')
    } finally {
      await backend.destroy()
    }
    await withBackend('bluez', async ({ backend: linux }) => {
      expect((await linux.adapter.currentState()).authorization).toBe('unknown')
    })
  })

  test('advertisement solicited UUIDs and connectable cross when the radio reports them', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        await stage.stageAdvertisement({
          peerId: 'peer-x',
          localName: 'X',
          connectable: true,
          solicitedServiceUuids: [HRM_SERVICE]
        })
        const observation = await nextValue(iterator, 5000)
        expect(observation.connectable).toMatchObject({ state: 'present', value: true })
        expect(observation.solicitedServiceUuids).toMatchObject({ state: 'present', value: [HRM_SERVICE] })
        expect(observation.overflowServiceUuids.state).toBe('unavailable')
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test.each(['winrt', 'bluez'])('%s security: state, pair, watch, cancel, unpair through the core', async platform => {
    await withBackend(platform, async ({ backend, stage }) => {
      expect(backend.security).toBeDefined()
      const peerId = String(await observePeer(backend, stage))
      await stage.stageSecurity('peer-1', 'not-bonded', true)
      expect(await backend.security.state(peerId, { signal: null, deadline: null })).toMatchObject({
        bond: 'not-bonded',
        pairingPossible: true,
        encryption: 'unsupported'
      })
      const watch = backend.security.watch(peerId)[Symbol.asyncIterator]()
      // The watch opens with the current state, as the 4.x watches did.
      expect(await nextValue(watch, 5000)).toMatchObject({ kind: 'state', peerId, state: { bond: 'not-bonded' } })
      await stage.stagePairOutcome('peer-1', 'paired')
      const paired = await backend.security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'le',
        protection: 'system-default',
        ceremony: 'system'
      })
      expect(paired).toMatchObject({ outcome: 'paired', state: { bond: 'bonded' } })
      expect(await nextValue(watch, 5000)).toMatchObject({ kind: 'state', peerId, state: { bond: 'bonded' } })
      expect(await backend.security.cancelPairing(peerId, { signal: null, deadline: null })).toEqual({
        outcome: 'not-pairing'
      })
      expect((await backend.security.unpair(peerId, { signal: null, deadline: null })).outcome).toMatch(
        /^(unpaired|already-unpaired)$/
      )
      await expect(
        backend.security.pair(peerId, {
          signal: null,
          deadline: null,
          transport: 'le',
          protection: 'system-default',
          ceremony: { kind: 'agent', agent: { onChallenge: async () => ({ kind: 'confirm', confirmed: true }) } }
        })
      ).rejects.toMatchObject({
        normalized: {
          code: 'capability.unsupported',
          // B-R2: BlueZ keeps the legacy `.pair` segment; WinRT never had it.
          operation: platform === 'bluez' ? 'bluez.security.pair.custom-ceremony' : 'winrt.security.custom-ceremony'
        }
      })
      await expect(
        backend.security.pair(peerId, {
          signal: null,
          deadline: null,
          transport: 'le',
          protection: 'system-default',
          ceremony: 'system',
          secureConnections: 'require'
        })
      ).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
    })
  })

  test('BlueZ pairing generation: the host controller holds the generation for a directed pair and it is restored', async () => {
    const calls = []
    const controller = {
      read: async adapterId => {
        calls.push(['read', adapterId])
        return 'enabled'
      },
      set: async (adapterId, generation) => {
        calls.push(['set', adapterId, generation])
      }
    }
    const harness = realBinding('bluez')
    const provider = createDesktopRustCoreBackendProvider({
      platform: 'bluez',
      owner: 'pairing-generation',
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: 'linux',
      pairingGeneration: controller
    })
    const [adapter] = await provider.listAdapters()
    const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
    const stage = harness.opened[harness.opened.length - 1]
    try {
      const generationRow = backend.features.registrations.find(
        entry => entry.id === BUILT_IN_FEATURE_IDS.securityPairingGeneration
      )
      // The branch that mutates adapter-wide state reports its blast radius.
      expect(generationRow).toMatchObject({
        state: 'limited',
        limitations: [expect.objectContaining({ code: 'bluez-pairing-generation-is-adapter-wide' })]
      })
      const peerId = String(await observePeer(backend, stage))
      await stage.stagePairOutcome('peer-1', 'paired')
      const result = await backend.security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'le',
        protection: 'system-default',
        ceremony: 'system',
        secureConnections: 'disallow'
      })
      expect(result.outcome).toBe('paired')
      expect(calls.map(call => call.slice(0, 1).concat(call.slice(2)))).toEqual([
        ['read'],
        ['set', 'legacy-only'],
        ['set', 'enabled']
      ])
    } finally {
      await backend.destroy()
    }
  })

  test('CoreBluetooth write-without-response readiness: the core probe, then OS reports for this connection', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      expect(backend.features.registrations.map(entry => entry.id)).toContain(
        BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness
      )
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const watch = await backend.connections.writeWithoutResponseReadiness(lease.connection)
      const events = watch.events[Symbol.asyncIterator]()
      expect(await nextValue(events, 3000)).toMatchObject({ ready: false, ordinal: 1 })
      await stage.stageWriteReadiness('peer-1', true, true)
      expect(await nextValue(events, 5000)).toMatchObject({ ready: true, ordinal: 2 })
      expect(await watch.close()).toEqual({ state: 'released', failures: [] })
    })
  })

  test.each(['winrt', 'bluez'])('%s registers no readiness watch (the OS has no readiness signal)', async platform => {
    await withBackend(platform, async ({ backend }) => {
      expect(backend.connections.writeWithoutResponseReadiness).toBeUndefined()
    })
  })

  test('maximum-write-length rejects empty connection ids as invalid (F9), as legacy did', async () => {
    await withBackend('corebluetooth', async ({ backend }) => {
      const row = backend.features.registrations.find(entry => entry.id === 'gatt:maximum-write-length')
      expect(row?.implementation).toBeDefined()
      // An empty id is a malformed argument, not a missing connection.
      await expect(
        row.implementation.invoke({ connectionId: '', connectionGeneration: '1', mode: 'with-response' })
      ).rejects.toMatchObject({
        normalized: { code: 'argument.invalid', domain: 'gatt', operation: 'direct-gatt.gatt.maximum-write-length' }
      })
      await expect(
        row.implementation.invoke({ connectionId: 'x', connectionGeneration: '', mode: 'with-response' })
      ).rejects.toMatchObject({ normalized: { code: 'argument.invalid', domain: 'gatt' } })
    })
  })

  test('CoreBluetooth RSSI keeps its legacy integer-precision limits (F7)', async () => {
    await withBackend('corebluetooth', async ({ backend }) => {
      const rssi = backend.features.registrations.find(entry => entry.id === 'connection:rssi')
      expect(rssi).toBeDefined()
      // The legacy registry reported integer dBm precision, not generic availability.
      expect(rssi.limits).toMatchObject({
        minimumRssiIntegerPrecision: { minimum: 1, maximum: 1, unit: 'dBm' }
      })
    })
  })

  test('readiness overflow keeps drop-oldest (F1), as the legacy watch did', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const watch = await backend.connections.writeWithoutResponseReadiness(lease.connection)
      try {
        // The legacy watch dropped the oldest observation on overflow,
        // never the newest.
        expect(watch.events.overflowPolicy).toBe('drop-oldest')
      } finally {
        await watch.close()
      }
    })
  })

  test('readiness reprobes every 100 ms while unready (F2 safety net)', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const watch = await backend.connections.writeWithoutResponseReadiness(lease.connection)
      const events = watch.events[Symbol.asyncIterator]()
      try {
        // The probe answers first; with no OS edge the 100 ms reprobe
        // re-reads and re-emits while the link stays unready.
        expect(await nextValue(events, 3000)).toMatchObject({ ready: false, ordinal: 1 })
        expect(await nextValue(events, 3000)).toMatchObject({ ready: false, ordinal: 2 })
        expect(await nextValue(events, 3000)).toMatchObject({ ready: false, ordinal: 3 })
      } finally {
        await events.return?.()
        await watch.close()
      }
    })
  })

  test('a readiness report arriving during the probe is replayed, never dropped (F3)', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const { lease } = await connectAndDiscover(backend, stage)
      await stage.stageWriteReadiness('peer-1', false)
      const pending = backend.connections.writeWithoutResponseReadiness(lease.connection)
      // Announced while the probe is still in flight: the watch buffers it
      // and replays it after the probe, as the legacy watch did.
      await stage.stageWriteReadiness('peer-1', true, true)
      const watch = await pending
      const events = watch.events[Symbol.asyncIterator]()
      try {
        // The during-probe report (true) arrives even though the probe was
        // still in flight when the OS announced it: nothing the watch was
        // live for is lost. (The probe value itself is covered by the
        // probe-then-reports test above.)
        const seen = []
        for (let index = 0; index < 10; index += 1) {
          const value = await nextValue(events, 3000)
          seen.push(value.ready)
          if (value.ready === true) break
        }
        expect(seen).toContain(true)
      } finally {
        await events.return?.()
        await watch.close()
      }
    })
  })

  test('WinRT scan terminated by the OS ends every consumer, and a new scan starts', async () => {
    await withBackend('winrt', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      await stage.stageScanTerminated(true, 'radio turned off')
      expect(await nextItem(iterator, 5000)).toMatchObject({
        kind: 'terminal',
        reason: 'source-failed',
        error: { platform: { safeMessage: 'radio turned off' } }
      })
      const next = await backend.scanner.start(scanOptions(), 'client-1')
      await next.stop()
    })
  })

  test('CoreBluetooth exposes no security backend (legacy had none either)', async () => {
    await withBackend('corebluetooth', async ({ backend }) => {
      expect(backend.security).toBeUndefined()
    })
  })

  test('WinRT registers maintain-connection (the Windows adapter holds it per connection)', async () => {
    await withBackend('winrt', async ({ backend }) => {
      expect(backend.features.registrations.map(entry => entry.id)).toContain(
        BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection
      )
    })
  })

  test('BlueZ address targeting: an out-of-band address connects without a scan', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      expect(backend.features.registrations.map(entry => entry.id)).toContain(BUILT_IN_FEATURE_IDS.peerAddressTargeting)
      await stage.stageAddress('AA:BB:CC:DD:EE:07', 'public', 'peer-7')
      await stage.stageAdvertisement({ peerId: 'peer-7', address: 'AA:BB:CC:DD:EE:07' })
      const peerId = backend.connections.peerFromAddress({ address: 'AA:BB:CC:DD:EE:07', addressType: 'public' })
      const lease = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null })
      expect(lease.connection.peerId).toBe(peerId)
      expect(backend.dispatchCounters().resolveAddress).toBe(1)
      await lease.release()
    })
  })

  test('BlueZ advertisement address type is the OS answer', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      await stage.stageAddress('AA:BB:CC:DD:EE:08', 'random', 'peer-8')
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        await stage.stageAdvertisement({ peerId: 'peer-8', address: 'AA:BB:CC:DD:EE:08' })
        const observation = await nextValue(iterator, 5000)
        expect(observation.device.address).toEqual({ value: 'AA:BB:CC:DD:EE:08', type: 'random' })
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test('BlueZ address without a reported type is random, as legacy mapped it', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        // No staged address type: BlueZ did not report one, and legacy
        // mapped every non-public (including unknown) type to random.
        await stage.stageAdvertisement({ peerId: 'peer-9', address: 'AA:BB:CC:DD:EE:09' })
        const observation = await nextValue(iterator, 5000)
        expect(observation.device.address).toEqual({ value: 'AA:BB:CC:DD:EE:09', type: 'random' })
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test.each([
    ['corebluetooth', 'unavailable'],
    ['winrt', 'absent'],
    ['bluez', 'absent']
  ])('%s reports unprovided fields %s with a backend-scoped device (F8)', async (platform, missing) => {
    await withBackend(platform, async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        // As each legacy backend mapped it: CoreBluetooth `unavailable`,
        // WinRT and BlueZ `absent`; every device identity backend-scoped.
        await stage.stageAdvertisement({ peerId: 'peer-f8', rssi: -59, localName: 'F8' })
        const observation = await nextValue(iterator, 5000)
        expect(observation.device.scope).toBe('backend')
        expect(observation.localName).toMatchObject({ state: 'present', value: 'F8' })
        expect(observation.sourceTimestamp.state).toBe(missing)
        expect(observation.appearance.state).toBe(missing)
        expect(observation.txPower.state).toBe(missing)
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test('BlueZ advertisement without UUIDs leaves serviceUuids absent, as legacy did', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const iterator = lease.observations[Symbol.asyncIterator]()
      try {
        // BlueZ cannot distinguish "none" from "not reported": no UUIDs
        // property is absent, never present([]).
        await stage.stageAdvertisement({ peerId: 'peer-10', rssi: -61, localName: 'NoUuids' })
        const observation = await nextValue(iterator, 5000)
        expect(observation.serviceUuids.state).toBe('absent')
      } finally {
        await iterator.return?.()
        await lease.stop()
      }
    })
  })

  test('characteristic flags and access requirements come from the radio; unreported stays unknown', async () => {
    await withBackend('bluez', async ({ backend, stage }) => {
      const peerId = await observePeer(backend, stage)
      await stage.stageMtu('peer-1', 185)
      await stage.stageServices('peer-1', hrmServices())
      await stage.stageCharacteristicAccess(
        { peerId: 'peer-1', serviceUuid: HRM_SERVICE, characteristicUuid: HRM_MEASUREMENT },
        { broadcast: true, authenticatedSignedWrites: false, reliableWrite: true, encryptRead: true }
      )
      const lease = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null })
      const snapshot = await (
        await backend.gatt.discover(lease.connection, { signal: null, deadline: null })
      ).snapshot()
      const measurement = snapshot.characteristics.find(entry => entry.path.characteristicUuid === HRM_MEASUREMENT)
      expect(measurement.properties).toMatchObject({
        broadcast: true,
        reliableWrite: true,
        authenticatedSignedWrites: false
      })
      expect(measurement.properties.availability).toMatchObject({
        broadcast: 'known',
        authenticatedSignedWrites: 'known',
        extendedProperties: 'unknown'
      })
      expect(measurement.access).toEqual({ read: 'encrypted', write: 'none' })
      const control = snapshot.characteristics.find(entry => entry.path.characteristicUuid !== HRM_MEASUREMENT)
      expect(control.properties.availability.broadcast).toBe('unknown')
      expect(control.access).toEqual({ read: 'unknown', write: 'unknown' })
      await lease.release()
    })
  })
})

describe('scan name prefix reaches the OS filter (LEGACY-AUDIT-2 N10)', () => {
  test.each(PLATFORMS)(
    '%s: a caller name prefix is handed to the radio and the software match still filters',
    async platform => {
      await withBackend(platform, async ({ backend, stage }) => {
        const lease = await backend.scanner.start(
          scanOptions({ filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: 'Polar' } }),
          'client-1'
        )
        const iterator = lease.observations[Symbol.asyncIterator]()
        try {
          // Pattern also matches an address prefix, so the OS narrows only:
          // a name outside the prefix never reaches the caller.
          await stage.stageAdvertisement({ peerId: 'peer-other', rssi: -40, localName: 'Other' })
          await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Polar H10' })
          const observed = await nextValue(iterator, 5000)
          expect(observed.localName).toMatchObject({ value: 'Polar H10' })
        } finally {
          await iterator.return?.()
          await lease.stop()
        }
        const unfiltered = await backend.scanner.start(scanOptions(), 'client-1')
        await unfiltered.stop()
        expect(await stage.stagedScanNamePrefixes()).toEqual(['Polar', null])
      })
    }
  )
})

// LEGACY-AUDIT-5 S5: public errors carry each host's 4.x operation id
// (legacy corebluetooth-*, winrt-*, bluez-* backends), never the core's own.
describe('public errors report the 4.x operation id of each host', () => {
  const PREFIX = { corebluetooth: 'direct-gatt', winrt: 'winrt', bluez: 'bluez' }
  const handle = platform =>
    platform === 'bluez' ? name => `bluez.gatt.${name}` : name => `${PREFIX[platform]}.gatt.database-${name}`
  const failure = promise =>
    promise.then(
      () => null,
      error => error.normalized
    )

  test.each(PLATFORMS)('%s: verbs failing in the OS', async platform => {
    await withBackend(platform, async ({ backend, stage }) => {
      const { database, measurement, lease } = await connectAndDiscover(backend, stage)
      await stage.failNextRadioOp('read', 'os')
      expect((await failure(database.read(measurement.path, { signal: null, deadline: null }))).operation).toBe(
        handle(platform)('read')
      )
      await stage.failNextRadioOp('read', 'os')
      const direct = backend.gatt.read(measurement.path, {
        operation: { signal: null, deadline: null, correlation: 'legacy-read' }
      })
      expect((await failure(direct.completion)).operation).toBe(`${PREFIX[platform]}.gatt.read`)
      await stage.failNextRadioOp('write', 'os')
      expect(
        (
          await failure(
            database.write(measurement.path, new Uint8Array([1]), {
              signal: null,
              deadline: null,
              mode: 'with-response'
            })
          )
        ).operation
      ).toBe(handle(platform)('write'))
      await stage.failNextRadioOp('subscribe', 'os')
      expect((await failure(database.subscribe(measurement.path, subscribeOptions()))).operation).toBe(
        platform === 'bluez' ? 'bluez.gatt.subscribe' : `${PREFIX[platform]}.gatt.database-subscribe`
      )
      await lease.release()
      const peerId = await observePeer(backend, stage)
      await stage.failNextRadioOp('connect', 'os')
      expect((await failure(backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null }))).operation).toBe(
        `${PREFIX[platform]}.connect`
      )
      const again = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null })
      await stage.failNextRadioOp('discover', 'os')
      expect((await failure(backend.gatt.discover(again.connection, { signal: null, deadline: null }))).operation).toBe(
        `${PREFIX[platform]}.gatt.discover`
      )
      await again.release()
      await stage.failNextRadioOp('start-scan', 'os')
      expect((await failure(backend.scanner.start(scanOptions(), 'client-1'))).operation).toBe(
        `${PREFIX[platform]}.scan.start`
      )
    })
  })

  test.each(PLATFORMS)('%s: connect on a never-observed peer is connection.not-found (W-R3)', async platform => {
    await withBackend(platform, async ({ backend }) => {
      // No scan, no staging: this peer was never observed.
      const failure = await backend.connections
        .connect('peer-never-observed', 'client-1', { signal: null, deadline: null })
        .then(
          () => null,
          error => error.normalized
        )
      expect(failure).toMatchObject({
        code: 'connection.not-found',
        domain: 'connection',
        // BlueZ reports the connect op; CoreBluetooth and WinRT the `.peer` segment.
        operation: platform === 'bluez' ? 'bluez.connect' : `${PREFIX[platform]}.connect.peer`
      })
    })
  })

  test('CoreBluetooth when-available intent keeps its legacy operation id (F5)', async () => {
    await withBackend('corebluetooth', async ({ backend, stage }) => {
      const peerId = await observePeer(backend, stage)
      await expect(
        backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null, intent: 'when-available' })
      ).rejects.toMatchObject({
        normalized: { code: 'capability.unsupported', operation: 'direct-gatt.connect.when-available' }
      })
    })
  })

  test.each(PLATFORMS)('%s: provider and loader ids', async platform => {
    const harness = realBinding(platform)
    const provider = createDesktopRustCoreBackendProvider({
      platform,
      owner: 'legacy-ids',
      now: () => 1,
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: { bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' }[platform]
    })
    expect((await failure(provider.create({ selectedAdapterId: 'no-such-adapter' }))).operation).toBe(
      `${PREFIX[platform]}.provider.select-adapter`
    )
    let guard = null
    try {
      createDesktopRustCoreBackendProvider({
        platform,
        owner: 'x',
        now: () => 1,
        loadBinding: jest.fn(),
        hostPlatform: 'sunos'
      })
    } catch (error) {
      guard = error.normalized
    }
    expect(guard).toMatchObject({
      code: 'capability.unavailable',
      operation: `${PREFIX[platform]}.native-boundary.load`,
      platform: { code: DESKTOP_RUST_CORE_PROFILES[platform].requiredPlatformCode }
    })
  })
})

describe('parity table integrity', () => {
  test('every row names its platforms and either how it works or what blocks it', () => {
    for (const row of DESKTOP_RUST_CORE_PARITY) {
      expect(row.platforms.length).toBeGreaterThan(0)
      if (row.rust.state === 'implemented') expect(row.rust.how.length).toBeGreaterThan(0)
      else expect(row.rust.missing.length).toBeGreaterThan(0)
    }
    expect(new Set(DESKTOP_RUST_CORE_PARITY.map(row => row.id)).size).toBe(DESKTOP_RUST_CORE_PARITY.length)
  })
})

// The production-radio leg (UbmCentral.open on this OS's real adapter) is not
// a jest test: under a process without Bluetooth permission macOS aborts it
// (TCC SIGABRT). It runs from scripts/ci/napi-clean-tarball-acceptance.js
// --probe radio in a Bluetooth-authorized terminal (docs/NODE.md).
