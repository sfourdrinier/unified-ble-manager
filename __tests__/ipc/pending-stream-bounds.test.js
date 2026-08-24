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

async function createIpcHarness(options = {}) {
  const listeners = []
  const bootstrap = bootstrapRecord()
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
    acknowledge: async () => ({ kind: 'event.ack' })
  }
  const ipc = await IpcBleManager.create(transport, options)
  return {
    ipc,
    bootstrap,
    emit(streamId, item, eventId = `event-${streamId}`) {
      listeners[0]({
        rendererLease: bootstrap.rendererLease,
        eventId,
        streamId,
        item
      })
    }
  }
}

async function flushPump() {
  for (let attempt = 0; attempt < 8; attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

async function firstStreamItem(ipc, streamId) {
  const stream = ipc.registerStream(streamId, isRecord)
  const iterator = stream[Symbol.asyncIterator]()
  const item = await iterator.next()
  await iterator.return()
  return item
}

describe('IPC pre-registration stream buffering', () => {
  test('delivers legitimate early events in order after registerStream', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('early-1', { kind: 'value', value: { seq: 1 } }, 'early-a')
    emit('early-1', { kind: 'value', value: { seq: 2 } }, 'early-b')
    await flushPump()
    const stream = ipc.registerStream('early-1', isRecord)
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 1 } } })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 2 } } })
    await iterator.return()
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting).toEqual({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0
    })
    await ipc.destroy()
  })

  test('unique unknown stream IDs remain globally bounded', async () => {
    const { ipc, emit } = await createIpcHarness()
    const count = 400
    for (let index = 0; index < count; index += 1) {
      emit(`unknown-${index}`, { kind: 'value', value: { index } }, `unknown-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.pendingIdCount).toBeLessThanOrEqual(256)
    expect(accounting.pendingItemCount).toBeLessThanOrEqual(512)
    expect(accounting.pendingByteCount).toBeLessThanOrEqual(2 * 1024 * 1024)
    expect(accounting.pendingIdCount + accounting.tombstoneCount).toBeGreaterThan(0)

    let retained = 0
    for (let index = 0; index < count; index += 1) {
      const item = await firstStreamItem(ipc, `unknown-${index}`)
      if (item.value?.kind === 'value' && item.value.value?.index === index) retained += 1
    }
    await ipc.destroy()
    expect(retained).toBeLessThanOrEqual(256)
  })

  test('aggregate eviction is fail-visible when the evicted ID later registers', async () => {
    const { ipc, emit } = await createIpcHarness()
    for (let index = 0; index < 257; index += 1) {
      emit(`quota-${index}`, { kind: 'value', value: { index } }, `quota-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const evicted = await firstStreamItem(ipc, 'quota-0')
    expect(evicted.value?.kind).toBe('terminal')
    expect(['overflow', 'source-failed']).toContain(evicted.value?.reason)
    const kept = await firstStreamItem(ipc, 'quota-256')
    await expect(kept).toMatchObject({ value: { kind: 'value', value: { index: 256 } } })
    await ipc.destroy()
  })

  test('terminal-only unknown IDs cannot accumulate forever', async () => {
    const { ipc, emit } = await createIpcHarness()
    const count = 400
    for (let index = 0; index < count; index += 1) {
      emit(`terminal-${index}`, { kind: 'terminal', reason: 'closed' }, `terminal-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.pendingIdCount).toBeLessThanOrEqual(256)
    let originalTerminals = 0
    for (let index = 0; index < count; index += 1) {
      const item = await firstStreamItem(ipc, `terminal-${index}`)
      if (item.value?.kind === 'terminal' && item.value.reason === 'closed') originalTerminals += 1
    }
    await ipc.destroy()
    expect(originalTerminals).toBeLessThanOrEqual(256)
  })

  test('pending unknown IDs expire after the injected age bound', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    emit('stale-1', { kind: 'value', value: { seq: 1 } }, 'stale-event')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(ipc).pendingIdCount).toBe(1)
    now = 5_001
    const item = await firstStreamItem(ipc, 'stale-1')
    expect(item.value?.kind).toBe('terminal')
    expect(['overflow', 'source-failed']).toContain(item.value?.reason)
    await ipc.destroy()
  })

  test('destroy returns pending accounting to zero', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('pending-1', { kind: 'value', value: { seq: 1 } }, 'destroy-a')
    emit('pending-2', { kind: 'value', value: { seq: 2 } }, 'destroy-b')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(ipc).pendingIdCount).toBe(2)
    await ipc.destroy()
    expect(inspectIpcPendingStreamAccountingForTests(ipc)).toEqual({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0
    })
  })
})
