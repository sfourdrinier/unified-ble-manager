const { attachBleBackend, createBleManager, createManagerOwnershipAuthority } = require('../../src/manager/ble-manager')
const { DEFAULT_BLE_MANAGER_OPTIONS } = require('../../src/manager/ble-manager')
const { opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function releaseFailedRecord() {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind: 'adapter',
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation: 'adapter-states-close',
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

function createCloser(policy) {
  let attempts = 0
  let held = null
  return {
    attempts() {
      return attempts
    },
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
      if (held !== null) {
        await held.promise
      }
      if (policy === 'fail-then-succeed') {
        return attempts === 1 ? releaseFailedRecord() : releasedRecord()
      }
      if (policy === 'always-fail') {
        return releaseFailedRecord()
      }
      return releasedRecord()
    }
  }
}

function installWatchClose(backend, closer) {
  const originalWatchState = backend.adapter.watchState.bind(backend.adapter)
  backend.adapter.watchState = async () => {
    const watch = await originalWatchState()
    const originalClose = watch.transitions.close.bind(watch.transitions)
    watch.transitions.close = async () => {
      const result = await closer.close()
      if (result.state === 'released') {
        await originalClose()
      }
      return result
    }
    return watch
  }
}

async function createFixture(closer) {
  const fixture = createDeterministicTestBackend()
  installWatchClose(fixture.backend, closer)
  const attached = await attachBleBackend(fixture.backend, compatibility())
  const manager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('adapter-watch-client', 'client', 'deterministic:adapter-watch-client'),
      managerId: opaqueId('adapter-watch-manager', 'manager', 'deterministic:adapter-watch-manager'),
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

async function flush() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

describe('adapter-state watch cleanup ownership', () => {
  test('stop does not drop the watch before close succeeds', async () => {
    const closer = createCloser('fail-then-succeed')
    const { manager } = await createFixture(closer)
    const session = await manager.adapterStates()

    await expect(session.stop()).resolves.toMatchObject({ state: 'release-failed' })
    expect(closer.attempts()).toBe(1)

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(2)
  })

  test('destroy retries a watch whose close returned release-failed', async () => {
    const closer = createCloser('fail-then-succeed')
    const { manager } = await createFixture(closer)
    await manager.adapterStates()

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(closer.attempts()).toBe(1)

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(2)
  })

  test('manual stop, abort, and destroy share one in-flight close', async () => {
    const closer = createCloser('always-succeed')
    const releaseHold = closer.hold()
    const { manager } = await createFixture(closer)
    const controller = new AbortController()
    const session = await manager.adapterStates({ signal: controller.signal })

    const stopPromise = session.stop()
    await flush()
    controller.abort()
    const destroyPromise = manager.destroy()
    await flush()

    expect(closer.attempts()).toBe(1)
    releaseHold()

    await expect(stopPromise).resolves.toMatchObject({ state: 'released' })
    await expect(destroyPromise).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(1)
  })

  test('abort after successful stop is a no-op', async () => {
    const closer = createCloser('always-succeed')
    const { manager } = await createFixture(closer)
    const controller = new AbortController()
    const session = await manager.adapterStates({ signal: controller.signal })

    await expect(session.stop()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(1)

    controller.abort()
    await flush()
    expect(closer.attempts()).toBe(1)

    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(1)
  })

  test('abort listener is removed on success and on failure', async () => {
    const successCloser = createCloser('always-succeed')
    const successFixture = await createFixture(successCloser)
    const successController = new AbortController()
    const addSuccess = jest.spyOn(successController.signal, 'addEventListener')
    const removeSuccess = jest.spyOn(successController.signal, 'removeEventListener')
    const successSession = await successFixture.manager.adapterStates({ signal: successController.signal })
    await expect(successSession.stop()).resolves.toMatchObject({ state: 'released' })
    const addedSuccess = addSuccess.mock.calls.find(call => call[0] === 'abort')
    expect(addedSuccess).toBeDefined()
    expect(removeSuccess).toHaveBeenCalledWith('abort', addedSuccess[1])
    successController.abort()
    await flush()
    expect(successCloser.attempts()).toBe(1)
    await successFixture.manager.destroy()

    const failureCloser = createCloser('fail-then-succeed')
    const failureFixture = await createFixture(failureCloser)
    const failureController = new AbortController()
    const addFailure = jest.spyOn(failureController.signal, 'addEventListener')
    const removeFailure = jest.spyOn(failureController.signal, 'removeEventListener')
    const failureSession = await failureFixture.manager.adapterStates({ signal: failureController.signal })
    await expect(failureSession.stop()).resolves.toMatchObject({ state: 'release-failed' })
    const addedFailure = addFailure.mock.calls.find(call => call[0] === 'abort')
    expect(addedFailure).toBeDefined()
    expect(removeFailure).toHaveBeenCalledWith('abort', addedFailure[1])
    failureController.abort()
    await flush()
    expect(failureCloser.attempts()).toBe(1)
    await expect(failureFixture.manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(failureCloser.attempts()).toBe(2)
  })

  test('destroy aggregates remaining watch cleanup failures', async () => {
    const closer = createCloser('always-fail')
    const { manager } = await createFixture(closer)
    await manager.adapterStates()
    await manager.adapterStates()

    const result = await manager.destroy()
    expect(result.state).toBe('release-failed')
    expect(result.failures).toHaveLength(2)
    expect(result.failures.every(failure => failure.resourceKind === 'adapter')).toBe(true)
    expect(closer.attempts()).toBe(2)
  })
})

describe('pending adapter watch acquisition ownership', () => {
  test('abort settles before backend acquisition, and late failed cleanup remains retryable', async () => {
    const closer = createCloser('fail-then-succeed')
    const { manager, fixture } = await createFixture(closer)
    const original = fixture.backend.adapter.watchState
    let release
    const gate = new Promise(resolve => {
      release = resolve
    })
    fixture.backend.adapter.watchState = async () => {
      await gate
      return original()
    }
    const controller = new AbortController()
    const acquisition = manager.adapterStates({ signal: controller.signal })
    controller.abort()
    await expect(acquisition).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    release()
    await new Promise(resolve => setImmediate(resolve))
    expect(closer.attempts()).toBe(1)
    expect(manager.traces()).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ transition: 'adapter-states-late-stop', cause: 'platform.failure' })
      ])
    )
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(2)
  })

  test('destroy cancels a pending acquisition and refuses to report complete until it settles', async () => {
    const closer = createCloser('always-succeed')
    const { manager, fixture } = await createFixture(closer)
    const original = fixture.backend.adapter.watchState
    let release
    const gate = new Promise(resolve => {
      release = resolve
    })
    fixture.backend.adapter.watchState = async () => {
      await gate
      return original()
    }
    const acquisition = manager.adapterStates()
    const rejected = expect(acquisition).rejects.toMatchObject({
      normalized: { code: 'operation.cancelled-by-destroy' }
    })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await rejected
    release()
    await new Promise(resolve => setImmediate(resolve))
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(closer.attempts()).toBe(1)
  })
})

test('cancelled backend probes stay bounded until their late failures settle', async () => {
  const { manager, fixture } = await createFixture(createCloser('always-succeed'))
  const rejects = []
  fixture.backend.adapter.watchState = () => new Promise((_, reject) => rejects.push(reject))
  for (let index = 0; index < 64; index++) {
    const controller = new AbortController()
    const acquisition = manager.adapterStates({ signal: controller.signal })
    controller.abort()
    await expect(acquisition).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  }
  await expect(manager.adapterStates()).rejects.toMatchObject({ normalized: { code: 'stream.quota' } })
  expect(rejects).toHaveLength(64)
  for (const reject of rejects) reject(new Error('late availability failure'))
  await new Promise(resolve => setImmediate(resolve))
  expect(manager.traces()).toEqual(
    expect.arrayContaining([
      expect.objectContaining({ transition: 'adapter-states-acquisition', cause: 'platform.failure' })
    ])
  )
  await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
})
