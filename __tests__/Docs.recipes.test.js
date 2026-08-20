// __tests__/Docs.recipes.test.js

const fs = require('fs')
const path = require('path')
const {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority
} = require('../src')
const { DEFAULT_BLE_MANAGER_OPTIONS } = require('../src/manager/ble-manager')
const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')
const { canonicalUuid, VirtualPeripheral } = require('../src/testing')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
const { deterministicScenarioAdvertisement } = require('../src/testing/scenarios/manager-scenario-executor')
const { HEART_RATE_SERVICE } = require('../src/profiles/heart-rate')
const { runFiniteHrsJourney } = require('./docs-recipes/finite-hrs-journey')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function hrsPeripheral() {
  return new VirtualPeripheral({
    key: 'finite-hrs-recipe',
    services: [
      {
        uuid: canonicalUuid('180d'),
        occurrence: 0,
        primary: true,
        characteristics: [
          {
            uuid: canonicalUuid('2a37'),
            occurrence: 0,
            initialValue: new Uint8Array([0x06, 72]),
            readable: false,
            writableWithResponse: false,
            writableWithoutResponse: false,
            notifying: true,
            indicating: false,
            descriptors: []
          }
        ]
      }
    ]
  })
}

async function createRecipeFixture() {
  const fixture = createDeterministicTestBackend({ peripheral: hrsPeripheral() })
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const manager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('recipe-client', 'client', 'deterministic:recipe'),
      managerId: opaqueId('recipe-manager', 'manager', 'deterministic:recipe'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    {
      ...DEFAULT_BLE_MANAGER_OPTIONS,
      now: () => fixture.controller.clock.now(),
      timer: {
        scheduleAt: (deadlineValue, action) => fixture.controller.clock.scheduleAt(deadlineValue, action)
      }
    }
  )
  return { fixture, manager }
}

async function settle(fixture, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

function expectZeroCounters(counters) {
  expect(Object.entries(counters).filter(([, value]) => Number(value) !== 0)).toEqual([])
}

describe('finite HRS documentation recipe', () => {
  test('README marks the helper-first journey', () => {
    const readme = fs.readFileSync(path.join(__dirname, '..', 'README.md'), 'utf8')
    expect(readme).toContain('// @ubm-recipe finite-hrs')
    expect(readme).toContain('scanUntil')
    expect(readme).toContain('withConnection')
    expect(readme).toContain('firstNotification')
    expect(readme).not.toContain('batteryLevelSelector()')
  })

  test('executes against DeterministicTestBackend without Battery or Control Point', async () => {
    const { fixture, manager } = await createRecipeFixture()
    const measurement = await runFiniteHrsJourney({
      manager,
      settle: promise => settle(fixture, promise),
      onPeer: async () => {
        fixture.controller.clock.runUntilIdle()
        for (let turn = 0; turn < 8; turn += 1) {
          await Promise.resolve()
        }
        const advertisement = deterministicScenarioAdvertisement()
        fixture.controller.emitAdvertisement({
          ...advertisement,
          serviceUuids: { state: 'present', value: [HEART_RATE_SERVICE], provenance: 'observed' }
        })
      },
      onNotify: async path => {
        fixture.controller.clock.runUntilIdle()
        for (let turn = 0; turn < 8; turn += 1) {
          await Promise.resolve()
        }
        fixture.controller.emitNotification(
          {
            serviceUuid: path.serviceUuid,
            serviceOccurrence: Number(path.serviceOccurrence),
            characteristicUuid: path.characteristicUuid,
            characteristicOccurrence: Number(path.characteristicOccurrence)
          },
          new Uint8Array([0x06, 76])
        )
      }
    })
    expect(measurement.beatsPerMinute).toBe(76)
    expectZeroCounters(fixture.backend.resourceCounters())
  }, 15000)
})
