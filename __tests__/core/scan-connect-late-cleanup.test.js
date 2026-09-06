const { attachBleBackend, createBleManager, createManagerOwnershipAuthority } = require('../../src/manager/ble-manager')
const { DEFAULT_BLE_MANAGER_OPTIONS } = require('../../src/manager/ble-manager')
const { capacity, deadline, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { awaitSignal } = require('../helpers/async')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function releaseFailedRecord(resourceKind, operation) {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind,
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation,
          platform: null,
          retryability: 'never'
        }
      }
    ]
  }
}

function releasedRecord() {
  return { state: 'released', failures: [] }
}

function createCloser(resourceKind, operation, policy) {
  let attempts = 0
  let held = null
  let resolveFirstAttempt
  const firstAttempt = new Promise(resolve => {
    resolveFirstAttempt = resolve
  })
  return {
    attempts() {
      return attempts
    },
    firstAttempt,
    hold() {
      let resolveHold
      const promise = new Promise(resolve => {
        resolveHold = resolve
      })
      held = { promise, resolve: resolveHold }
      return () => {
        if (held === null) {
          return
        }
        held.resolve()
        held = null
      }
    },
    async close() {
      attempts += 1
      if (attempts === 1) {
        resolveFirstAttempt()
      }
      if (held !== null) {
        await held.promise
      }
      if (policy === 'fail-then-succeed') {
        return attempts === 1 ? releaseFailedRecord(resourceKind, operation) : releasedRecord()
      }
      return releasedRecord()
    }
  }
}

function scanOptions(signal = null, operationDeadline = null) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(1024),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: operationDeadline,
    signal,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

function operation(signal = null, operationDeadline = null) {
  return { signal, deadline: operationDeadline }
}

function peer() {
  return opaqueId('deterministic-peer', 'peer', 'deterministic')
}

function managerOptions(fixture) {
  return {
    ...DEFAULT_BLE_MANAGER_OPTIONS,
    now: () => fixture.controller.clock.now(),
    timer: {
      scheduleAt: (deadlineValue, action) => fixture.controller.clock.scheduleAt(deadlineValue, action)
    }
  }
}

async function createOwningFixture() {
  const fixture = createDeterministicTestBackend()
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const manager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('scan-connect-client', 'client', 'deterministic:scan-connect-client'),
      managerId: opaqueId('scan-connect-manager', 'manager', 'deterministic:scan-connect-manager'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    managerOptions(fixture)
  )
  return { fixture, manager }
}

async function createBorrowedFixture() {
  const fixture = createDeterministicTestBackend()
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attached)
  const owner = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('scan-connect-owner-client', 'client', 'deterministic:scan-connect-owner-client'),
      managerId: opaqueId('scan-connect-owner-manager', 'manager', 'deterministic:scan-connect-owner-manager'),
      ownerMode: 'owning'
    },
    authority,
    managerOptions(fixture)
  )
  const manager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('scan-connect-borrower-client', 'client', 'deterministic:scan-connect-borrower-client'),
      managerId: opaqueId('scan-connect-borrower-manager', 'manager', 'deterministic:scan-connect-borrower-manager'),
      ownerMode: 'borrowing'
    },
    authority,
    managerOptions(fixture)
  )
  return { fixture, manager, owner }
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

async function settle(controller, promise) {
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
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

function gateBackendMethod(target, methodName, wrapLease, toNativeArgs) {
  const original = target[methodName].bind(target)
  let releaseGate
  const gate = new Promise(resolve => {
    releaseGate = resolve
  })
  let resolveLease
  const leaseCreated = new Promise(resolve => {
    resolveLease = resolve
  })
  target[methodName] = async (...args) => {
    await gate
    const lease = await original(...toNativeArgs(args))
    wrapLease(lease)
    resolveLease(lease)
    return lease
  }
  return {
    release() {
      releaseGate()
    },
    leaseCreated
  }
}

function withoutCallerAdmission(options) {
  return { ...options, signal: null, deadline: null }
}

function installScanAcquisition(backend, closer) {
  return gateBackendMethod(
    backend.scanner,
    'start',
    lease => {
      const originalStop = lease.stop.bind(lease)
      lease.stop = async () => {
        const result = await closer.close()
        if (result.state === 'released') {
          return originalStop()
        }
        return result
      }
    },
    args => [withoutCallerAdmission(args[0]), args[1]]
  )
}

function installConnectAcquisition(backend, closer) {
  return gateBackendMethod(
    backend.connections,
    'connect',
    lease => {
      const originalRelease = lease.release.bind(lease)
      lease.release = async () => {
        const result = await closer.close()
        if (result.state === 'released') {
          return originalRelease()
        }
        return result
      }
    },
    args => [args[0], args[1], withoutCallerAdmission(args[2])]
  )
}

const acquisitions = [
  {
    name: 'scan',
    resourceKind: 'scan',
    operation: 'scan-stale-admission-release',
    install: installScanAcquisition,
    start: (manager, signal) => manager.scan(scanOptions(signal))
  },
  {
    name: 'connect',
    resourceKind: 'connection',
    operation: 'connect-stale-admission-release',
    install: installConnectAcquisition,
    start: (manager, signal) => manager.connect(peer(), operation(signal))
  }
]

describe('pending scan/connect acquisition ownership', () => {
  test.each(acquisitions)('destroy during pending $name retains a failed late lease for retry', async acquisition => {
    const closer = createCloser(acquisition.resourceKind, acquisition.operation, 'fail-then-succeed')
    const { manager, fixture } = await createOwningFixture()
    const gate = acquisition.install(fixture.backend, closer)
    const pending = acquisition.start(manager, null)
    const rejected = expect(pending).rejects.toMatchObject({
      normalized: { code: 'operation.cancelled-by-destroy' }
    })

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await rejected
    expect(closer.attempts()).toBe(0)

    gate.release()
    await settle(fixture.controller, gate.leaseCreated)
    await awaitSignal(closer.firstAttempt, `the late ${acquisition.name} cleanup to run`)
    await flushMicrotasks()
    expect(closer.attempts()).toBe(1)

    await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(2)
  })

  test.each(acquisitions)(
    'abort settles before backend $name acquisition, and late failed cleanup remains retryable',
    async acquisition => {
      const closer = createCloser(acquisition.resourceKind, acquisition.operation, 'fail-then-succeed')
      const { manager, fixture } = await createOwningFixture()
      const gate = acquisition.install(fixture.backend, closer)
      const controller = new AbortController()
      const pending = acquisition.start(manager, controller.signal)
      controller.abort()
      await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      expect(closer.attempts()).toBe(0)

      await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      gate.release()
      await settle(fixture.controller, gate.leaseCreated)
      await awaitSignal(closer.firstAttempt, `the late ${acquisition.name} cleanup to run`)
      await flushMicrotasks()
      expect(closer.attempts()).toBe(1)
      expect(manager.traces()).toEqual(
        expect.arrayContaining([
          expect.objectContaining({
            transition: `${acquisition.name}-stale-admission-release`,
            cause: 'platform.failure'
          })
        ])
      )

      await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({ state: 'released' })
      expect(closer.attempts()).toBe(2)
    }
  )

  test('borrowed-manager connect cleanup retries the exact lease without destroying owner resources', async () => {
    const closer = createCloser('connection', 'connect-stale-admission-release', 'fail-then-succeed')
    const { fixture, manager, owner } = await createBorrowedFixture()
    const ownerConnection = await settle(fixture.controller, owner.connect(peer(), operation()))
    const otherPeer = opaqueId('deterministic-other-peer', 'peer', 'deterministic')
    const gate = installConnectAcquisition(fixture.backend, closer)
    const pending = manager.connect(otherPeer, operation())
    const rejected = expect(pending).rejects.toMatchObject({
      normalized: { code: 'operation.cancelled-by-destroy' }
    })

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await rejected
    expect(Number(fixture.backend.resourceCounters().connectionLeases)).toBeGreaterThan(0)
    expect(ownerConnection.connection.isCurrent()).toBe(true)

    gate.release()
    await settle(fixture.controller, gate.leaseCreated)
    await awaitSignal(closer.firstAttempt, 'the late borrowed connect cleanup to run')
    await flushMicrotasks()
    expect(closer.attempts()).toBe(1)
    expect(ownerConnection.connection.isCurrent()).toBe(true)

    await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(2)
    expect(ownerConnection.connection.isCurrent()).toBe(true)
    await expect(settle(fixture.controller, ownerConnection.discover(operation()))).resolves.toBeDefined()
    await settle(fixture.controller, owner.destroy())
  })

  test.each(acquisitions)(
    'expired deadline after a late $name lease does not adopt and remains retryable',
    async acquisition => {
      const closer = createCloser(acquisition.resourceKind, acquisition.operation, 'fail-then-succeed')
      const { manager, fixture } = await createOwningFixture()
      const gate = acquisition.install(fixture.backend, closer)
      const operationDeadline = deadline(Number(fixture.controller.clock.now()) + 5)
      const pending =
        acquisition.name === 'scan'
          ? manager.scan(scanOptions(null, operationDeadline))
          : manager.connect(peer(), operation(null, operationDeadline))
      const outcome = pending.then(
        value => ({ state: 'fulfilled', value }),
        error => ({ state: 'rejected', error })
      )

      fixture.controller.clock.advanceBy(10)
      gate.release()
      await settle(fixture.controller, gate.leaseCreated)
      const result = await settle(fixture.controller, outcome)
      expect(result.state).toBe('rejected')
      expect(result.error).toMatchObject({ normalized: { code: 'operation.timed-out' } })
      expect(Number(manager.localResourceCounters().connectionLeases)).toBe(0)
      expect(Number(manager.localResourceCounters().scanConsumers)).toBe(0)
      await awaitSignal(closer.firstAttempt, `the late ${acquisition.name} deadline cleanup to run`)
      await flushMicrotasks()
      expect(closer.attempts()).toBe(1)

      await expect(settle(fixture.controller, manager.destroy())).resolves.toMatchObject({ state: 'released' })
      expect(closer.attempts()).toBe(2)
    }
  )

  test('in-flight late connect release is shared with manager retry', async () => {
    const closer = createCloser('connection', 'connect-stale-admission-release', 'always-succeed')
    const releaseHold = closer.hold()
    const { manager, fixture } = await createOwningFixture()
    const gate = installConnectAcquisition(fixture.backend, closer)
    const pending = manager.connect(peer(), operation())
    const rejected = expect(pending).rejects.toMatchObject({
      normalized: { code: 'operation.cancelled-by-destroy' }
    })

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await rejected
    gate.release()
    await settle(fixture.controller, gate.leaseCreated)
    await awaitSignal(closer.firstAttempt, 'the late connect cleanup to start')
    expect(closer.attempts()).toBe(1)

    const retry = manager.destroy()
    await Promise.resolve()
    expect(closer.attempts()).toBe(1)
    releaseHold()
    await expect(settle(fixture.controller, retry)).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(1)
  })
})
