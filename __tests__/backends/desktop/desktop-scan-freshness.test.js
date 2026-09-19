'use strict'

// LEGACY-AUDIT-5 S2 / finding 121: 4.x reported advertisements only while a
// scan was active. A sighting the radio made while no scan ran, or during an
// earlier scan, is never delivered to a later scan, and every observation is
// stamped with when the core received it, not when the host took it.

const {
  HOST_PLATFORM,
  nextEvent,
  nextItem,
  nextValue,
  openBackend,
  realBinding,
  scanOptions
} = require('../../helpers/desktop-rust-core-harness')
const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

describe('only this scan’s sightings reach this scan', () => {
  test.each(PLATFORMS)('%s: sightings between scans and from the previous scan never reach the next one', async platform => {
    const { backend, stage } = await openBackend(platform)
    try {
      const first = await backend.scanner.start(scanOptions(), 'client-1')
      const firstValues = first.observations[Symbol.asyncIterator]()
      await stage.stageAdvertisement({ peerId: 'peer-a', rssi: -40, localName: 'During first' })
      expect((await nextValue(firstValues, 5000)).localName).toMatchObject({ value: 'During first' })
      // Queued for the first scan but never taken before it stops.
      await stage.stageAdvertisement({ peerId: 'peer-b', rssi: -41, localName: 'Tail of first' })
      await first.stop()
      await stage.stageAdvertisement({ peerId: 'peer-c', rssi: -42, localName: 'Between scans' })
      await new Promise(resolve => setTimeout(resolve, 30))
      const second = await backend.scanner.start(scanOptions(), 'client-1')
      const secondValues = second.observations[Symbol.asyncIterator]()
      await stage.stageAdvertisement({ peerId: 'peer-d', rssi: -43, localName: 'During second' })
      expect((await nextValue(secondValues, 5000)).localName).toMatchObject({ value: 'During second' })
      await second.stop()
    } finally {
      await backend.destroy()
    }
  })
})

/**
 * A backend whose central hands the provider `advertisement` from
 * `takeScanObservation` once `control.inject` is set (a double of the addon's
 * queue, to reach the provider's own attribution, timestamp and label rules).
 */
async function openInjectingBackend(platform, control) {
  const harness = realBinding(platform)
  const original = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (owner, options) => {
    const central = await original(owner, options)
    return new Proxy(central, {
      get(target, property) {
        const value = Reflect.get(target, property)
        if (typeof value !== 'function') return value
        if (property === 'takeScanObservation') {
          return async () => {
            if (control.inject.length > 0) return control.inject.shift()(control)
            return Reflect.apply(value, target, [])
          }
        }
        if (property === 'startScan') {
          return async (...args) => {
            const started = await Reflect.apply(value, target, args)
            control.scanIds.push(started.operationId)
            return started
          }
        }
        return (...args) => Reflect.apply(value, target, args)
      }
    })
  }
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: `freshness-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: HOST_PLATFORM[platform]
  })
  const [adapter] = await provider.listAdapters()
  return provider.create({ selectedAdapterId: adapter.adapterId })
}

const ad =
  ({ scanOperationId, ageMs = 0, ...advertisement } = {}) =>
  control => ({
    advertisement: {
      peerId: 'peer-x',
      rssi: -50,
      localName: 'Injected',
      serviceUuids: [],
      manufacturerData: [],
      serviceData: [],
      source: 'advertisement',
      ...advertisement
    },
    scanOperationId: scanOperationId ?? control.scanIds[control.scanIds.length - 1],
    ageMs
  })

describe('the provider attributes and stamps from the core’s own facts', () => {
  test.each(PLATFORMS)('%s: an observation of another scan is refused, counted, never delivered', async platform => {
    const control = { inject: [], scanIds: [] }
    const backend = await openInjectingBackend(platform, control)
    try {
      const events = backend.events()[Symbol.asyncIterator]()
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const values = lease.observations[Symbol.asyncIterator]()
      control.inject.push(ad({ localName: 'Stale', scanOperationId: 'scan-that-ended' }), ad({ localName: 'Fresh' }))
      expect((await nextValue(values, 5000)).localName).toMatchObject({ value: 'Fresh' })
      const warning = await nextEvent(
        events,
        event => event.kind === 'diagnostic-warning' && event.code === 'scan-observation-foreign',
        5000
      )
      expect(warning).toBeDefined()
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })

  test.each(PLATFORMS)('%s: an observation is stamped when the core received it', async platform => {
    const control = { inject: [], scanIds: [] }
    const backend = await openInjectingBackend(platform, control)
    try {
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const values = lease.observations[Symbol.asyncIterator]()
      const before = performance.now()
      control.inject.push(ad({ ageMs: 400 }))
      const observation = await nextValue(values, 5000)
      const after = performance.now()
      expect(Number(observation.receivedAtMonotonicMs)).toBeLessThanOrEqual(after - 400 + 1)
      expect(Number(observation.receivedAtMonotonicMs)).toBeGreaterThanOrEqual(before - 400 - 1)
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })
})

// LEGACY-AUDIT-5 S2 / finding 122: an observation is labelled with what it
// is. The OS's merged device state (BlueZ Device1, a known-device report) is
// derived data; one advertisement's own data is raw only where the 4.x
// backend reported it raw (WinRT; CoreBluetooth's parsed dictionary was
// derived).
describe('observation provenance', () => {
  const EXPECTED = {
    winrt: { advertisement: 'platform-raw', 'device-state': 'platform-derived' },
    corebluetooth: { advertisement: 'platform-derived', 'device-state': 'platform-derived' },
    bluez: { advertisement: 'platform-derived', 'device-state': 'platform-derived' }
  }
  test.each(PLATFORMS.flatMap(platform => ['advertisement', 'device-state'].map(source => [platform, source])))(
    '%s, %s',
    async (platform, source) => {
      const control = { inject: [], scanIds: [] }
      const backend = await openInjectingBackend(platform, control)
      try {
        const lease = await backend.scanner.start(scanOptions(), 'client-1')
        const values = lease.observations[Symbol.asyncIterator]()
        control.inject.push(ad({ source }))
        expect((await nextValue(values, 5000)).provenance).toBe(EXPECTED[platform][source])
        await lease.stop()
      } finally {
        await backend.destroy()
      }
    }
  )

  test('an unknown source label is malformed, never guessed', async () => {
    const control = { inject: [], scanIds: [] }
    const backend = await openInjectingBackend('winrt', control)
    try {
      const events = backend.events()[Symbol.asyncIterator]()
      const lease = await backend.scanner.start(scanOptions(), 'client-1')
      const values = lease.observations[Symbol.asyncIterator]()
      control.inject.push(ad({ source: 'rumour', localName: 'Bad' }), ad({ localName: 'Good' }))
      // The malformed record is counted on the stream, then the next one delivers.
      expect(await nextItem(values, 5000)).toMatchObject({ kind: 'overflow', droppedItems: 1 })
      expect((await nextValue(values, 5000)).localName).toMatchObject({ value: 'Good' })
      await nextEvent(events, event => event.kind === 'diagnostic-warning' && event.code === 'scan-record-malformed', 5000)
      await lease.stop()
    } finally {
      await backend.destroy()
    }
  })
})

test('OS-adapter failure counts, including unreadable sightings, are surfaced by the addon', () => {
  const { loadAddon } = require('../../helpers/desktop-rust-core-harness')
  const counts = loadAddon().UbmCentral.osAdapterFailures()
  for (const field of ['linkStateRelease', 'eventDrops', 'watchFailures', 'advertisementReadFailures']) {
    expect(Number.isSafeInteger(counts[field]) && counts[field] >= 0).toBe(true)
  }
})
