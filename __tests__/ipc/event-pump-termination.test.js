const { IpcBleManager, inspectIpcPendingStreamAccountingForTests } = require('../../src/ipc/manager')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')
const { ElectronRendererBleClient } = require('../../src/electron/renderer')
const { IpcPublicManagerAdapter } = require('../../src/ipc/public-manager')
const { awaitSignal } = require('../helpers/async')

function negotiated(axis, value = axis === 'ipc-protocol' ? 4 : 1) {
  const selected = { axis, value }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function bootstrapRecord() {
  const backendGeneration = 'backend-generation-1'
  return {
    attachment: {
      attachmentId: 'attachment-1',
      backendInstanceId: 'backend-1',
      backendGeneration,
      adapter: {
        adapterId: 'adapter-1',
        displayName: 'Bluetooth',
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: null,
          backendGeneration,
          updatedAt: 1,
          safeReason: null
        },
        adapterGeneration: 'adapter-generation-1',
        limitations: []
      }
    },
    attachmentId: 'attachment-1',
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
        id,
        state: 'unsupported',
        selectedSchemaRange: negotiated('capability-schema').localRange,
        implementationOrigin: 'backend-native',
        tck: {
          suiteId: 'capability.catalog-v2',
          requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
          contractRange: negotiated('capability-schema').localRange
        },
        evidence: {
          receiptId: `fixture-${id}`,
          evidenceLevel: 'blocked',
          implementationVersion: 'fixture',
          sourceDigest: `fixture-${id}`,
          scenarioIds: ['capability.truth-limits-evidence-and-binding'],
          limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }]
        },
        limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }],
        limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
      }))
    },
    renderer: { clientId: 'client-1', windowScope: 'window', sessionScope: 'session' },
    rendererLease: { leaseId: 'lease-1', generation: 'lease-generation-1' }
  }
}

function permanentAckFailure() {
  return {
    kind: 'failure',
    error: {
      code: 'platform.transport',
      domain: 'ipc',
      operation: 'event.ack',
      platform: null,
      retryability: 'never'
    }
  }
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

async function flushPump() {
  for (let attempt = 0; attempt < 16; attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

async function createPumpHarness(options = {}) {
  const listeners = []
  const bootstrap = bootstrapRecord()
  let failAck = options.failAck === true
  let releaseAttempts = 0
  let scanStopAttempts = 0
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') {
        releaseAttempts += 1
        return { kind: 'release', cleanup: options.release?.(releaseAttempts) ?? { state: 'released', failures: [] } }
      }
      if (request.envelope?.command === 'scan.start') {
        return { kind: 'route', payload: options.scanStart ?? { handle: 'owned-scan' } }
      }
      if (request.envelope?.command === 'scan.stop') {
        scanStopAttempts += 1
        const stopPayload = await options.scanStop?.(scanStopAttempts)
        return {
          kind: 'route',
          payload: stopPayload ?? { state: 'released', failures: [] }
        }
      }
      return { kind: 'route', payload: { state: 'released', failureCount: 0 } }
    },
    subscribe(listener) {
      listeners.push(listener)
      return () => undefined
    },
    acknowledge: async () => (failAck ? permanentAckFailure() : { kind: 'event.ack' })
  }
  const ipc = await IpcBleManager.create(transport, options)
  return {
    ipc,
    bootstrap,
    releaseAttempts: () => releaseAttempts,
    scanStopAttempts: () => scanStopAttempts,
    failAck() {
      failAck = true
    },
    emit(streamId, item, eventId = `event-${streamId}`) {
      listeners[0]({
        rendererLease: bootstrap.rendererLease,
        eventId,
        streamId,
        item
      })
    },
    emitMalformed(eventId = 'malformed-event') {
      listeners[0]({
        rendererLease: bootstrap.rendererLease,
        eventId,
        streamId: 123,
        item: { kind: 'value', value: { seq: 1 } }
      })
    }
  }
}

async function killPump(harness) {
  harness.failAck()
  harness.emit('pump-kill', { kind: 'value', value: { seq: 0 } }, 'pump-kill-event')
  await flushPump()
  expectConsoleErrorMatching(
    '[ElectronRendererBleClient] Event acknowledgement failed permanently; terminating event delivery:',
    expect.objectContaining({
      error: expect.objectContaining({
        code: 'platform.transport',
        operation: 'event.ack',
        retryability: 'never'
      })
    })
  )
}

describe('IPC event pump termination', () => {
  test.each(['reject', 'release-failed', 'aggregate-reject'])(
    'missing required scan plan reports %s stop and retains retry debt',
    async stopMode => {
      const failure = {
        resourceKind: 'scan',
        error: {
          code: 'scan.stop-failed',
          domain: 'scan',
          operation: 'fixture.missing-plan-stop',
          platform: null,
          retryability: 'caller-decides'
        }
      }
      const harness = await createPumpHarness({
        scanStop: attempt => {
          if (attempt > 1) return { state: 'released', failures: [] }
          if (stopMode === 'reject') throw new Error('missing-plan-stop-rejected')
          if (stopMode === 'aggregate-reject') {
            throw new AggregateError([new Error('native-stop-a'), new Error('native-stop-b')], 'native stop failed')
          }
          return { state: 'release-failed', failures: [failure] }
        },
        release: attempt =>
          attempt === 1 ? { state: 'release-failed', failures: [failure] } : { state: 'released', failures: [] }
      })
      const manager = new IpcPublicManagerAdapter(harness.ipc, { requireScanPlan: true })
      const scanError = await manager.scan().then(
        value => value,
        error => error
      )
      expect(scanError).toMatchObject({ name: 'AggregateError' })
      if (stopMode === 'aggregate-reject') {
        expect(scanError.errors).toEqual(
          expect.arrayContaining([
            expect.objectContaining({
              normalized: expect.objectContaining({ operation: 'ipc-public-manager.scan-plan' })
            }),
            expect.objectContaining({ message: 'native stop failed' })
          ])
        )
      }
      expect(harness.scanStopAttempts()).toBe(1)
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      expect(harness.scanStopAttempts()).toBe(2)
      await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    }
  )
  test.each([
    ['released', 'reject'],
    ['release-failed', 'resolve'],
    ['reject', 'reject']
  ])('destroy reaches %s parent lease while child stop is unresolved and later %s', async (leaseState, lateMode) => {
    let resolveChild
    let rejectChild
    let noteRelease
    const releaseReached = new Promise(resolve => {
      noteRelease = resolve
    })
    const childStop = new Promise((resolve, reject) => {
      resolveChild = resolve
      rejectChild = reject
    })
    const harness = await createPumpHarness({
      scanStop: () => childStop,
      release: attempt => {
        noteRelease()
        if (attempt > 1 || leaseState === 'released') return { state: 'released', failures: [] }
        if (leaseState === 'reject') throw new Error('lease-release-rejected')
        return {
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'renderer-lease',
              error: {
                code: 'platform.transport',
                domain: 'ipc',
                operation: 'fixture.release',
                platform: null,
                retryability: 'caller-decides'
              }
            }
          ]
        }
      }
    })
    await harness.ipc.scan()
    harness.emit('owned-scan', { kind: 'terminal', reason: 'source-failed' }, 'blocked-child-stop')
    await flushPump()
    expect(harness.scanStopAttempts()).toBe(1)
    const result = harness.ipc.destroy().then(
      value => value,
      error => error
    )
    await expect(harness.ipc.adapterState()).rejects.toMatchObject({ normalized: { code: 'lifecycle.destroyed' } })
    await awaitSignal(releaseReached, 'parent lease release despite unresolved child stop')
    expect(harness.releaseAttempts()).toBeGreaterThan(0)
    const first = await result
    if (leaseState === 'released') expect(first).toEqual({ state: 'released', failures: [] })
    else expect(first.state === 'release-failed' || first instanceof Error).toBe(true)
    if (leaseState === 'reject') {
      expectConsoleErrorMatching(
        '[ElectronRendererBleClient] Release failed; client remains retryable:',
        expect.objectContaining({ message: 'lease-release-rejected' })
      )
    }
    if (lateMode === 'reject') rejectChild(new Error('late-child-stop-rejected'))
    else
      resolveChild({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'scan',
            error: {
              code: 'scan.stop-failed',
              domain: 'scan',
              operation: 'fixture.late-stop',
              platform: null,
              retryability: 'caller-decides'
            }
          }
        ]
      })
    await flushPump()
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })
  test('invalid scan plan retains a release-failed compensation receipt for destroy retry', async () => {
    const failure = {
      resourceKind: 'scan',
      error: {
        code: 'scan.stop-failed',
        domain: 'scan',
        operation: 'fixture.scan-plan-stop',
        platform: null,
        retryability: 'never'
      }
    }
    const harness = await createPumpHarness({
      scanStart: { handle: 'owned-scan', plan: {} },
      scanStop: attempt =>
        attempt === 1 ? { state: 'release-failed', failures: [failure] } : { state: 'released', failures: [] }
    })
    const rejection = await harness.ipc.scan({}).then(
      () => null,
      error => error
    )
    expect(rejection).toBeInstanceOf(AggregateError)
    expect(rejection.errors).toHaveLength(2)
    expect(rejection.errors[0]).toMatchObject({ normalized: { code: 'protocol.violation' } })
    expect(rejection.errors[1]).toMatchObject({ cleanup: { state: 'release-failed' } })
    expect(harness.scanStopAttempts()).toBe(1)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(harness.scanStopAttempts()).toBe(2)
  })

  test('failed automatic scan stop retries across refused lease release and settles after a successful retry', async () => {
    const failure = {
      resourceKind: 'scan',
      error: {
        code: 'scan.stop-failed',
        domain: 'scan',
        operation: 'fixture.scan-stop',
        platform: null,
        retryability: 'never'
      }
    }
    const harness = await createPumpHarness({
      scanStop: attempt =>
        attempt < 3 ? { state: 'release-failed', failures: [failure] } : { state: 'released', failures: [] },
      release: attempt =>
        attempt === 1 ? { state: 'release-failed', failures: [failure] } : { state: 'released', failures: [] }
    })
    await harness.ipc.scan({})
    harness.emit('owned-scan', { kind: 'terminal', reason: 'source-failed' }, 'owned-scan-terminal')
    await flushPump()
    expect(harness.scanStopAttempts()).toBe(1)
    await expect(harness.ipc.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([
        expect.objectContaining({ cleanup: { state: 'release-failed', failures: [failure] } })
      ])
    })
    expect(harness.scanStopAttempts()).toBe(2)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(harness.scanStopAttempts()).toBe(3)
  })

  test('automatic terminal and explicit stop share one in-flight scan stop', async () => {
    let finishStop
    const stopResult = new Promise(resolve => {
      finishStop = resolve
    })
    const harness = await createPumpHarness({ scanStop: () => stopResult })
    const scan = await harness.ipc.scan({})
    harness.emit('owned-scan', { kind: 'terminal', reason: 'source-failed' }, 'owned-scan-terminal')
    await flushPump()
    const explicit = scan.stop()
    expect(harness.scanStopAttempts()).toBe(1)
    finishStop({ state: 'released', failures: [] })
    await expect(explicit).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(harness.scanStopAttempts()).toBe(1)
  })

  test('thrown automatic scan-stop error remains visible when lease release is refused', async () => {
    const stopError = new Error('scan stop transport unavailable')
    const leaseFailure = {
      resourceKind: 'renderer-lease',
      error: {
        code: 'platform.transport',
        domain: 'ipc',
        operation: 'fixture.release',
        platform: null,
        retryability: 'never'
      }
    }
    const harness = await createPumpHarness({
      scanStop: () => {
        throw stopError
      },
      release: attempt =>
        attempt === 1 ? { state: 'release-failed', failures: [leaseFailure] } : { state: 'released', failures: [] }
    })
    await harness.ipc.scan({})
    harness.emit('owned-scan', { kind: 'terminal', reason: 'source-failed' }, 'thrown-stop-terminal')
    await flushPump()
    await expect(harness.ipc.destroy()).rejects.toMatchObject({ errors: expect.arrayContaining([stopError]) })
    expect(harness.scanStopAttempts()).toBe(2)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('malformed active child value retains the exact diagnostic and retries owner cleanup', async () => {
    const harness = await createPumpHarness()
    let ownerAttempts = 0
    const child = harness.ipc.registerStream('malformed-value-child', isRecord, undefined, undefined, () => {
      ownerAttempts += 1
      if (ownerAttempts === 1) throw new Error('owner cleanup refused')
    })
    harness.emit('malformed-value-child', { kind: 'value', value: 42 }, 'malformed-child-value')
    await flushPump()
    await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: {
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'protocol.malformed', operation: 'ipc-manager.stream-value' }
      }
    })
    expect(ownerAttempts).toBe(1)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(ownerAttempts).toBe(2)
  })

  test.each([
    ['item kind', { kind: 'unknown' }],
    ['overflow policy', { kind: 'overflow', policy: 'invalid', droppedItems: 1, droppedBytes: 1, replacedItems: 0 }],
    ['terminal reason', { kind: 'terminal', reason: 'invalid' }],
    ['terminal error', { kind: 'terminal', reason: 'source-failed', error: { code: 'invalid' } }]
  ])('malformed active child %s remains a structured child failure', async (_label, item) => {
    const harness = await createPumpHarness()
    const onTerminal = jest.fn()
    const child = harness.ipc.registerStream('malformed-control-child', isRecord, undefined, undefined, onTerminal)
    harness.emit('malformed-control-child', item, `malformed-control-${_label}`)
    await flushPump()
    await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error: { code: 'protocol.malformed' } }
    })
    expect(onTerminal).toHaveBeenCalledTimes(1)
    await harness.ipc.destroy()
  })

  test('inner source terminal retains its structured diagnostic for every child', async () => {
    const error = {
      code: 'platform.transport',
      domain: 'ipc',
      operation: 'fixture.inner-event-source',
      platform: null,
      retryability: 'never'
    }
    const source = {
      async *[Symbol.asyncIterator]() {
        yield {
          kind: 'terminal',
          reason: 'source-failed',
          droppedItems: 0,
          droppedBytes: 0,
          replacedItems: 0,
          error
        }
      }
    }
    const getter = jest.spyOn(ElectronRendererBleClient.prototype, 'events', 'get').mockReturnValue(source)
    try {
      const harness = await createPumpHarness()
      await flushPump()
      const child = harness.ipc.registerStream('late-child', isRecord)
      await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
        value: { kind: 'terminal', reason: 'source-failed', error }
      })
      await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      getter.mockRestore()
    }
  })

  test('bare inner iterator failure is a source-failed terminal and does not poison cleanup', async () => {
    const source = {
      async *[Symbol.asyncIterator]() {
        throw new Error('inner iterator refused')
      }
    }
    const getter = jest.spyOn(ElectronRendererBleClient.prototype, 'events', 'get').mockReturnValue(source)
    try {
      const harness = await createPumpHarness()
      await flushPump()
      const child = harness.ipc.registerStream('failed-iterator-child', isRecord)
      await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
        value: {
          kind: 'terminal',
          reason: 'source-failed',
          error: {
            code: 'platform.transport',
            platform: { code: 'iterator-failed', safeMessage: 'inner iterator refused' }
          }
        }
      })
      await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      getter.mockRestore()
    }
  })

  test('global terminal closes scan, notification, and lifecycle children', async () => {
    const harness = await createPumpHarness()
    const scan = harness.ipc.registerStream('scan-child', isRecord)
    const notification = harness.ipc.registerStream('notification-child', isRecord)
    const lifecycle = harness.ipc.registerStream('lifecycle-child', isRecord)
    const scanIterator = scan[Symbol.asyncIterator]()
    const notificationIterator = notification[Symbol.asyncIterator]()
    const lifecycleIterator = lifecycle[Symbol.asyncIterator]()
    await killPump(harness)
    await expect(scanIterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    await expect(notificationIterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    await expect(lifecycleIterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    expect(inspectIpcPendingStreamAccountingForTests(harness.ipc).activeStreamHandles).toEqual([])
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('bare inner stream completion is source-failed for all children', async () => {
    const source = { async *[Symbol.asyncIterator]() {} }
    const getter = jest.spyOn(ElectronRendererBleClient.prototype, 'events', 'get').mockReturnValue(source)
    try {
      const harness = await createPumpHarness()
      await flushPump()
      const child = harness.ipc.registerStream('natural-child', isRecord)
      await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
        value: { kind: 'terminal', reason: 'source-failed' }
      })
      await harness.ipc.destroy()
    } finally {
      getter.mockRestore()
    }
  })

  test('malformed global event terminates children and does not leave an unobserved rejection', async () => {
    const unhandled = []
    const onUnhandled = reason => {
      unhandled.push(reason)
    }
    process.on('unhandledRejection', onUnhandled)
    try {
      const harness = await createPumpHarness()
      const child = harness.ipc.registerStream('malformed-child', isRecord)
      const iterator = child[Symbol.asyncIterator]()
      harness.emitMalformed()
      await flushPump()
      await expect(iterator.next()).resolves.toMatchObject({
        value: { kind: 'terminal', reason: 'source-failed' }
      })
      expect(unhandled).toEqual([])
      await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(harness.ipc.adapterState()).rejects.toMatchObject({
        normalized: { code: 'lifecycle.destroyed' }
      })
    } finally {
      process.off('unhandledRejection', onUnhandled)
    }
  })

  test('pending pre-registration state is cleared and its terminal cause remains in the cleanup ledger', async () => {
    const harness = await createPumpHarness()
    harness.emit('pending-child', { kind: 'value', value: { seq: 1 } }, 'pending-event')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(harness.ipc).pendingIdCount).toBe(1)
    await killPump(harness)
    expect(inspectIpcPendingStreamAccountingForTests(harness.ipc)).toMatchObject({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0,
      activeStreamHandles: []
    })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('route after pump death fails closed', async () => {
    const harness = await createPumpHarness()
    await killPump(harness)
    await expect(harness.ipc.adapterState()).rejects.toMatchObject({
      normalized: { code: 'lifecycle.destroyed', operation: 'ipc-manager.released' }
    })
    await harness.ipc.destroy()
  })

  test('destroy after pump death is idempotent', async () => {
    const harness = await createPumpHarness()
    await killPump(harness)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('historical delivery failure does not poison a later successful native cleanup', async () => {
    const harness = await createPumpHarness({
      release: attempt =>
        attempt === 1
          ? {
              state: 'release-failed',
              failures: [
                {
                  resourceKind: 'renderer-lease',
                  error: {
                    code: 'platform.transport',
                    domain: 'ipc',
                    operation: 'event-pump-test.release',
                    platform: null,
                    retryability: 'never'
                  }
                }
              ]
            }
          : { state: 'released', failures: [] }
    })
    const child = harness.ipc.registerStream('failure-child', isRecord)
    harness.emitMalformed()
    await flushPump()
    await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error: { code: 'protocol.malformed' } }
    })
    await expect(harness.ipc.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(harness.releaseAttempts()).toBe(2)
  })

  test('inner bounded event queue overflow closes child streams without inventing per-stream drop counts', async () => {
    const harness = await createPumpHarness()
    const child = harness.ipc.registerStream('queue-child', isRecord, {
      itemCapacity: 512,
      byteCapacity: 1024 * 1024,
      reservedControlCapacity: 1
    })
    for (let index = 0; index < 140; index += 1) {
      harness.emit('queue-child', { kind: 'value', value: { index } }, `queue-${index}`)
    }
    await flushPump()
    await expect(child[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: {
        kind: 'terminal',
        reason: 'overflow',
        droppedItems: 0,
        droppedBytes: 0,
        error: {
          code: 'stream.overflow',
          platform: {
            metadata: {
              attribution: 'unknown',
              droppedItems: expect.any(Number),
              droppedBytes: expect.any(Number)
            }
          }
        }
      }
    })
    await expect(harness.ipc.adapterState()).rejects.toMatchObject({
      normalized: { code: 'lifecycle.destroyed' }
    })
    await harness.ipc.destroy()
  })

  test('all child owner cleanups are attempted and failures are aggregated', async () => {
    const harness = await createPumpHarness()
    const firstError = new Error('owner-cleanup-scan')
    const secondError = new Error('owner-cleanup-notification')
    const calls = []
    harness.ipc.registerStream('scan-child', isRecord, undefined, undefined, () => {
      calls.push('scan')
      throw firstError
    })
    harness.ipc.registerStream('notification-child', isRecord, undefined, undefined, () => {
      calls.push('notification')
      throw secondError
    })
    harness.ipc.registerStream('lifecycle-child', isRecord, undefined, undefined, () => {
      calls.push('lifecycle')
    })
    await killPump(harness)
    expect(calls.sort()).toEqual(['lifecycle', 'notification', 'scan'])
    await expect(harness.ipc.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([firstError, secondError])
    })
  })

  test('destroy retries only child cleanup phases that remain unresolved', async () => {
    const harness = await createPumpHarness()
    const persistentError = new Error('owner-cleanup-persistent')
    const attempts = { retryable: 0, persistent: 0 }
    harness.ipc.registerStream('retryable-child', isRecord, undefined, undefined, () => {
      attempts.retryable += 1
      if (attempts.retryable < 2) throw new Error('owner-cleanup-retryable')
    })
    harness.ipc.registerStream('persistent-child', isRecord, undefined, undefined, () => {
      attempts.persistent += 1
      throw persistentError
    })
    await killPump(harness)
    expect(attempts).toEqual({ retryable: 1, persistent: 1 })
    await expect(harness.ipc.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([persistentError])
    })
    expect(attempts).toEqual({ retryable: 2, persistent: 2 })
    await expect(harness.ipc.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([persistentError])
    })
    expect(attempts).toEqual({ retryable: 2, persistent: 3 })
  })
})
