// __tests__/tck/first-party-tck-registration.test.js

const {
  createFirstPartyBackendTckRegistry,
  createWebBluetoothFirstPartyTckRegistration,
  createCoreBluetoothFirstPartyTckRegistration,
  createBluezFirstPartyTckRegistration,
  createWinRtFirstPartyTckRegistration
} = require('../../src/testing')
const {
  InMemoryWebBluetoothTckBoundary,
  WEB_BLUETOOTH_TCK_BATTERY_SERVICE_UUID: BATTERY_SERVICE_UUID
} = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { createWebBluetoothFeatureRegistry } = require('../../src/web/web-feature-registry')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'

jest.setTimeout(60000)

describe('first-party backend standard TCK registrations', () => {
  test('runs Web applicable provider and capability suites while retaining explicit platform exclusions', async () => {
    const boundaries = []
    const registration = createWebBluetoothFirstPartyTckRegistration({
      createBoundary: () => {
        const boundary = createWebTckBoundary()
        boundaries.push(boundary)
        return boundary
      },
      chooserRequest: webChooserRequest()
    })
    const registry = createFirstPartyBackendTckRegistry([registration])

    const report = await registry.run('unified-ble:web-bluetooth')

    expect(report.standard.baseScenarioIds).toEqual([
      'identity.provider-loadability-and-adapter-availability',
      'identity.adapter-selection-and-unique-instance',
      'identity.valid-all-axis-negotiation',
      'identity.version-skew-and-malformed-offers',
      'capability.truth-limits-evidence-and-binding',
      'gatt.duplicate-uuid-occurrences-route-exactly'
    ])
    expect(registration.featureSuites).toEqual([
      expect.objectContaining({
        suiteId: 'web-chooser-discovery',
        scenarioIds: [
          'web.chooser-connect-discover-read-notify-destroy',
          'web.unsupported-capabilities-reject-and-remain-honest'
        ]
      })
    ])
    expect(report.standard.featureSuiteIds).toEqual(['web-chooser-discovery'])
    expect(report.standard.featureBindings).toEqual([
      expect.objectContaining({
        featureId: 'connection:direct',
        suiteId: 'web-chooser-discovery',
        requiredScenarioIds: ['web.chooser-connect-discover-read-notify-destroy'],
        evidenceScenarioIds: ['web.chooser-connect-discover-read-notify-destroy']
      }),
      // The shared discovery vocabulary (5.0), bound to the same chooser suite.
      expect.objectContaining({
        featureId: 'discovery:system-chooser',
        suiteId: 'web-chooser-discovery',
        requiredScenarioIds: [
          'web.chooser-connect-discover-read-notify-destroy',
          'web.unsupported-capabilities-reject-and-remain-honest'
        ],
        evidenceScenarioIds: [
          'web.chooser-connect-discover-read-notify-destroy',
          'web.unsupported-capabilities-reject-and-remain-honest'
        ]
      }),
      expect.objectContaining({
        featureId: 'web:chooser-discovery',
        suiteId: 'web-chooser-discovery',
        requiredScenarioIds: [
          'web.chooser-connect-discover-read-notify-destroy',
          'web.unsupported-capabilities-reject-and-remain-honest'
        ],
        evidenceScenarioIds: [
          'web.chooser-connect-discover-read-notify-destroy',
          'web.unsupported-capabilities-reject-and-remain-honest'
        ]
      })
    ])
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
        }),
        expect.objectContaining({
          scenarioId: 'web.unsupported-capabilities-reject-and-remain-honest',
          error: null,
          facts: [
            expect.objectContaining({
              id: 'web-unsupported-capabilities-reject-and-report-runtime-truth',
              holds: true,
              detail: expect.objectContaining({
                scanRejected: true,
                resourcesReleased: true,
                unsupportedFeatureIds: ['web:background-operation', 'web:continuous-scan', 'web:state-restoration']
              })
            })
          ]
        })
      ])
    )
    const scenarioBoundary = boundaries.find(boundary => boundary.resourceSnapshot().chooserRequests === 2)
    expect(scenarioBoundary).toBeDefined()
    expect(scenarioBoundary.resourceSnapshot()).toMatchObject({
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
    expect([...scenarioBoundary.expectedReadValue]).toEqual([0, 72])
    expect([...scenarioBoundary.expectedInitialNotificationValue]).toEqual([0, 73])
    const controllerFixture = await registration.factory.create({
      scenarioId: 'web.chooser-connect-discover-read-notify-destroy'
    })
    expect(controllerFixture.controller.availableActions).toEqual(['resolve-chooser', 'emit-notification'])
    expect(controllerFixture.controller.availableActions).not.toContain('queue-advertisement')
    const controllerBoundary = boundaries[boundaries.length - 1]
    expect(controllerBoundary).toBeDefined()
    expect(controllerFixture.featureScenarioAdapters.webChooser.expectedSelectedPeerId).toBe(
      controllerBoundary.expectedSelectedPeerId
    )
    expect(typeof controllerFixture.featureScenarioAdapters.webChooser.expectedSelectedPeerId).toBe('string')
    await controllerFixture.dispose()
    expect(report.capabilityExclusions.map(exclusion => exclusion.featureId)).toEqual([
      'web:continuous-scan',
      'web:background-operation',
      'web:state-restoration',
      'web:live-radio'
    ])
  })

  test('binds every blocked Web capability to the runner-owned unsupported-capability receipt', () => {
    const registry = createWebBluetoothFeatureRegistry('web-registry-receipt-binding-test')
    const canonicalReceiptId = 'web.unsupported-capabilities-reject-and-remain-honest'
    const expectedFeatureIds = ['web:background-operation', 'web:continuous-scan', 'web:state-restoration']

    for (const featureId of expectedFeatureIds) {
      const feature = registry.registrations.find(registration => registration.id === featureId)
      if (feature === undefined) {
        throw new Error(`Web feature registry lacks ${featureId}`)
      }
      expect(feature.tck.requiredScenarioIds).toEqual([canonicalReceiptId])
      expect(feature.evidence.scenarioIds).toEqual([canonicalReceiptId])
    }
  })

  test('executes the Web chooser scenario with contract-valid empty read and initial notification values', async () => {
    const boundaries = []
    const registry = createFirstPartyBackendTckRegistry([
      createWebBluetoothFirstPartyTckRegistration({
        createBoundary: () => {
          const boundary = new InMemoryWebBluetoothTckBoundary({
            implementationVersion: 'web-zero-length-tck-boundary',
            browserEngine: 'web-zero-length-tck-browser',
            expectedReadValue: new Uint8Array(),
            expectedInitialNotificationValue: new Uint8Array()
          })
          boundaries.push(boundary)
          return boundary
        },
        chooserRequest: webChooserRequest()
      })
    ])

    const report = await registry.run('unified-ble:web-bluetooth')
    const receipt = report.standard.receipts.find(
      candidate => candidate.scenarioId === 'web.chooser-connect-discover-read-notify-destroy'
    )
    expect(receipt).toMatchObject({
      error: null,
      facts: [
        expect.objectContaining({
          id: 'web-chooser-vertical-slice-preserves-selection-and-cleans-up',
          holds: true,
          detail: expect.objectContaining({
            cancelledPeerRejected: true,
            exactInitialNotification: true,
            exactReadBytes: true,
            exactSecondNotification: true,
            ownedReadBytes: true
          })
        })
      ]
    })
    const scenarioBoundary = boundaries.find(boundary => boundary.resourceSnapshot().chooserRequests === 2)
    expect(scenarioBoundary).toBeDefined()
    expect([...scenarioBoundary.expectedReadValue]).toEqual([])
    expect([...scenarioBoundary.expectedInitialNotificationValue]).toEqual([])
    expect(scenarioBoundary.resourceSnapshot()).toMatchObject({
      connected: false,
      disconnectListeners: 0,
      notificationListeners: 0,
      activeTimers: 0
    })
  })

  // The desktop legs run the Rust route (LEGACY-AUDIT-2 N3): the production
  // desktop provider over the REAL N-API addon on its synthetic radio.
  test.each(['corebluetooth', 'winrt'])(
    '%s deferred feature reaches native acquisition, not only a catalog binding',
    async platform => {
      const { openBackend, observePeer, callNames } = require('../helpers/desktop-rust-core-harness')
      const { backend, stage, harness } = await openBackend(platform)
      try {
        const peerId = await observePeer(backend, stage)
        const lease = await backend.connections.connect(peerId, 'tck-deferred', {
          intent: 'when-available',
          signal: null,
          deadline: performance.now() + 1000
        })
        expect(callNames(harness.calls)).toContain('connectWhenAvailable')
        expect(callNames(harness.calls)).not.toContain('connect')
        expect(await lease.release()).toEqual({ state: 'released', failures: [] })
      } finally {
        await backend.destroy()
      }
    }
  )

  test('runs every applicable CoreBluetooth scenario on the Rust route without unsupported promotion', async () => {
    const harness = require('../helpers/desktop-rust-core-harness').realBinding('corebluetooth')
    const registration = createCoreBluetoothFirstPartyTckRegistration({
      now: () => performance.now(),
      binding: harness.binding
    })
    const report = await createFirstPartyBackendTckRegistry([registration]).run('unified-ble:corebluetooth')

    expect(report.standard.baseScenarioIds).toEqual([
      'identity.provider-loadability-and-adapter-availability',
      'identity.adapter-selection-and-unique-instance',
      'identity.valid-all-axis-negotiation',
      'identity.version-skew-and-malformed-offers',
      'capability.truth-limits-evidence-and-binding',
      'adapter.atomic-snapshot-and-watch',
      'scan.owner-join-authority-and-signature',
      'scan.fairness-abort-deadline-and-final-cleanup',
      'connection.lease-joins-borrowing-transfer-and-revocation',
      'connection.two-client-arbitration',
      'gatt.discovery-complete-paths-and-services-changed',
      'gatt.duplicate-uuid-occurrences-route-exactly',
      'diagnostics.trace-redaction-and-resource-counters',
      'scenario.scan-connect-discover-read-notify-destroy'
    ])
    expect(report.standard.featureSuiteIds).toEqual([
      'tck.feature.connection.when-available',
      'connection-controls',
      'tck.feature.gatt.maximum-write-length'
    ])
    expect(report.standard.featureBindings.map(binding => binding.featureId)).toEqual([
      'connection:direct',
      'gatt:descriptors',
      BUILT_IN_FEATURE_IDS.peerKnown,
      BUILT_IN_FEATURE_IDS.peerSystemConnected,
      BUILT_IN_FEATURE_IDS.connectionWhenAvailable,
      BUILT_IN_FEATURE_IDS.connectionRssi,
      // finding 217 follow-up: desktop routes measure the effective ATT MTU
      // (macOS maximumWriteValueLength + 3, WinRT MaxPduSize, BlueZ characteristic MTU).
      BUILT_IN_FEATURE_IDS.connectionEffectiveMtu,
      'gatt:maximum-write-length',
      BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness
    ])
    expectEveryReceiptHolds(report)
    const deferredBinding = report.standard.featureBindings.find(
      binding => binding.featureId === BUILT_IN_FEATURE_IDS.connectionWhenAvailable
    )
    expect(deferredBinding).toMatchObject({
      suiteId: 'tck.feature.connection.when-available',
      requiredScenarioIds: ['connection.when-available-acquires-and-releases'],
      evidenceScenarioIds: ['connection.when-available-acquires-and-releases']
    })
    for (const scenarioId of deferredBinding.evidenceScenarioIds) {
      const receipt = report.standard.receipts.find(candidate => candidate.scenarioId === scenarioId)
      expect(receipt).toBeDefined()
      expect(receipt.error).toBeNull()
      expect(receipt.facts.length).toBeGreaterThan(0)
      expect(receipt.facts.every(fact => fact.holds)).toBe(true)
    }
    expect(harness.calls.filter(([name]) => name === 'connectWhenAvailable')).toHaveLength(1)
    expect(report.standard.receipts).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          scenarioId: 'connection.rssi-and-att-mtu-capability-contract',
          facts: expect.arrayContaining([
            expect.objectContaining({
              id: 'connection-att-mtu-is-negotiated-or-explicitly-unavailable',
              holds: true,
              detail: expect.objectContaining({ mtuExplicitlyUnavailable: true })
            })
          ])
        }),
        expect.objectContaining({
          scenarioId: 'gatt.discovery-complete-paths-and-services-changed',
          facts: expect.arrayContaining([
            expect.objectContaining({ id: 'gatt-services-changed-invalidates-database-generation', holds: true })
          ])
        })
      ])
    )
    expect(report.standard.featureBindings.map(binding => binding.featureId)).not.toContain(
      BUILT_IN_FEATURE_IDS.connectionRequestMtu
    )
    expect(report.capabilityExclusions).toEqual([])
  })

  test.each([
    ['bluez', 'unified-ble:bluez-dbus', 'bluez-provider-contract-v1', 'tck.feature.security.bluez'],
    ['winrt', 'unified-ble:winrt', 'winrt-provider-contract-v2', 'tck.feature.security.winrt']
  ])(
    'runs the %s provider, public vertical and security profile on the Rust route',
    async (platform, backendId, suiteId, securitySuite) => {
      const create = platform === 'bluez' ? createBluezFirstPartyTckRegistration : createWinRtFirstPartyTckRegistration
      const inventory = [
        {
          peerId: platform === 'bluez' ? 'hci0/dev_AA_BB_CC_DD_EE_FF' : 'AA:BB:CC:DD:EE:FF',
          name: 'TCK bonded peer',
          connection: 'disconnected'
        }
      ]
      const inventoryRead = jest.spyOn(inventory, 'map')
      const harness = require('../helpers/desktop-rust-core-harness').realBinding(platform)
      const registration = create({
        now: () => performance.now(),
        binding: harness.binding,
        bondedInventory: inventory
      })
      expect(registration.suites.map(suite => suite.suiteId)).toEqual([suiteId])
      const report = await createFirstPartyBackendTckRegistry([registration]).run(backendId)

      if (platform === 'winrt') {
        const controls = report.standard.receipts.find(
          receipt => receipt.scenarioId === 'connection.rssi-and-att-mtu-capability-contract'
        )
        expect(controls.facts).toEqual(
          expect.arrayContaining([
            expect.objectContaining({
              id: 'connection-phy-truth-is-explicit',
              holds: true,
              detail: expect.objectContaining({
                readMeasured: true,
                txPhy: 'le-2m',
                rxPhy: 'le-coded',
                requestExplicitlyUnsupported: true,
                requestErrorCode: 'capability.unsupported'
              })
            })
          ])
        )
        expect(harness.calls.some(([method]) => method === 'readPhy')).toBe(true)
      }

      expect(report.standard.baseScenarioIds).toEqual([
        'identity.provider-loadability-and-adapter-availability',
        'identity.adapter-selection-and-unique-instance',
        'identity.valid-all-axis-negotiation',
        'identity.version-skew-and-malformed-offers',
        'capability.truth-limits-evidence-and-binding',
        'adapter.atomic-snapshot-and-watch',
        'gatt.duplicate-uuid-occurrences-route-exactly',
        'scenario.scan-connect-discover-read-notify-destroy'
      ])
      // finding 217 follow-up: BlueZ and WinRT now measure the effective ATT MTU,
      // so the connection-controls suite applies to them as it does on macOS.
      expect(report.standard.featureSuiteIds).toEqual([
        'tck.feature.peer.bonded',
        ...(['winrt', 'bluez'].includes(platform) ? ['tck.feature.connection.when-available'] : []),
        'connection-controls',
        securitySuite,
        'tck.feature.gatt.maximum-write-length'
      ])
      expectEveryReceiptHolds(report)
      const parameterCalls = harness.calls.filter(([name]) => name === 'connectionParameters')
      if (platform === 'winrt') {
        // Each bound capability exercises the shared controls scenario;
        // adding a callable feature must not impose a stale exact call count.
        expect(parameterCalls.length).toBeGreaterThanOrEqual(3)
      } else {
        expect(parameterCalls).toHaveLength(0)
      }
      expect(inventoryRead).toHaveBeenCalledTimes(1)
      expect(harness.calls.filter(([name]) => name === 'connectWhenAvailable')).toHaveLength(
        ['winrt', 'bluez'].includes(platform) ? 1 : 0
      )
      expect(
        report.standard.featureBindings.find(binding => binding.featureId === BUILT_IN_FEATURE_IDS.peerBonded)
      ).toMatchObject({
        suiteId: 'tck.feature.peer.bonded',
        requiredScenarioIds: ['peer.bonded-enumeration-preserves-native-facts'],
        evidenceScenarioIds: ['peer.bonded-enumeration-preserves-native-facts']
      })
      const security = report.standard.receipts.filter(
        receipt => receipt.scenarioId === 'security.state-pair-cancel-unpair'
      )
      expect(security.length).toBeGreaterThan(0)
      expect(security[0].facts).toEqual(
        expect.arrayContaining([
          expect.objectContaining({
            id: 'security-pairing-cancellation-cleans-up',
            holds: true,
            detail: expect.objectContaining({
              cancelled: 'cancelled',
              cancelledPair: 'cancelled',
              afterCancellation: 'not-pairing'
            })
          }),
          expect.objectContaining({
            id: 'security-pairing-is-terminal-and-idempotent',
            holds: true,
            detail: { paired: 'paired', alreadyPaired: 'already-paired' }
          })
        ])
      )
      expect(
        report.capabilityExclusions.map(exclusion => ({ featureId: exclusion.featureId, state: exclusion.state }))
      ).toEqual(
        platform === 'bluez'
          ? [
              { featureId: 'bluez:acquire-write', state: 'unavailable' },
              { featureId: 'bluez:acquire-notify', state: 'unavailable' },
              { featureId: 'bluez:pairing-agent', state: 'unsupported' },
              { featureId: 'bluez:deterministic-advanced-scenario-controls', state: 'unavailable' },
              { featureId: 'bluez:live-radio', state: 'unavailable' }
            ]
          : [{ featureId: 'winrt:live-radio', state: 'unavailable' }]
      )
      expect(report.capabilityExclusions.every(exclusion => exclusion.reason.length > 0)).toBe(true)
    }
  )

  test.each(['bluez', 'winrt'])('%s bonded receipt refuses an empty native inventory', async platform => {
    const create = platform === 'bluez' ? createBluezFirstPartyTckRegistration : createWinRtFirstPartyTckRegistration
    const registration = create({
      now: () => performance.now(),
      binding: desktopCoreBinding(platform),
      bondedInventory: []
    })
    await expect(createFirstPartyBackendTckRegistry([registration]).run(registration.backendId)).rejects.toThrow(
      'required fact peer-bonded-enumeration-preserves-native-facts did not hold'
    )
  })

  test.each(['corebluetooth', 'winrt'])('%s deferred receipt refuses a missing native acquisition', async platform => {
    const create =
      platform === 'corebluetooth' ? createCoreBluetoothFirstPartyTckRegistration : createWinRtFirstPartyTckRegistration
    const binding = desktopCoreBinding(platform)
    const calls = []
    const registration = create({
      now: () => performance.now(),
      binding: {
        ...binding,
        openSynthetic: async (...args) => {
          const central = await binding.openSynthetic(...args)
          return new Proxy(central, {
            get(target, property) {
              if (property === 'connectWhenAvailable')
                return () => {
                  calls.push(property)
                  throw new Error('native deferred acquisition refused')
                }
              const value = Reflect.get(target, property)
              return typeof value === 'function' ? value.bind(target) : value
            }
          })
        }
      }
    })
    await expect(createFirstPartyBackendTckRegistry([registration]).run(registration.backendId)).rejects.toThrow()
    expect(calls).toEqual(['connectWhenAvailable'])
  })

  test('a desktop leg never opens a production radio', async () => {
    const binding = desktopCoreBinding('bluez')
    const openProduction = jest.fn(binding.openProduction)
    const registration = createBluezFirstPartyTckRegistration({
      now: () => performance.now(),
      binding: { ...binding, openProduction }
    })
    const [adapter] = await registration.factory.provider.listAdapters()
    const backend = await registration.factory.provider.create({ selectedAdapterId: adapter.adapterId })
    expect(backend.identity.runtime.diagnostics).toMatchObject({ radio: 'synthetic', transport: 'napi-UbmCentral' })
    await backend.destroy()
    expect(openProduction).not.toHaveBeenCalled()
  })

  test('a desktop leg refuses a central without the synthetic staging surface', async () => {
    const binding = desktopCoreBinding('winrt')
    const closed = []
    const registration = createWinRtFirstPartyTckRegistration({
      now: () => performance.now(),
      binding: {
        ...binding,
        openSynthetic: async (owner, options) => {
          const central = await binding.openSynthetic(owner, options)
          const surface = {}
          for (const name of ['close', 'createTicket', 'adapterState']) {
            surface[name] = (...args) => central[name](...args)
          }
          surface.close = async () => {
            closed.push(owner)
            return central.close()
          }
          return surface
        }
      }
    })
    await expect(registration.factory.provider.listAdapters()).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible', operation: 'winrt.tck.synthetic-surface' }
    })
    expect(closed).toHaveLength(1)
  })
})

function expectEveryReceiptHolds(report) {
  expect(report.standard.receipts.length).toBeGreaterThan(report.standard.baseScenarioIds.length)
  for (const receipt of report.standard.receipts) {
    expect({ scenarioId: receipt.scenarioId, error: receipt.error }).toEqual({
      scenarioId: receipt.scenarioId,
      error: null
    })
    expect(receipt.facts.every(fact => fact.holds)).toBe(true)
  }
}

/** The identity-verified binding over the checkout's REAL N-API addon. */
function desktopCoreBinding(platform) {
  const { bindDesktopCore } = require('../../src/desktop-core-addon')
  const { DESKTOP_RUST_CORE_PROFILES } = require('../../src/backends/desktop/desktop-rust-core-provider')
  const { addonPath, loadAddon } = require('../helpers/desktop-rust-core-harness')
  return bindDesktopCore(
    { platform, operationPrefix: DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix },
    { module: loadAddon(), path: addonPath, mode: 'source', sidecar: null }
  )
}

function createWebTckBoundary() {
  return new InMemoryWebBluetoothTckBoundary({
    implementationVersion: 'web-first-party-tck-boundary',
    browserEngine: 'first-party-tck-browser'
  })
}

function webChooserRequest() {
  return {
    filters: [{ serviceUuids: [SERVICE_UUID], manufacturerData: [], localNamePrefix: null }],
    acceptAllDevices: false,
    optionalServices: [SERVICE_UUID, BATTERY_SERVICE_UUID]
  }
}
