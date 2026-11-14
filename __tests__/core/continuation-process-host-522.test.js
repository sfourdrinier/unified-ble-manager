'use strict'

jest.mock('../../src/manager/ble-manager', () => ({
  attachBleBackend: jest.fn(async () => ({})),
  createManagerOwnershipAuthority: jest.fn(() => ({})),
  createBleManager: jest.fn(async () => ({ destroy: async () => ({ state: 'released', failures: [] }) })),
  DEFAULT_BLE_MANAGER_OPTIONS: { maximumValueBytes: 512, traceMaximumRecords: 10, traceMaximumBytes: 1024 }
}))

const { createDesktopProcessHostFromBackend } = require('../../src/desktop-process-host')
const { DESKTOP_RUST_CORE_PROFILES } = require('../../src/backends/desktop/desktop-rust-core-provider')
const { createNativeContinuationControl } = require('../../src/core/native-continuation-control')
const envelope = value => JSON.stringify({ ok: true, value })
const claim = {
  claimToken: 'prefix',
  consumerCount: 0,
  selectors: [],
  batches: [],
  disposed: false,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: null
}
const receipt = { disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null }

test('process host forwards the complete retained handoff including exact receipt retirement', async () => {
  const access = {
    execute: jest.fn(),
    describeBacklog: jest.fn(),
    prepareClaim: jest.fn(),
    acknowledgeClaim: jest.fn(),
    prepareRetainedClaimV1: jest.fn(async () => envelope(claim)),
    acknowledgeRetainedClaimV1: jest.fn(async () => envelope(receipt)),
    retireRetainedClaimReceiptV1: jest.fn(async () => envelope({}))
  }
  const backend = {
    identity: { registeredBackendId: DESKTOP_RUST_CORE_PROFILES.bluez.backendId },
    enableProcessOwnedRecording() {},
    nativeContinuationController: () => ({}),
    nativeContinuationControlAccess: () => access,
    destroy: async () => ({ state: 'released' })
  }
  const host = await createDesktopProcessHostFromBackend(backend, {
    now: () => 0,
    randomBytes: length => new Uint8Array(length)
  })
  const prefix = await createNativeContinuationControl(host.continuationAccess).claimRetainedV1()
  await prefix.acknowledge()
  await prefix.retireReceipt()
  expect(access.retireRetainedClaimReceiptV1).toHaveBeenCalledWith('prefix')
  await host.destroy()
})

test('process host exposes durable peer-failure recovery only to its trusted owner', async () => {
  const recovery = Object.freeze({
    provenance: Object.freeze({ peerId: 'private-peer', consumer: 'private-consumer' }),
    read: jest.fn(async offset => ({ offset, bytes: new Uint8Array([1, 2]), nextOffset: '2', done: true })),
    acknowledge: jest.fn(async () => undefined)
  })
  const backend = {
    identity: { registeredBackendId: DESKTOP_RUST_CORE_PROFILES.bluez.backendId },
    enableProcessOwnedRecording() {},
    nativeContinuationController: () => ({}),
    nativeContinuationControlAccess: () => ({
      execute: jest.fn(),
      describeBacklog: jest.fn(),
      prepareClaim: jest.fn(),
      acknowledgeClaim: jest.fn()
    }),
    durablePeerNotificationFailureRecoveries: jest.fn(() => [recovery]),
    destroy: async () => ({ state: 'released' })
  }
  const host = await createDesktopProcessHostFromBackend(backend, {
    now: () => 0,
    randomBytes: length => new Uint8Array(length)
  })
  const [listed] = host.peerFailureRecovery.list()
  expect(listed).toBe(recovery)
  await listed.acknowledge()
  expect(recovery.acknowledge).toHaveBeenCalledTimes(1)
  await host.destroy()
})
