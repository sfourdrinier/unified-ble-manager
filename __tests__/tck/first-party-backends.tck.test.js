// __tests__/tck/first-party-backends.tck.test.js

const {
  createFirstPartyBackendTckRegistry,
  createWebBluetoothFirstPartyTckRegistration,
  createCoreBluetoothFirstPartyTckRegistration,
  createBluezFirstPartyTckRegistration,
  createWinRtFirstPartyTckRegistration,
  createReactNativeAndroidFirstPartyTckRegistration,
  createReactNativeAppleFirstPartyTckRegistration
} = require('../../src/testing')
const {
  DeterministicRustCoreNative,
  DEFAULT_PEER: RUST_CORE_PEER_ID
} = require('../../test-support/react-native/deterministic-rust-core-native')
const { deterministicRustCoreTckBoundary } = require('../../test-support/react-native/rust-core-harness')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')
const {
  InMemoryWebBluetoothTckBoundary,
  WEB_BLUETOOTH_TCK_BATTERY_SERVICE_UUID: BATTERY_SERVICE_UUID
} = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')

const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'

jest.setTimeout(60000)

describe('first-party deterministic backend TCK registry', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
  })

  afterEach(() => {
    if (previousRuntime === undefined) {
      delete global.__unifiedBleNativeProtocolV2
      return
    }
    global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test('registers and executes every first-party deterministic backend while retaining explicit exclusions', async () => {
    const androidNative = new DeterministicRustCoreNative({ platform: 'android' })
    const appleNative = new DeterministicRustCoreNative({ platform: 'apple' })
    const webBoundaries = []
    let androidOwner = 0
    let appleOwner = 0
    const registry = createFirstPartyBackendTckRegistry([
      createWebBluetoothFirstPartyTckRegistration({
        createBoundary: () => {
          const boundary = createWebTckBoundary()
          webBoundaries.push(boundary)
          return boundary
        },
        chooserRequest: webChooserRequest()
      }),
      createCoreBluetoothFirstPartyTckRegistration({
        now: () => performance.now(),
        binding: desktopCoreBinding('corebluetooth')
      }),
      createBluezFirstPartyTckRegistration({ now: () => performance.now(), binding: desktopCoreBinding('bluez') }),
      createWinRtFirstPartyTckRegistration({ now: () => performance.now(), binding: desktopCoreBinding('winrt') }),
      createReactNativeAndroidFirstPartyTckRegistration({
        native: androidNative,
        now: () => 20,
        nativePeerId: RUST_CORE_PEER_ID,
        boundary: {
          ...deterministicRustCoreTckBoundary(androidNative),
          seedRestorationJournal: () =>
            androidNative.seedRestored([{ peerId: 'C0:FF:EE:00:00:03', connected: true }])
        },
        security: {
          customCeremonySupported: false,
          supportsAlreadyUnpaired: false,
          supportsCancellation: true,
          supportsUnpair: false
        },
        createOwnerId: () => {
          androidOwner += 1
          return `first-party-registry-android-${androidOwner}`
        }
      }),
      createReactNativeAppleFirstPartyTckRegistration({
        native: appleNative,
        now: () => 20,
        nativePeerId: RUST_CORE_PEER_ID,
        boundary: {
          ...deterministicRustCoreTckBoundary(appleNative),
          seedRestorationJournal: () =>
            appleNative.seedRestored([{ peerId: 'C0FFEE00-0000-4000-8000-000000000001', connected: true }])
        },
        createOwnerId: () => {
          appleOwner += 1
          return `first-party-registry-apple-${appleOwner}`
        }
      })
    ])
    const registrations = [
      {
        backendId: 'unified-ble:web-bluetooth',
        prepare: () => undefined,
        exclusions: ['web:continuous-scan', 'web:background-operation', 'web:state-restoration', 'web:live-radio']
      },
      { backendId: 'unified-ble:corebluetooth', prepare: () => undefined, exclusions: [] },
      {
        backendId: 'unified-ble:bluez-dbus',
        prepare: () => undefined,
        exclusions: [
          'bluez:acquire-write',
          'bluez:acquire-notify',
          'bluez:pairing-agent',
          'bluez:deterministic-advanced-scenario-controls',
          'bluez:live-radio'
        ]
      },
      { backendId: 'unified-ble:winrt', prepare: () => undefined, exclusions: ['winrt:live-radio'] },
      {
        backendId: 'unified-ble:react-native-android',
        prepare: () => undefined,
        exclusions: []
      },
      {
        backendId: 'unified-ble:react-native-apple',
        prepare: () => undefined,
        exclusions: [BUILT_IN_FEATURE_IDS.connectionRequestMtu]
      }
    ]

    expect(registry.registeredBackendIds()).toEqual(registrations.map(registration => registration.backendId))
    for (const registration of registrations) {
      registration.prepare()
      const report = await registry.run(registration.backendId)
      expect(report.backendId).toBe(registration.backendId)
      expect(report.standard.receipts.length).toBeGreaterThan(0)
      const baseReceipts = report.standard.receipts.filter(receipt =>
        report.standard.baseScenarioIds.includes(receipt.scenarioId)
      )
      expect(baseReceipts.map(receipt => receipt.scenarioId)).toEqual(report.standard.baseScenarioIds)
      expect(baseReceipts).toEqual(
        report.standard.baseScenarioIds.map(scenarioId =>
          expect.objectContaining({
            scenarioId,
            error: null,
            facts: expect.arrayContaining([expect.objectContaining({ holds: true })])
          })
        )
      )
      expect(report.standard.receipts).toEqual(
        expect.arrayContaining([
          expect.objectContaining({
            scenarioId: 'gatt.duplicate-uuid-occurrences-route-exactly',
            error: null,
            facts: [
              expect.objectContaining({
                id: 'gatt-duplicate-uuid-occurrences-are-indexed-per-parent',
                holds: true,
                detail: expect.objectContaining({
                  pathsUnique: true,
                  parentsResolve: true,
                  occurrencesExact: true,
                  distinctServiceUuids: 2,
                  maximumServiceOccurrence: 1,
                  maximumCharacteristicOccurrence: 1,
                  maximumDescriptorOccurrence: 1
                })
              }),
              expect.objectContaining({
                id: 'gatt-duplicate-uuid-notifications-route-to-exact-instance',
                holds: true,
                detail: { routedTargets: 3, routedExactly: true }
              })
            ]
          })
        ])
      )
      for (const receipt of report.standard.receipts) {
        expect(receipt.error).toBeNull()
        expect(receipt.facts.length).toBeGreaterThan(0)
        expect(receipt.facts.every(fact => fact.holds)).toBe(true)
      }
      if (registration.backendId === 'unified-ble:web-bluetooth') {
        expect(report.standard.featureSuiteIds).toEqual(['web-chooser-discovery'])
        expect(report.standard.receipts).toEqual(
          expect.arrayContaining([
            expect.objectContaining({
              scenarioId: 'web.chooser-connect-discover-read-notify-destroy',
              error: null,
              facts: [
                expect.objectContaining({
                  id: 'web-chooser-vertical-slice-preserves-selection-and-cleans-up',
                  holds: true,
                  detail: expect.objectContaining({ cancelledPeerRejected: true })
                })
              ]
            })
          ])
        )
      }
      if (registration.backendId === 'unified-ble:react-native-android') {
        expect(report.standard.featureSuiteIds).toContain('tck.feature.security.android')
        expect(report.standard.receipts).toEqual(
          expect.arrayContaining([
            expect.objectContaining({
              scenarioId: 'security.state-pair-cancel-unpair',
              error: null,
              facts: expect.arrayContaining([
                expect.objectContaining({ id: 'security-pairing-cancellation-cleans-up', holds: true }),
                expect.objectContaining({ id: 'security-unpair-is-explicit', holds: true })
              ])
            })
          ])
        )
      }
      expect(report.capabilityExclusions.map(exclusion => exclusion.featureId)).toEqual(registration.exclusions)
    }
    const webScenarioBoundary = webBoundaries.find(boundary => boundary.resourceSnapshot().chooserRequests === 2)
    expect(webScenarioBoundary).toBeDefined()
    expect(webScenarioBoundary.resourceSnapshot()).toMatchObject({
      lastChooserRequest: {
        filters: [{ services: [SERVICE_UUID], manufacturerData: [], namePrefix: null }],
        acceptAllDevices: false,
        optionalServices: [SERVICE_UUID, BATTERY_SERVICE_UUID]
      },
      connected: false,
      disconnectListeners: 0,
      notificationListeners: 0,
      activeTimers: 0
    })
    expect([...webScenarioBoundary.expectedReadValue]).toEqual([0, 72])
    expect([...webScenarioBoundary.expectedInitialNotificationValue]).toEqual([0, 73])
  })

  test('does not fabricate skipped system-only security outcomes in receipt details', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const registration = createReactNativeAndroidFirstPartyTckRegistration({
      native,
      now: () => 20,
      nativePeerId: RUST_CORE_PEER_ID,
      boundary: {
        ...deterministicRustCoreTckBoundary(native),
        seedRestorationJournal: () => native.seedRestored([{ peerId: 'C0:FF:EE:00:00:03', connected: true }])
      },
      security: {
        customCeremonySupported: false,
        supportsAlreadyUnpaired: false,
        supportsCancellation: false,
        supportsUnpair: false
      }
    })

    const report = await createFirstPartyBackendTckRegistry([registration]).run('unified-ble:react-native-android')
    const receipt = report.standard.receipts.find(
      candidate => candidate.scenarioId === 'security.state-pair-cancel-unpair'
    )
    expect(receipt).toBeDefined()
    expect(receipt.facts).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: 'security-pairing-cancellation-cleans-up',
          holds: true,
          detail: expect.objectContaining({
            supportsCancellation: false,
            cancelled: null,
            cancelledPair: null,
            afterCancellation: null
          })
        }),
        expect.objectContaining({
          id: 'security-unpair-is-explicit',
          holds: true,
          detail: expect.objectContaining({ supportsUnpair: false, unpaired: null })
        })
      ])
    )
  })
})

describe('duplicate-UUID occurrence scenario', () => {
  const OCCURRENCE_SCENARIO = 'gatt.duplicate-uuid-occurrences-route-exactly'

  function androidRegistration(native, boundary) {
    return createReactNativeAndroidFirstPartyTckRegistration({
      native,
      now: () => 20,
      nativePeerId: RUST_CORE_PEER_ID,
      boundary
    })
  }

  async function runOccurrenceScenario(registration) {
    const { runBackendTck } = require('../../src/tck/runner')
    return runBackendTck(registration.factory, [], {
      proofScope: 'deterministic',
      baseScenarioIds: [OCCURRENCE_SCENARIO]
    })
  }

  test('fails the routing fact when notifications are delivered by UUID instead of complete path', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    const byUuidOnly = {
      ...deterministicRustCoreTckBoundary(native),
      emitNotification: (address, bytes) =>
        native.emitNotification(bytes, address.nativePeerId, address.characteristicUuid)
    }
    await expect(runOccurrenceScenario(androidRegistration(native, byUuidOnly))).rejects.toThrow(
      'required fact gatt-duplicate-uuid-notifications-route-to-exact-instance did not hold'
    )
  })

  test('refuses a world without a repeated UUID at every level instead of passing vacuously', async () => {
    const { defaultPeripheral } = require('../../test-support/react-native/deterministic-rust-core-native')
    const peripheral = defaultPeripheral()
    const native = new DeterministicRustCoreNative({
      platform: 'android',
      peripherals: [{ ...peripheral, services: peripheral.services.slice(0, 1) }]
    })
    await expect(
      runOccurrenceScenario(androidRegistration(native, deterministicRustCoreTckBoundary(native)))
    ).rejects.toThrow('the fixture world lacks a second service UUID')
  })
})

function createWebTckBoundary() {
  return new InMemoryWebBluetoothTckBoundary({
    implementationVersion: 'first-party-registry-web-boundary',
    browserEngine: 'first-party-registry-browser'
  })
}

function webChooserRequest() {
  return {
    filters: [{ serviceUuids: [SERVICE_UUID], manufacturerData: [], localNamePrefix: null }],
    acceptAllDevices: false,
    optionalServices: [SERVICE_UUID, BATTERY_SERVICE_UUID]
  }
}

/**
 * The identity-verified binding over the checkout's REAL N-API addon: the
 * desktop legs drive the production provider on its synthetic radio.
 */
function desktopCoreBinding(platform) {
  const { bindDesktopCore } = require('../../src/desktop-core-addon')
  const { DESKTOP_RUST_CORE_PROFILES } = require('../../src/backends/desktop/desktop-rust-core-provider')
  const { addonPath, loadAddon } = require('../helpers/desktop-rust-core-harness')
  return bindDesktopCore(
    { platform, operationPrefix: DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix },
    { module: loadAddon(), path: addonPath, mode: 'source', sidecar: null }
  )
}
