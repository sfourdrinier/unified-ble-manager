const {
  IpcBleManager,
  inspectIpcPendingStreamAccountingForTests
} = require('../../src/ipc/manager')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

function negotiated(axis, value = axis === 'ipc-protocol' ? 2 : 1) {
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
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
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

  test('natural completion is source-failed for all children', async () => {
    const harness = await createPumpHarness()
    const child = harness.ipc.registerStream('natural-child', isRecord)
    const iterator = child[Symbol.asyncIterator]()
    await killPump(harness)
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    await harness.ipc.destroy()
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
      await expect(harness.ipc.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
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
      name: 'TypeError'
    })
    await harness.ipc.destroy()
  })

  test('destroy after pump death is idempotent', async () => {
    const harness = await createPumpHarness()
    await killPump(harness)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
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
