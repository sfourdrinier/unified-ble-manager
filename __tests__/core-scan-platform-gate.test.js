// __tests__/core-scan-platform-gate.test.js
// ScanOptions.platform reaches the backend contract, so the exported
// host-neutral BleManager.scan() must not bypass the public manager's
// capability gate: a backend that does not register scan:platform-options
// (the deterministic backend included) would silently scan with its own
// defaults. The core path therefore fails closed for every caller.
const {
  BleManager: InternalBleManager,
  attachBleBackend,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../src/advanced')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
const { capacity, opaqueId, version, versionRange } = require('../src/backend-contract/primitives')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function scanOptions(platform) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false },
    ...(platform === undefined ? {} : { platform })
  }
}

async function createManagerFixture() {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const now = () => Number(fixture.controller.clock.now())
  const internal = await InternalBleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('core-scan-platform-client', 'client', 'core-scan-platform'),
      managerId: opaqueId('core-scan-platform-manager', 'manager', 'core-scan-platform'),
      ownerMode: 'owning'
    },
    authority,
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
  )
  return { fixture, internal }
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
  for (let attempt = 0; attempt < 20 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

describe('host-neutral scan platform capability gate', () => {
  test('rejects platform scan options on a backend without scan:platform-options', async () => {
    const { internal } = await createManagerFixture()
    await expect(internal.scan(scanOptions({ kind: 'android', mode: 'low-power' }))).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported', operation: 'unified-core.scan.platform-options' }
    })
    await expect(internal.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('still scans without platform options on the same backend', async () => {
    const { fixture, internal } = await createManagerFixture()
    const session = await settle(fixture, internal.scan(scanOptions()))
    await expect(settle(fixture, session.stop())).resolves.toMatchObject({ state: 'released' })
    await expect(settle(fixture, internal.destroy())).resolves.toMatchObject({ state: 'released' })
  })
})
